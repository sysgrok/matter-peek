//! matter-peek — a Matter controller for diagnostics, in one executable.
//!
//! `matter-peek serve` runs the server: the controller, its API over
//! WebSocket and the web UI, on one port. Every other command goes through
//! the API: to a running server with `--server <ws-url>`, else to a controller
//! started in this process (which needs the store to itself, so not while a
//! server runs): everything the web UI does, from a terminal.
//! `matter-peek mcp` offers the API to AI agents, as MCP tools over stdio;
//! `matter-peek chat` asks the assistant, whose model `matter-peek assistant`
//! sets.
//!
//! ```text
//! matter-peek serve [--listen 127.0.0.1:17335]
//! matter-peek [--server ws://127.0.0.1:17335/ws [--token TOKEN]] [--json] <command> [args...]
//! matter-peek --help
//! ```

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use clap::builder::BoolishValueParser;
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::client::Connection;
use matter_peek_core::api::platform::data_dir;
use matter_peek_core::api::{self, display, query, wire};

mod chat;
mod client;
mod mcp;

/// Where `serve` listens by default: port 17335 is "PEEK" on a phone's keypad,
/// after a 1.
const DEFAULT_LISTEN: &str = "127.0.0.1:17335";

/// How much of an OTA image goes in one upload request.
const UPLOAD_CHUNK_LEN: usize = 256 * 1024;

#[derive(Parser)]
#[command(
    name = "matter-peek",
    version,
    about = "Commission, inspect and update Matter devices"
)]
struct Cli {
    /// Use the matter-peek server at this URL (e.g. ws://127.0.0.1:17335/ws),
    /// instead of a controller in this process.
    #[arg(long, global = true, value_name = "URL")]
    server: Option<String>,
    /// The access token of the server given with --server. By default, the one
    /// of a server whose store is here (in --store-dir, else in the user's data
    /// directory), if this user can read it.
    #[arg(long, global = true, env = "MATTER_PEEK_TOKEN", value_name = "TOKEN")]
    token: Option<String>,
    /// Print the API's responses and events as JSON.
    #[arg(long, global = true)]
    json: bool,
    /// Work in this fabric of ours: its name, its index (#2), or all. Else
    /// the default one.
    #[arg(long, global = true, env = "MATTER_PEEK_FABRIC", value_name = "FABRIC")]
    fabric: Option<String>,
    /// Keep the controller's store (its fabrics, devices and settings), the
    /// assistant's chats and the server's access token in this directory,
    /// instead of the user's data directory: for example a service's.
    #[arg(long, global = true, env = "MATTER_PEEK_STORE_DIR", value_name = "DIR")]
    store_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the devices on our fabric. Without --server, that is the remembered
    /// ones: the others are found by `sync`, which a server runs on its own.
    List {
        /// On all our fabrics.
        #[arg(long)]
        all: bool,
        /// Only those this filter matches, e.g. `net:thread is:online
        /// -eco:google`; `matter-peek filters` lists what it takes.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        filter: Vec<String>,
    },
    /// What `list` filters take: their syntax, facets and states.
    Filters,
    /// Sync the device list with the devices on our fabrics.
    Sync,
    /// Our fabrics: the "homes" devices are commissioned into.
    #[command(subcommand)]
    Fabric(FabricCmd),
    /// Find the commissionable devices on the local network.
    Discover {
        /// A discriminator or a setup code, to narrow to one device.
        filter: Option<String>,
        /// The most devices to list.
        #[arg(long, default_value_t = 16)]
        max: u8,
    },
    /// List the commissioned devices the network advertises, on our fabrics
    /// or others'. One on none of ours needs its controller to open a
    /// commissioning window for us.
    Advertised {
        /// Only those on no fabric of ours.
        #[arg(long)]
        others: bool,
    },
    /// Read the setup code from a picture of a device's QR code (PNG or JPEG).
    Qr { image: PathBuf },
    /// Commission a device onto our fabric, over IP (--ip) or Bluetooth (--ble).
    Commission {
        /// Its passcode, manual pairing code or MT: QR code payload.
        setup_code: String,
        /// Reach it over IP, at this address (e.g. "[fd00::1]:5540").
        #[arg(
            long,
            value_name = "ADDR",
            required_unless_present = "ble",
            conflicts_with = "ble"
        )]
        ip: Option<String>,
        /// Reach it over Bluetooth LE: the device matching the setup code, or the
        /// one at --ble-addr.
        #[arg(long)]
        ble: bool,
        /// The device's Bluetooth address (see `discover`).
        #[arg(long, value_name = "BT_ADDR", requires = "ble")]
        ble_addr: Option<String>,
        /// The Wi-Fi network the device joins (over Bluetooth).
        #[arg(long, value_name = "SSID", requires_all = ["ble", "wifi_password"])]
        wifi: Option<String>,
        /// The password of the --wifi network.
        #[arg(long, value_name = "PASSWORD", requires = "wifi")]
        wifi_password: Option<String>,
        /// The Thread network the device joins (over Bluetooth): its operational
        /// dataset, hex-encoded.
        #[arg(long, value_name = "HEX", requires = "ble", conflicts_with = "wifi")]
        thread_dataset: Option<String>,
        /// Also keep a record of the device, so it can be tagged.
        #[arg(long)]
        remember: bool,
    },
    /// Cancel the commission running on the server.
    Cancel,
    /// Keep a record of a listed device, so it can be tagged.
    Remember { node: Node },
    /// Drop the record of a remembered device, and its tags, keeping it on our
    /// fabric (that is `decommission`).
    Forget { node: Node },
    /// Replace the tags of a remembered device (none clears them).
    Tag {
        node: Node,
        /// The tags, as separate arguments or comma-separated.
        tags: Vec<String>,
    },
    /// Read the health of a device.
    Peek { node: Node },
    /// Read a device's data model: its endpoints and their device types, their
    /// clusters, the clusters' attributes (* writable) and commands.
    Model {
        node: Node,
        /// Only this endpoint.
        #[arg(long)]
        endpoint: Option<u16>,
        /// Only this cluster: its ID or name.
        #[arg(long)]
        cluster: Option<String>,
        /// Then follow it live, printing the attributes reported, until
        /// interrupted.
        #[arg(long)]
        live: bool,
    },
    /// Read the events a device keeps (presses, alarms, faults, boots, ...),
    /// oldest first.
    Events {
        node: Node,
        /// Only this endpoint.
        #[arg(long)]
        endpoint: Option<u16>,
        /// Only this cluster: its ID or name.
        #[arg(long)]
        cluster: Option<String>,
        /// Then follow them live, printing those reported, until interrupted.
        #[arg(long)]
        live: bool,
    },
    /// Write an attribute of a device: a JSON value (an enum by name, a
    /// bitmap as the names of its bits, a struct as an object).
    Write {
        node: Node,
        endpoint: u16,
        /// The cluster: its ID or name.
        cluster: String,
        /// The attribute: its ID or name.
        attribute: String,
        value: String,
    },
    /// Invoke a command of a device, with its fields as a JSON object.
    Invoke {
        node: Node,
        endpoint: u16,
        /// The cluster: its ID or name.
        cluster: String,
        /// The command: its ID or name.
        command: String,
        fields: Option<String>,
    },
    /// Read the networking of a device: its interfaces and addresses, the
    /// networks it knows, and the diagnostics of the network it is on; or
    /// change the networks it knows.
    Network {
        node: Node,
        /// The endpoint of the network interface; 0 is the primary one.
        #[arg(long, default_value_t = 0)]
        endpoint: u16,
        #[command(subcommand)]
        change: Option<NetworkChange>,
    },
    /// Follow the health of a device live, until interrupted.
    Watch { node: Node },
    /// Retrieve a diagnostic log of a device.
    Logs {
        node: Node,
        #[arg(value_enum, default_value_t = LogKind::Crash)]
        kind: LogKind,
        /// Write the log to this file, instead of printing it.
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    /// Open a commissioning window on a device, so that another controller
    /// (Google Home, Home Assistant, ...) can commission it too, and show the
    /// code to commission it with.
    Share {
        node: Node,
        /// How long the window stays open, 3 to 15 minutes.
        #[arg(long, default_value_t = 15)]
        minutes: u16,
        /// Close the window instead, whoever opened it.
        #[arg(long, conflicts_with = "status")]
        close: bool,
        /// Show the window instead: whether it is open, and its code.
        #[arg(long)]
        status: bool,
    },
    /// Show a device's clock; with `--set`, set it from ours: its UTC time,
    /// its time zone and daylight saving time, and us as its time source.
    Time {
        node: Node,
        /// Set it from ours.
        #[arg(long)]
        set: bool,
    },
    /// Make a device identify itself (e.g. blink).
    Identify {
        node: Node,
        #[arg(default_value_t = 3)]
        secs: u16,
    },
    /// Remove another controller's fabric from a device, by the device's index
    /// of it (as `peek` lists the fabrics): that controller loses the device.
    RemoveFabric { node: Node, index: u8 },
    /// Remove our fabric from a device and drop it from the list.
    Decommission {
        node: Node,
        /// Drop it from the list without contacting it.
        #[arg(long)]
        force: bool,
    },
    /// OTA images and device updates.
    #[command(subcommand)]
    Ota(Ota),
    /// The Check-Ins of a device that sleeps (an intermittently connected
    /// device).
    #[command(subcommand)]
    Icd(Icd),
    /// The Thread mesh as our Thread devices see it: networks, nodes, links
    /// and what is wrong. Reads each Thread device in turn.
    Thread {
        #[command(subcommand)]
        view: Option<ThreadView>,
    },
    /// Write the store to FILE: our fabrics and their keys, the devices, the
    /// assistant's model with its API key, and its chats. Encrypted with a
    /// passphrase, which it asks for. The store stays: use one of the two.
    Export {
        file: PathBuf,
        /// Do not encrypt it: anyone with the file can control the devices.
        #[arg(long, conflicts_with = "passphrase_stdin")]
        plain: bool,
        /// Read the passphrase from the first line of stdin.
        #[arg(long)]
        passphrase_stdin: bool,
    },
    /// Replace the store with the one `export` wrote to FILE, asking for its
    /// passphrase if it is encrypted. Only while there are no devices on our
    /// fabrics. A server restarts with it; without --server, it is used from
    /// the next start.
    Import {
        file: PathBuf,
        /// Read the passphrase from the first line of stdin.
        #[arg(long)]
        passphrase_stdin: bool,
    },
    /// Run the server: the controller, its API over WebSocket (at /ws) and the
    /// web UI (at /), on one port, until stopped. The assistant's model is
    /// the stored one, unless MATTER_PEEK_LLM_URL, MATTER_PEEK_LLM_MODEL and
    /// MATTER_PEEK_LLM_KEY set one.
    Serve {
        /// The address to listen on: 0.0.0.0:17335 for the whole network.
        /// Clients give the access token, which the log shows when it starts,
        /// and `matter-peek token` prints.
        #[arg(long, default_value = DEFAULT_LISTEN, value_name = "ADDR")]
        listen: String,
    },
    /// Print the access token of the server whose store is here (in
    /// --store-dir, else in the user's data directory), making one if there is
    /// none yet.
    Token {
        /// Make a new one: the old one stops working when the server starts
        /// again.
        #[arg(long)]
        new: bool,
    },
    /// Offer the API to an AI agent, as MCP tools over stdio (the MCP stdio
    /// transport), until stdin closes. Without --server, holds the store while
    /// it runs.
    Mcp {
        /// Also offer the tools that change devices or the stored state.
        #[arg(long)]
        allow_mutate: bool,
    },
    /// Ask the assistant about the devices, in a chat: its model looks into
    /// them with the API's tools. The chat is kept, as the web UI's. Without
    /// --server, holds the store while it runs.
    Chat {
        /// Ask this, answer, and end (else chat until /quit or end of input).
        prompt: Option<String>,
        /// Show the model's thinking.
        #[arg(long)]
        show_thinking: bool,
        /// Show the tools' results, and the model's timings and tokens.
        #[arg(long)]
        verbose: bool,
        /// Also offer the tools that change devices or the stored state; each
        /// call is asked for first.
        #[arg(long)]
        allow_mutate: bool,
    },
    /// The assistant's model: any behind an OpenAI-compatible API, e.g. a
    /// local Ollama. Kept in the store; its API key never leaves the side
    /// that runs the assistant. Without a subcommand, shows it.
    Assistant {
        #[command(subcommand)]
        action: Option<AssistantAction>,
    },
}

/// What to do with the assistant's model.
#[derive(Subcommand)]
enum AssistantAction {
    /// Show the assistant's model.
    Show,
    /// Set the assistant's model; what is not given stays. An API key stays
    /// only while the API stays at the same address.
    Set {
        /// The model's OpenAI-compatible API, e.g. http://localhost:11434/v1.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        /// The model, as the API names it, e.g. qwen3.5:9b (see `assistant
        /// models`).
        #[arg(long)]
        model: Option<String>,
        #[command(flatten)]
        key: KeyArgs,
        /// Forget the API key.
        #[arg(long, conflicts_with_all = ["api_key", "api_key_stdin"])]
        no_api_key: bool,
        /// Whether the model thinks before it answers: slower, more careful.
        #[arg(long, value_name = "on|off", value_parser = BoolishValueParser::new())]
        think: Option<bool>,
    },
    /// List the models the assistant's API offers, or another API's.
    Models {
        /// Another API than the assistant's.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        #[command(flatten)]
        key: KeyArgs,
    },
    /// Ask the model to say hello, to see that it answers.
    Test,
    /// Forget the assistant's model, and its key.
    Clear,
}

/// Where an API key comes from: never an argument, which shell histories and
/// process lists show.
#[derive(Args)]
struct KeyArgs {
    /// Ask for the API key (typed without echo).
    #[arg(long, conflicts_with = "api_key_stdin")]
    api_key: bool,
    /// Read the API key from the first line of stdin.
    #[arg(long)]
    api_key_stdin: bool,
}

impl KeyArgs {
    /// The key, as asked for; `Keep` if not.
    fn read(&self) -> io::Result<api::KeyChange> {
        let key = if self.api_key {
            rpassword::prompt_password("API key: ")?
        } else if self.api_key_stdin {
            let mut line = String::new();
            io::stdin().read_line(&mut line)?;
            line
        } else {
            return Ok(api::KeyChange::Keep);
        };

        match key.trim() {
            "" => Err(io::Error::other("no API key given")),
            key => Ok(api::KeyChange::Set { key: key.into() }),
        }
    }
}

/// A change to the networks a device knows, each committed under a fail-safe.
/// A network is named as `network` shows it: an SSID; a Thread network by its
/// name or its extended PAN ID.
#[derive(Subcommand)]
enum NetworkChange {
    /// Give the device a network, or new credentials for one it knows. New
    /// credentials for the connected network are proven by reconnecting.
    Add {
        /// A Wi-Fi network, by SSID.
        #[arg(long, value_name = "SSID", requires = "password")]
        wifi: Option<String>,
        /// The password of the --wifi network.
        #[arg(long, requires = "wifi")]
        password: Option<String>,
        /// A Thread network: its operational dataset, hex-encoded.
        #[arg(
            long,
            value_name = "HEX",
            conflicts_with = "wifi",
            required_unless_present = "wifi"
        )]
        thread_dataset: Option<String>,
    },
    /// Make the device forget a network; not the connected one.
    Remove { network: String },
    /// Connect the device to a network it knows. Committed once the device
    /// answers on it; otherwise the device returns to its previous network.
    Connect { network: String },
    /// Move a network to another place in the order the device tries them.
    Reorder {
        network: String,
        /// Its new place, from 0.
        index: u8,
    },
}

/// What of the Thread mesh to show.
#[derive(Subcommand)]
enum ThreadView {
    /// Networks, nodes, links and findings.
    Mesh,
    /// Only what is wrong, and the networks.
    Findings,
}

#[derive(Subcommand)]
enum FabricCmd {
    /// List our fabrics.
    List,
    /// Create a fabric, with a root CA of its own.
    Create { name: String },
    /// Rename a fabric (by name or #index).
    Rename { fabric: String, name: String },
    /// Make a fabric the default: what a command without --fabric works in.
    Default { fabric: String },
    /// Delete a fabric; not the default. Refused while it has devices, unless
    /// --decommission or --forget.
    Delete {
        fabric: String,
        /// Decommission its devices first.
        #[arg(long, conflicts_with = "forget")]
        decommission: bool,
        /// Forget its devices: they keep the dead fabric until another
        /// controller removes it.
        #[arg(long)]
        forget: bool,
    },
}

#[derive(Subcommand)]
enum Ota {
    /// List the stored OTA images.
    Images,
    /// List the OTA updates devices asked us for in the last hour, and how far
    /// they got.
    Updates,
    /// Store an OTA image file.
    Upload { file: PathBuf },
    /// Delete a stored OTA image.
    Delete { name: String },
    /// Update a device to a stored image, and follow the update.
    Update { node: Node, name: String },
}

#[derive(Subcommand)]
enum Icd {
    /// Register for its Check-Ins: a device that can idle long then sleeps
    /// long, and checks in with us when it wakes. A commission does this by
    /// itself.
    Register { node: Node },
    /// Drop our registration for its Check-Ins: with no other client, it idles
    /// short again, reachable within seconds.
    Unregister { node: Node },
    /// Keep it awake for a while, so that it answers: if it sleeps long and
    /// sleeps now, wait for it to wake up (or wake it, as its hint says).
    Wake { node: Node },
    /// Keep it awake until interrupted (10 min at most): once it wakes up, ask
    /// it again and again to stay awake.
    StayAwake { node: Node },
}

#[derive(Clone, Copy, ValueEnum)]
enum LogKind {
    Support,
    Network,
    Crash,
}

impl From<LogKind> for api::LogIntent {
    fn from(kind: LogKind) -> Self {
        match kind {
            LogKind::Support => Self::EndUserSupport,
            LogKind::Network => Self::NetworkDiag,
            LogKind::Crash => Self::CrashLogs,
        }
    }
}

/// A device's node ID, in hex (`0x1b66f`) or decimal.
#[derive(Clone, Copy)]
struct Node(u64);

impl std::str::FromStr for Node {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let node_id = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            Some(hex) => u64::from_str_radix(hex, 16),
            None => s.parse(),
        };

        node_id.map(Self).map_err(|_| format!("not a node ID: {s}"))
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // Logs go to stderr: stdout is the output, or the MCP stream; a chat
    // keeps to its conversation.
    let level = match cli.command {
        Command::Serve { .. } | Command::Mcp { .. } => "info",
        Command::Chat { .. } => "error",
        _ => "warn",
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level)).init();

    if let Command::Serve { listen } = &cli.command {
        return serve(listen, cli.store_dir.as_deref());
    }
    if let Command::Token { new } = &cli.command {
        return token(cli.store_dir.as_deref(), *new);
    }
    if let Command::Filters = &cli.command {
        print_filters();
        return ExitCode::SUCCESS;
    }

    match matter_peek_server::block_on(run(cli)) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Run the server on `listen`, with its store in `store_dir` if given, until
/// stopped.
fn serve(listen: &str, store_dir: Option<&Path>) -> ExitCode {
    let local = listen
        .parse::<std::net::SocketAddr>()
        .is_ok_and(|addr| addr.ip().is_loopback());
    if !local {
        log::warn!(
            "serve: listening on {listen}, beyond this machine, without TLS: the access token \
             and the rest travel unencrypted"
        );
    }

    match matter_peek_server::run(listen, matter_peek_server::llm::model_from_env(), store_dir) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Print the access token kept in `store_dir`, else in the user's data
/// directory; a new one if `renew`.
fn token(store_dir: Option<&Path>, renew: bool) -> ExitCode {
    let dir = store_dir.map_or_else(data_dir, Path::to_path_buf);

    match matter_peek_server::access_token(&dir, renew) {
        Ok(token) => {
            println!("{token}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: the access token in {}: {e}", dir.display());
            ExitCode::FAILURE
        }
    }
}

/// Where the API is, as the command line says: a server, with its token given
/// or kept here, else a controller in this process.
fn endpoint(cli: &Cli) -> client::Endpoint {
    let token = cli.token.clone().or_else(|| {
        let dir = cli.store_dir.clone().unwrap_or_else(data_dir);
        cli.server.as_ref()?;
        matter_peek_server::stored_access_token(&dir).ok().flatten()
    });

    client::Endpoint {
        server: cli.server.clone(),
        token,
        store_dir: cli.store_dir.clone(),
    }
}

/// Run the command; `Ok(false)` when the API answered with an error.
async fn run(cli: Cli) -> io::Result<bool> {
    let fabric = cli
        .fabric
        .as_deref()
        .map(api::FabricRef::parse)
        .unwrap_or_default();
    let endpoint = endpoint(&cli);

    // Its own connection, for as long as stdin is open.
    match cli.command {
        Command::Mcp { allow_mutate } => {
            mcp::serve(endpoint, fabric, allow_mutate).await?;
            return Ok(true);
        }
        Command::Chat {
            prompt,
            show_thinking,
            verbose,
            allow_mutate,
        } => {
            let args = chat::ChatArgs {
                prompt,
                show_thinking,
                verbose,
                allow_mutate,
                fabric: fabric.clone(),
            };
            return chat::chat(endpoint, args).await;
        }
        _ => {}
    }

    let conn = match &endpoint.server {
        Some(url) => client::connect(url, endpoint.token.as_deref()).await?,
        // Short-lived: no periodic sync, which the process could exit in the
        // middle of.
        None => client::connect_in_process(endpoint.in_process(false))?,
    };

    let out = Out { json: cli.json };

    match cli.command {
        Command::List { all, filter } => {
            let fabric = if all { api::FabricRef::All } else { fabric };
            let filter = (!filter.is_empty()).then(|| filter_text(&filter));
            let filtered = filter.is_some();
            let request = wire::Request::ListDevices { fabric, filter };

            match call(&conn, &out, request).await? {
                Some(wire::Response::Devices { devices, .. })
                    if filtered && devices.is_empty() && !out.json =>
                {
                    println!("No device matches the filter.");
                    Ok(true)
                }
                response => devices(&out, response),
            }
        }
        // Answered before connecting.
        Command::Filters => Ok(true),
        Command::Sync => devices(
            &out,
            call(&conn, &out, wire::Request::Sync { fabric }).await?,
        ),
        Command::Fabric(cmd) => {
            let request = match cmd {
                FabricCmd::List => wire::Request::Fabrics,
                FabricCmd::Create { name } => wire::Request::CreateFabric { name },
                FabricCmd::Rename { fabric, name } => wire::Request::RenameFabric {
                    fabric: api::FabricRef::parse(&fabric),
                    name,
                },
                FabricCmd::Default { fabric } => wire::Request::SetDefaultFabric {
                    fabric: api::FabricRef::parse(&fabric),
                },
                FabricCmd::Delete {
                    fabric,
                    decommission,
                    forget,
                } => wire::Request::DeleteFabric {
                    fabric: api::FabricRef::parse(&fabric),
                    deletion: if decommission {
                        api::FabricDeletion::Decommission
                    } else if forget {
                        api::FabricDeletion::Forget
                    } else {
                        api::FabricDeletion::Refuse
                    },
                },
            };

            fabrics(&out, call(&conn, &out, request).await?)
        }
        Command::Discover { filter, max } => {
            let (discriminator, setup_code) = discover_filter(filter);
            let request = wire::Request::Discover {
                discriminator,
                setup_code,
                max,
            };

            Ok(match call(&conn, &out, request).await? {
                Some(wire::Response::Discovered {
                    devices,
                    ble_devices,
                }) if !out.json => {
                    if devices.is_empty() && ble_devices.is_empty() {
                        println!("No commissionable devices found.");
                    }
                    for d in devices {
                        println!(
                            "{}  commissionable id {}",
                            d.addr,
                            display::hex_u64(d.commissionable_id)
                        );
                    }
                    for d in ble_devices {
                        println!(
                            "Bluetooth {}  discriminator {}  vendor {}  product {}",
                            d.addr,
                            d.discriminator,
                            display::vendor_label(d.vendor_id, None),
                            display::product_label(d.vendor_id, d.product_id, None)
                        );
                    }
                    true
                }
                response => response.is_some(),
            })
        }
        Command::Advertised { others } => {
            Ok(
                match call(&conn, &out, wire::Request::AdvertisedDevices).await? {
                    Some(wire::Response::AdvertisedDevices { mut devices }) if !out.json => {
                        devices.retain(|d| !others || !d.ours());
                        // Those to share with us first.
                        devices.sort_by_key(api::AdvertisedDevice::ours);
                        if devices.is_empty() {
                            println!("No commissioned devices found.");
                        }
                        for d in devices {
                            print_advertised(&d);
                        }
                        true
                    }
                    response => response.is_some(),
                },
            )
        }
        Command::Qr { image } => {
            let request = wire::Request::ReadQrImage {
                image: BASE64.encode(std::fs::read(image)?),
            };

            Ok(match call(&conn, &out, request).await? {
                Some(wire::Response::SetupCode { setup_code }) if !out.json => {
                    println!("{setup_code}");
                    true
                }
                response => response.is_some(),
            })
        }
        Command::Commission {
            setup_code,
            ip,
            ble: _,
            ble_addr,
            wifi,
            wifi_password,
            thread_dataset,
            remember,
        } => {
            let via = match ip {
                Some(addr) => api::CommissionVia::Ip { addr },
                None => api::CommissionVia::Ble {
                    addr: ble_addr,
                    network: match (wifi, thread_dataset) {
                        (Some(ssid), _) => Some(api::NetworkCredentials::Wifi {
                            ssid,
                            password: wifi_password.unwrap_or_default(),
                        }),
                        (None, Some(dataset)) => Some(api::NetworkCredentials::Thread { dataset }),
                        (None, None) => None,
                    },
                },
            };
            let request = wire::Request::Commission {
                via,
                setup_code,
                remember,
                fabric,
            };

            Ok(match call(&conn, &out, request).await? {
                Some(wire::Response::Commissioned(result)) if !out.json => {
                    println!(
                        "Commissioned as node {}.",
                        display::hex_u64(result.handle.node_id)
                    );
                    if let Some(node_id) = result.rematch_candidate {
                        println!(
                            "Note: the same model as device {}; maybe the same unit.",
                            display::hex_u64(node_id)
                        );
                    }
                    true
                }
                response => response.is_some(),
            })
        }
        Command::Cancel => ok(
            &out,
            call(&conn, &out, wire::Request::Cancel).await?,
            "Cancel requested.",
        ),
        Command::Remember { node } => devices(
            &out,
            call(&conn, &out, wire::Request::Remember { node_id: node.0 }).await?,
        ),
        Command::Forget { node } => devices(
            &out,
            call(&conn, &out, wire::Request::Forget { node_id: node.0 }).await?,
        ),
        Command::Tag { node, tags } => {
            let tags = tags
                .iter()
                .flat_map(|t| t.split(','))
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
            let request = wire::Request::SetTags {
                node_id: node.0,
                tags,
            };

            devices(&out, call(&conn, &out, request).await?)
        }
        Command::Peek { node } => Ok(
            match call(&conn, &out, wire::Request::PeekDevice { node_id: node.0 }).await? {
                Some(wire::Response::Peek { health, .. }) if !out.json => {
                    println!("{}", health_text(&health));
                    true
                }
                response => response.is_some(),
            },
        ),
        Command::Model {
            node,
            endpoint,
            cluster,
            live,
        } => {
            let request = wire::Request::ReadModel {
                node_id: node.0,
                endpoint,
                cluster: cluster.clone(),
            };
            let read = match call(&conn, &out, request).await? {
                Some(wire::Response::Model { model }) if !out.json => {
                    print_model(&model);
                    true
                }
                response => response.is_some(),
            };
            if !(read && live) {
                return Ok(read);
            }

            // The application endpoints cover less, if they cover what is asked.
            let scope = match endpoint {
                Some(endpoint) if endpoint != 0 => api::ModelScope::Application,
                _ => api::ModelScope::All,
            };
            let request = wire::Request::SubscribeModel {
                node_id: node.0,
                scope,
            };
            if call(&conn, &out, request).await?.is_none() {
                return Ok(false);
            }
            // Only what was asked for.
            let wanted_cluster = cluster.as_deref().map(str::to_lowercase);
            while let Some(msg) = conn.recv().await {
                let wire::ServerMsg::Event { mut event } = msg else {
                    continue;
                };
                if let wire::Event::Model { model, .. } = &mut event {
                    model
                        .endpoints
                        .retain(|e| endpoint.is_none_or(|wanted| wanted == e.id));
                    for e in &mut model.endpoints {
                        e.clusters.retain(|c| {
                            wanted_cluster.as_deref().is_none_or(|wanted| {
                                c.name.as_deref().map(str::to_lowercase).as_deref() == Some(wanted)
                                    || wanted
                                        .trim_start_matches("0x")
                                        .parse::<u32>()
                                        .is_ok_and(|id| id == c.id)
                            })
                        });
                    }
                }
                out.event(&event);
            }

            Ok(true)
        }
        Command::Events {
            node,
            endpoint,
            cluster,
            live,
        } => {
            let request = wire::Request::ReadEvents {
                node_id: node.0,
                endpoint,
                cluster: cluster.clone(),
            };
            // The number of the first event not read.
            let mut seen = 0;
            let read = match call(&conn, &out, request).await? {
                Some(wire::Response::Events { events }) => {
                    seen = events.last().map_or(0, |e| e.number + 1);
                    if !out.json {
                        if events.is_empty() {
                            println!("No events.");
                        }
                        for e in &events {
                            println!("{}", event_text(e));
                        }
                    }
                    true
                }
                response => response.is_some(),
            };
            if !(read && live) {
                return Ok(read);
            }

            let scope = match endpoint {
                Some(endpoint) if endpoint != 0 => api::ModelScope::Application,
                _ => api::ModelScope::All,
            };
            let request = wire::Request::SubscribeModel {
                node_id: node.0,
                scope,
            };
            if call(&conn, &out, request).await?.is_none() {
                return Ok(false);
            }
            // Only the events asked for, after those read.
            let wanted_cluster = cluster.as_deref().map(str::to_lowercase);
            while let Some(msg) = conn.recv().await {
                let wire::ServerMsg::Event { mut event } = msg else {
                    continue;
                };
                if let wire::Event::Model { model, .. } = &mut event {
                    model.endpoints.clear();
                    model.events.retain(|e| {
                        e.number >= seen
                            && endpoint.is_none_or(|wanted| wanted == e.endpoint)
                            && wanted_cluster.as_deref().is_none_or(|wanted| {
                                e.cluster_name.as_deref().map(str::to_lowercase).as_deref()
                                    == Some(wanted)
                                    || wanted
                                        .trim_start_matches("0x")
                                        .parse::<u32>()
                                        .is_ok_and(|id| id == e.cluster)
                            })
                    });
                    if let Some(last) = model.events.last() {
                        seen = last.number + 1;
                    }
                    if model.events.is_empty() {
                        continue;
                    }
                }
                out.event(&event);
            }

            Ok(true)
        }
        Command::Write {
            node,
            endpoint,
            cluster,
            attribute,
            value,
        } => {
            let request = wire::Request::WriteAttribute {
                node_id: node.0,
                endpoint,
                cluster,
                attribute: attribute.clone(),
                value,
            };
            Ok(match call(&conn, &out, request).await? {
                Some(wire::Response::Model { model }) if !out.json => {
                    let written = model
                        .endpoints
                        .iter()
                        .flat_map(|e| &e.clusters)
                        .flat_map(|c| &c.attributes)
                        .find(|a| {
                            a.name
                                .as_deref()
                                .is_some_and(|name| name.eq_ignore_ascii_case(&attribute))
                                || attribute.parse::<u32>().is_ok_and(|id| id == a.id)
                        });
                    match written.and_then(|a| a.value.as_deref()) {
                        Some(value) => println!("Written; it reads {value}"),
                        None => println!("Written."),
                    }
                    true
                }
                response => response.is_some(),
            })
        }
        Command::Invoke {
            node,
            endpoint,
            cluster,
            command,
            fields,
        } => {
            let request = wire::Request::InvokeCommand {
                node_id: node.0,
                endpoint,
                cluster,
                command,
                fields: fields.unwrap_or_default(),
            };
            Ok(match call(&conn, &out, request).await? {
                Some(wire::Response::CommandOutcome { outcome }) if !out.json => {
                    match (outcome.response, outcome.fields) {
                        (Some(response), Some(fields)) => println!("{response}: {fields}"),
                        _ => println!("Done."),
                    }
                    true
                }
                response => response.is_some(),
            })
        }
        Command::Network {
            node,
            endpoint,
            change,
        } => {
            let request = match change {
                None => wire::Request::ReadNetwork { node_id: node.0 },
                Some(change) => {
                    let Some(change) = network_change(&conn, node.0, endpoint, change).await?
                    else {
                        return Ok(false);
                    };
                    if !out.json {
                        println!("Changing the networks of the device…");
                    }
                    wire::Request::ChangeNetwork {
                        node_id: node.0,
                        endpoint,
                        change,
                    }
                }
            };

            Ok(match call(&conn, &out, request).await? {
                Some(wire::Response::Network { network, .. }) if !out.json => {
                    print!("{}", network_text(&network));
                    true
                }
                response => response.is_some(),
            })
        }
        Command::Watch { node } => watch(&conn, &out, node.0).await,
        Command::Logs { node, kind, output } => {
            let request = wire::Request::RetrieveLogs {
                node_id: node.0,
                intent: kind.into(),
            };

            match call(&conn, &out, request).await? {
                Some(wire::Response::Logs { intent, data, .. }) if !out.json => {
                    log(intent, &data, output)
                }
                response => Ok(response.is_some()),
            }
        }
        Command::Identify { node, secs } => {
            let request = wire::Request::Identify {
                node_id: node.0,
                secs,
            };

            ok(
                &out,
                call(&conn, &out, request).await?,
                &format!("Identifying for {secs} s."),
            )
        }
        Command::Share {
            node,
            minutes,
            close,
            status,
        } => {
            let node_id = node.0;
            let request = if close {
                wire::Request::CloseCommissioningWindow { node_id }
            } else if status {
                wire::Request::CommissioningWindow { node_id }
            } else {
                wire::Request::OpenCommissioningWindow {
                    node_id,
                    timeout_secs: Some(minutes.saturating_mul(60)),
                }
            };

            window(&out, call(&conn, &out, request).await?)
        }
        Command::Time { node, set } => {
            let node_id = node.0;
            let request = if set {
                wire::Request::SetDeviceTime { node_id }
            } else {
                wire::Request::DeviceTime { node_id }
            };

            device_time(&out, call(&conn, &out, request).await?)
        }
        Command::RemoveFabric { node, index } => {
            let request = wire::Request::RemoveDeviceFabric {
                node_id: node.0,
                fabric_index: index,
            };

            match call(&conn, &out, request).await? {
                Some(wire::Response::Peek { health, .. }) => {
                    if !out.json {
                        println!("Removed fabric #{index}.");
                        if let Some(fabrics) = &health.fabrics {
                            println!("{}", fabrics_text(fabrics, health.supported_fabrics));
                        }
                    }
                    Ok(true)
                }
                other => Ok(other.is_some()),
            }
        }
        Command::Decommission { node, force } => {
            let request = wire::Request::Decommission {
                node_id: node.0,
                force,
            };

            devices(&out, call(&conn, &out, request).await?)
        }
        Command::Ota(Ota::Images) => {
            images(&out, call(&conn, &out, wire::Request::OtaImages).await?)
        }
        Command::Ota(Ota::Updates) => {
            ota_updates(&out, call(&conn, &out, wire::Request::OtaUpdates).await?)
        }
        Command::Ota(Ota::Upload { file }) => upload(&conn, &out, &file).await,
        Command::Export {
            file,
            plain,
            passphrase_stdin,
        } => {
            let passphrase = new_passphrase(plain, passphrase_stdin)?;
            export(&conn, &out, &file, passphrase).await
        }
        Command::Import {
            file,
            passphrase_stdin,
        } => {
            let data = std::fs::read(&file)?;
            let passphrase = if !matter_peek_server::is_sealed(&data) {
                None
            } else if passphrase_stdin {
                Some(read_line()?)
            } else {
                Some(rpassword::prompt_password("Passphrase: ")?)
            };
            let request = wire::Request::ImportStore {
                data: BASE64.encode(data),
                passphrase,
            };

            Ok(match call(&conn, &out, request).await? {
                Some(wire::Response::Imported { restarts }) if !out.json => {
                    if restarts {
                        println!("Imported: the server restarts with it.");
                    } else {
                        println!("Imported: matter-peek uses it from its next start.");
                    }
                    true
                }
                response => response.is_some(),
            })
        }
        Command::Ota(Ota::Delete { name }) => images(
            &out,
            call(&conn, &out, wire::Request::OtaDelete { name }).await?,
        ),
        Command::Ota(Ota::Update { node, name }) => update(&conn, &out, node.0, name).await,
        Command::Icd(Icd::Register { node }) => ok(
            &out,
            call(
                &conn,
                &out,
                wire::Request::RegisterCheckIns { node_id: node.0 },
            )
            .await?,
            "Registered for its Check-Ins: it idles long now.",
        ),
        Command::Icd(Icd::Unregister { node }) => ok(
            &out,
            call(
                &conn,
                &out,
                wire::Request::UnregisterCheckIns { node_id: node.0 },
            )
            .await?,
            "Unregistered from its Check-Ins.",
        ),
        Command::Icd(Icd::StayAwake { node }) => {
            let request = wire::Request::StayAwake {
                node_id: node.0,
                on: true,
            };
            let response = call(&conn, &out, request).await?;
            if response.is_none() {
                return Ok(false);
            }
            if !out.json {
                println!("Keeping it awake once it wakes up; Ctrl-C to let it sleep.");
            }

            // The connection holds it awake: until interrupted.
            core::future::pending::<()>().await;
            Ok(true)
        }
        Command::Icd(Icd::Wake { node }) => {
            let request = wire::Request::WakeDevice { node_id: node.0 };
            match call(&conn, &out, request).await? {
                Some(wire::Response::Awake { ms, .. }) if !out.json => {
                    println!("Awake for {} s.", ms.div_ceil(1000));
                    Ok(true)
                }
                response => Ok(response.is_some()),
            }
        }
        Command::Assistant { action } => assistant(&conn, &out, action).await,
        Command::Thread { view } => {
            let response = call(&conn, &out, wire::Request::ReadThreadMesh { fabric }).await?;
            if let Some(wire::Response::ThreadMesh { mesh }) = &response {
                if !out.json {
                    let findings_only = matches!(view, Some(ThreadView::Findings));
                    println!("{}", thread_text(mesh, findings_only));
                }
            }
            Ok(response.is_some())
        }
        Command::Serve { .. }
        | Command::Token { .. }
        | Command::Mcp { .. }
        | Command::Chat { .. } => {
            unreachable!("served above")
        }
    }
}

/// How the output is printed.
struct Out {
    json: bool,
}

impl Out {
    fn event(&self, event: &wire::Event) {
        if self.json {
            println!("{}", serde_json::to_string(event).unwrap_or_default());
            return;
        }

        match event {
            wire::Event::Step { step, status, .. } => {
                let step = match step {
                    api::CommissionStep::Commission => "Commissioning",
                };
                match status {
                    api::StepStatus::Started => println!("{step}..."),
                    api::StepStatus::Done => println!("{step}: done"),
                    api::StepStatus::Failed { message } => println!("{step}: failed: {message}"),
                }
            }
            // A delta of attributes not shown here prints nothing.
            wire::Event::HealthReport { health, .. } => {
                if health.general.is_some() || health.software.is_some() {
                    println!("{}", health_text(health));
                }
            }
            wire::Event::Model { model, .. } => {
                for e in &model.events {
                    println!("{}", event_text(e));
                }
                for e in &model.endpoints {
                    for c in &e.clusters {
                        let cluster = named(c.name.as_deref(), display::hex_u32(c.id));
                        for a in &c.attributes {
                            let value = a.value.clone().or(a.error.clone()).unwrap_or_default();
                            println!(
                                "{} {cluster} {} = {value}",
                                e.id,
                                named(a.name.as_deref(), display::hex_u32(a.id))
                            );
                        }
                    }
                }
            }
            wire::Event::SubscriptionState { state, .. } => match state {
                api::SubscriptionState::Live { max_int_secs } => {
                    println!("(live, reporting at least every {max_int_secs} s)")
                }
                api::SubscriptionState::Offline => println!("(offline, retrying...)"),
                api::SubscriptionState::Stopped => println!("(stopped)"),
            },
            wire::Event::OtaProgress { progress, .. } => {
                println!("{}", ota_progress_text(progress))
            }
            // The commands ask the server for no chats, nor to follow the list.
            wire::Event::Chat { .. } | wire::Event::Devices { .. } => {}
        }
    }
}

/// Send `request` and wait for its response, printing the events meanwhile.
/// `None` when the API answered with an error (printed); with `--json`, the
/// response is printed here too.
async fn call(
    conn: &Connection,
    out: &Out,
    request: wire::Request,
) -> io::Result<Option<wire::Response>> {
    let response = conn.call(request, |event| out.event(&event)).await?;

    if out.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&response).unwrap_or_default()
        );
    }

    if let wire::Response::Error { message } = &response {
        if !out.json {
            eprintln!("error: {message}");
        }
        return Ok(None);
    }

    Ok(Some(response))
}

fn window(out: &Out, response: Option<wire::Response>) -> io::Result<bool> {
    let Some(wire::Response::CommissioningWindow { window }) = response else {
        return Ok(response.is_some());
    };

    if out.json {
        return Ok(true);
    }

    let opened_by = window
        .opened_by_vendor
        .map(|vid| format!(", opened by {}", display::vendor_label(vid, None)))
        .unwrap_or_default();
    match window.status {
        api::WindowStatus::Closed => println!("The commissioning window is closed."),
        api::WindowStatus::Basic => println!(
            "A basic commissioning window is open{opened_by}: the device's own setup code commissions it."
        ),
        api::WindowStatus::Enhanced if window.manual_code.is_none() => {
            println!("A commissioning window is open{opened_by}, with a code we do not know.")
        }
        api::WindowStatus::Enhanced => {}
    }

    if let (Some(code), Some(qr)) = (&window.manual_code, &window.qr_code) {
        println!(
            "Commission it in the other app (e.g. Google Home or Home Assistant: add a Matter device) with:"
        );
        println!();
        println!("  Code  {}", display::pairing_code_label(code));
        println!("  QR    {qr}");
        println!();
        for line in qr_lines(&window.qr_rows) {
            println!("  {line}");
        }
        if let Some(secs) = window.remaining_secs {
            println!("Open for another {}.", display::duration(secs));
        }
    }
    if let (Some(fabrics), Some(supported)) = (window.fabrics, window.supported_fabrics) {
        println!("It is on {fabrics} of {supported} fabrics.");
    }

    Ok(true)
}

fn device_time(out: &Out, response: Option<wire::Response>) -> io::Result<bool> {
    let (steps, time) = match response {
        Some(wire::Response::DeviceTime { time }) => (Vec::new(), time),
        Some(wire::Response::TimeSetup { setup }) => (setup.steps, setup.time),
        other => return Ok(other.is_some()),
    };

    if out.json {
        return Ok(true);
    }

    for step in &steps {
        match &step.error {
            None => println!("Set {}.", step.what),
            Some(e) => println!("Not set {}: {e}.", step.what),
        }
    }
    if !steps.is_empty() {
        println!();
    }
    println!("{}", time_text(&time));

    Ok(true)
}

/// A device's clock: its time and how far from ours, its source, its zone.
fn time_text(time: &api::DeviceTime) -> String {
    if !time.supported {
        return "It keeps no clock we can see (it has no Time Synchronization cluster).".into();
    }

    let mut lines = vec![match time.utc_unix_ms {
        Some(ms) => format!(
            "UTC       {}{}",
            display::utc_label(ms),
            time.offset_ms
                .map(|offset| format!(", {} of ours", display::clock_offset_label(offset)))
                .unwrap_or_default()
        ),
        None => "UTC       none: it has no time".into(),
    }];
    lines.push(format!(
        "Accuracy  {}{}",
        time.granularity.label(),
        time.time_source
            .map(|source| format!(", from {}", display::time_source_label(source)))
            .unwrap_or_default()
    ));

    let f = &time.features;
    if f.time_sync_client {
        lines.push(format!(
            "Source    {}",
            time.trusted_source_label()
                .unwrap_or_else(|| "none: no node to take its time from".into())
        ));
    }
    if f.ntp_client {
        lines.push(format!(
            "NTP       {}",
            time.default_ntp.as_deref().unwrap_or("no default server")
        ));
    }
    if f.time_zone {
        for zone in &time.time_zones {
            lines.push(format!(
                "Zone      {}{}{}",
                display::utc_offset_label(zone.offset_secs),
                zone.name
                    .as_deref()
                    .map(|name| format!(" {name}"))
                    .unwrap_or_default(),
                zone.valid_at_unix_ms
                    .map(|ms| format!(" from {}", display::utc_label(ms)))
                    .unwrap_or_default()
            ));
        }
        if time.dst_offsets.is_empty() {
            lines.push("DST       none set".into());
        }
        for dst in &time.dst_offsets {
            lines.push(format!(
                "DST       {}{}{}",
                display::utc_offset_label(dst.offset_secs),
                dst.from_unix_ms
                    .map(|ms| format!(" from {}", display::utc_label(ms)))
                    .unwrap_or_default(),
                dst.until_unix_ms
                    .map(|ms| format!(" until {}", display::utc_label(ms)))
                    .unwrap_or_default()
            ));
        }
        if let Some(ms) = time.local_unix_ms {
            lines.push(format!("Local     {}", display::local_time_label(ms)));
        }
    }

    let can: Vec<&str> = [
        (f.time_zone, "time zone"),
        (f.ntp_client, "NTP client"),
        (f.ntp_server, "NTP server"),
        (f.time_sync_client, "takes time from a node"),
    ]
    .into_iter()
    .filter_map(|(has, what)| has.then_some(what))
    .collect();
    lines.push(format!(
        "Features  {}",
        if can.is_empty() {
            "none (UTC time only)".into()
        } else {
            can.join(", ")
        }
    ));

    lines.join("\n")
}

/// A QR code in the terminal: two rows of modules per line, in half blocks,
/// dark on light, with a quiet zone.
fn qr_lines(rows: &[String]) -> Vec<String> {
    const QUIET: usize = 2;

    let size = rows.len();
    let dark = |x: usize, y: usize| {
        x >= QUIET
            && y >= QUIET
            && rows
                .get(y - QUIET)
                .and_then(|row| row.as_bytes().get(x - QUIET))
                == Some(&b'1')
    };
    let width = size + 2 * QUIET;

    (0..width)
        .step_by(2)
        .map(|y| {
            (0..width)
                .map(|x| match (dark(x, y), dark(x, y + 1)) {
                    (false, false) => '█',
                    (true, false) => '▄',
                    (false, true) => '▀',
                    (true, true) => ' ',
                })
                .collect()
        })
        .collect()
}

fn ok(out: &Out, response: Option<wire::Response>, text: &str) -> io::Result<bool> {
    if response.is_some() && !out.json {
        println!("{text}");
    }

    Ok(response.is_some())
}

fn fabrics(out: &Out, response: Option<wire::Response>) -> io::Result<bool> {
    let Some(wire::Response::Fabrics { fabrics }) = response else {
        return Ok(response.is_some());
    };

    if out.json {
        return Ok(true);
    }

    for fabric in fabrics {
        println!(
            "#{:<3} {:<24} fabric {}  our node {}  {} device(s){}",
            fabric.index,
            fabric.name,
            display::hex_u64(fabric.fabric_id),
            display::hex_u64(fabric.node_id),
            fabric.devices,
            if fabric.default { "  (default)" } else { "" }
        );
    }

    Ok(true)
}

/// The filter the words of `list` make: a word with spaces (quoted in the
/// shell) is quoted again.
fn filter_text(words: &[String]) -> String {
    let quoted: Vec<String> = words
        .iter()
        .map(|word| {
            if !word.contains(char::is_whitespace) || word.contains('"') {
                return word.clone();
            }
            match word.split_once(':') {
                Some((key, value)) => format!("{key}:\"{value}\""),
                None => format!("\"{word}\""),
            }
        })
        .collect();

    quoted.join(" ")
}

/// What filters take, a facet or state a line.
fn print_filters() {
    println!("Words match the start of a word of anything known of a device: its tags,");
    println!("names and facet values. key:value matches one facet, key:a,b either value;");
    println!("-term negates; quotes keep spaces. Facets with numbers take <, <=, >, >=, =");
    println!("in their units (uptime:<1h, heap:<16k, spec:>=1.4). All terms must match.");
    println!("No OR between terms, and no brackets. Live facts (is:shared, uptime:, ...)");
    println!("are known only of the devices followed now.");
    println!();
    println!("Facets:");
    for facet in query::FACETS {
        println!("  {:<10} {}", format!("{}:", facet.key), facet.help);
    }
    println!();
    println!("States (is:):");
    for (state, help) in query::STATES {
        println!("  {state:<13} {help}");
    }
}

fn devices(out: &Out, response: Option<wire::Response>) -> io::Result<bool> {
    let Some(wire::Response::Devices { devices, notice }) = response else {
        return Ok(response.is_some());
    };

    if out.json {
        return Ok(true);
    }

    if let Some(notice) = notice {
        println!("{notice}");
    }
    if devices.is_empty() {
        println!("No devices on our fabric.");
    }
    // Which fabric each is on, when they are on several.
    let fabrics = devices
        .iter()
        .any(|d| d.handle.fabric_index != devices[0].handle.fabric_index);

    for d in &devices {
        let kind = d
            .device_type()
            .and_then(display::device_type_name)
            .map(|name| format!(" ({name})"))
            .unwrap_or_default();
        let mut line = format!(
            "{}  {}{kind}  vendor {}",
            display::hex_u64(d.handle.node_id),
            d.title(),
            d.vendor().unwrap_or_else(|| "unknown".into())
        );
        if fabrics {
            line = format!("[{}]  {line}", d.fabric);
        }
        if d.is_test_vendor() {
            line.push_str(" (test vendor)");
        }
        if let Some(hw) = d.hardware_version() {
            line.push_str(&format!("  hw {hw}"));
        }
        if let Some(sw) = d.software_version() {
            line.push_str(&format!("  sw {sw}"));
        }
        if let Some(kind) = d.network {
            line.push_str(&format!("  {}", kind.label()));
        }
        if let Some(sleep) = d.sleep() {
            line.push_str(&format!("  {}", sleep.label.to_lowercase()));
        }
        match d.online {
            Some(true) => line.push_str("  online"),
            Some(false) => line.push_str("  offline"),
            None => {}
        }
        if !d.remembered {
            line.push_str("  not remembered");
        }
        if !d.tags.is_empty() {
            line.push_str(&format!("  tags: {}", d.tags.join(", ")));
        }
        println!("{line}");
    }

    Ok(true)
}

fn ota_updates(out: &Out, response: Option<wire::Response>) -> io::Result<bool> {
    let Some(wire::Response::OtaUpdates { updates }) = response else {
        return Ok(response.is_some());
    };

    if out.json {
        return Ok(true);
    }

    if updates.is_empty() {
        println!("No device asked for an OTA update in the last hour.");
    }
    for update in updates {
        let mut line = format!(
            "{}  {}",
            display::hex_u64(update.node_id),
            ota_progress_text(&update.progress)
        );
        if let (Some(downloaded), Some(size)) = (update.downloaded, update.size) {
            line.push_str(&format!(
                "  {} of {} KiB",
                downloaded / 1024,
                size.div_ceil(1024)
            ));
        }
        line.push_str(&format!(
            "  for {}  last progress {} ago",
            display::duration(update.elapsed_secs),
            display::duration(update.idle_secs)
        ));
        if let Some(secs) = update.remaining_secs() {
            line.push_str(&format!("  about {} left", display::duration(secs)));
        }
        println!("{line}");
    }

    Ok(true)
}

fn images(out: &Out, response: Option<wire::Response>) -> io::Result<bool> {
    let Some(wire::Response::OtaImages { images }) = response else {
        return Ok(response.is_some());
    };

    if out.json {
        return Ok(true);
    }

    if images.is_empty() {
        println!("No OTA images stored.");
    }
    for image in images {
        println!(
            "{}  vendor {}  product {}  version {}  {} bytes",
            image.name,
            display::vendor_label(image.vendor_id, None),
            display::product_label(image.vendor_id, image.product_id, None),
            display::software_version_label(image.version, Some(image.version_str)),
            image.size
        );
    }

    Ok(true)
}

/// The Thread mesh, for a terminal.
fn thread_text(mesh: &api::ThreadMesh, findings_only: bool) -> String {
    let label = |ext: u64| {
        mesh.nodes
            .iter()
            .find(|node| node.ext_address == ext)
            .map(|node| node.label.clone())
            .unwrap_or_else(|| format!("{ext:016X}"))
    };
    let mut lines = Vec::new();

    if mesh.networks.is_empty() {
        lines.push("No Thread networks: no Thread devices read, no border routers seen.".into());
    }
    for net in &mesh.networks {
        let mut facts = Vec::new();
        if let Some(channel) = net.channel {
            facts.push(format!("channel {channel}"));
        }
        if let Some(pan) = net.pan_id {
            facts.push(format!("PAN 0x{pan:04X}"));
        }
        if !net.partitions.is_empty() {
            let partitions: Vec<String> = net.partitions.iter().map(u32::to_string).collect();
            facts.push(format!("partition {}", partitions.join(", ")));
        }
        lines.push(format!(
            "{}  {:016X}  {}",
            net.name.as_deref().unwrap_or("(unnamed)"),
            net.extended_pan_id,
            facts.join(", ")
        ));
        if !net.devices.is_empty() {
            let devices: Vec<String> = mesh
                .nodes
                .iter()
                .filter(|node| {
                    matches!(node.kind, api::MeshNodeKind::Device { node_id } if net.devices.contains(&node_id))
                })
                .map(|node| node.label.clone())
                .collect();
            lines.push(format!("  on our fabric: {}", devices.join(", ")));
        }
        if !net.border_routers.is_empty() {
            lines.push(format!(
                "  border routers: {}",
                net.border_routers.join(", ")
            ));
        }
    }

    if !findings_only {
        lines.push(String::new());
        let counts = &mesh.counts;
        lines.push(format!(
            "Nodes ({}): {} on our fabric, {} border routers; {} routers, {} end devices, \
             {} sleepy, {} of unknown role",
            counts.nodes,
            counts.on_our_fabric,
            counts.border_routers,
            counts.routers,
            counts.end_devices,
            counts.sleepy_end_devices,
            counts.unknown_role
        ));
        lines.push(format!(
            "  {:<8} {:<6} {:<16}  {:<30} {:<16} {}",
            "ROLE", "RLOC16", "EXT ADDRESS", "NAME", "SLEEP", "PARENT"
        ));
        for node in &mesh.nodes {
            let role = match node.role {
                Some(role) => thread_role_text(role),
                None if node.sleepy() => "sleepy?",
                None => "-",
            };
            let kind = match node.kind {
                api::MeshNodeKind::BorderRouter => " (border router)",
                _ => "",
            };
            lines.push(format!(
                "  {role:<8} {:<6} {:016X}  {:<30} {:<16} {}",
                node.rloc16
                    .map(|r| format!("{r:04X}"))
                    .unwrap_or_else(|| "-".into()),
                node.ext_address,
                format!("{}{kind}", node.label),
                node.sleep.as_ref().map(sleep_text).unwrap_or_default(),
                node.parent.map(label).unwrap_or_default()
            ));
        }

        if !mesh.lan_nodes.is_empty() {
            lines.push(String::new());
            lines.push(format!(
                "Matter nodes on Thread of other fabrics, seen on the LAN ({}, {} sleepy; \
                 where they sit is unknown):",
                counts.lan_nodes, counts.lan_sleepy
            ));
            for node in &mesh.lan_nodes {
                let fabrics: Vec<String> = node
                    .fabrics
                    .iter()
                    .map(|f| format!("node {:X} on {:016X}", f.node_id, f.compressed_fabric_id))
                    .collect();
                lines.push(format!(
                    "  {}  {}  {}{}",
                    node.host,
                    node.address,
                    fabrics.join(", "),
                    node.sleep
                        .as_ref()
                        .map(|sleep| format!("  {}", sleep_text(sleep)))
                        .unwrap_or_default()
                ));
            }
        }

        lines.push(String::new());
        lines.push(format!("Links ({}):", mesh.links.len()));
        let signal = |signal: Option<api::LinkSignal>| match signal {
            Some(api::LinkSignal { quality, rssi }) => {
                let mut parts = Vec::new();
                if let Some(q) = quality {
                    parts.push(format!("{q}/3"));
                }
                if let Some(rssi) = rssi {
                    parts.push(format!("{rssi} dBm"));
                }
                if parts.is_empty() {
                    "heard".into()
                } else {
                    parts.join(" ")
                }
            }
            None => "?".into(),
        };
        for link in &mesh.links {
            let (a, b) = (label(link.a), label(link.b));
            let kind = if link.parent_child {
                "child"
            } else {
                "routers"
            };
            lines.push(format!(
                "  {a} hears {b}: {}; {b} hears {a}: {}  ({kind})",
                signal(link.a_hears_b),
                signal(link.b_hears_a)
            ));
        }

        if !mesh.routes.is_empty() {
            lines.push(String::new());
            lines.push(format!(
                "Routes to routers with no known link ({}):",
                mesh.routes.len()
            ));
            for route in &mesh.routes {
                let via = route
                    .via
                    .map(|via| format!(" through {}", label(via)))
                    .unwrap_or_default();
                lines.push(format!(
                    "  {} reaches {}{via}, path cost {}",
                    label(route.from),
                    label(route.to),
                    route.path_cost
                ));
            }
        }
    }

    lines.push(String::new());
    if mesh.findings.is_empty() {
        lines.push("Nothing wrong found.".into());
    } else {
        lines.push(format!("Findings ({}):", mesh.findings.len()));
        for finding in &mesh.findings {
            let mark = match finding.severity {
                api::FindingSeverity::Warning => "!",
                api::FindingSeverity::Info => "i",
            };
            lines.push(format!("  {mark} {}", finding.text));
        }
    }

    lines.join("\n")
}

/// How a node sleeps, short: its ICD idle time, how often it polls while
/// idle.
fn sleep_text(sleep: &api::MeshSleep) -> String {
    let mut parts = Vec::new();
    match sleep.icd_mode {
        Some(api::IcdMode::Lit) => parts.push("LIT".to_string()),
        Some(api::IcdMode::Sit) => parts.push("SIT".to_string()),
        None => {}
    }
    if let Some(ms) = sleep.idle_interval_ms {
        parts.push(format!("polls {}", duration_ms(ms)));
    }

    parts.join(" ")
}

/// A span of milliseconds, in the largest unit that fits.
fn duration_ms(ms: u32) -> String {
    match ms {
        0..1_000 => format!("{ms} ms"),
        1_000..60_000 if ms.is_multiple_of(1_000) => format!("{} s", ms / 1_000),
        1_000..60_000 => format!("{:.1} s", ms as f32 / 1_000.0),
        _ => format!("{} min", ms / 60_000),
    }
}

fn thread_role_text(role: api::ThreadRole) -> &'static str {
    match role {
        api::ThreadRole::Unspecified => "?",
        api::ThreadRole::Unassigned => "detached",
        api::ThreadRole::SleepyEndDevice => "sleepy",
        api::ThreadRole::EndDevice => "end",
        api::ThreadRole::Reed => "reed",
        api::ThreadRole::Router => "router",
        api::ThreadRole::Leader => "leader",
    }
}

/// Show, set, try or forget the assistant's model.
async fn assistant(
    conn: &Connection,
    out: &Out,
    action: Option<AssistantAction>,
) -> io::Result<bool> {
    let request = match action.unwrap_or(AssistantAction::Show) {
        AssistantAction::Show => wire::Request::Assistant,
        AssistantAction::Set {
            url,
            model,
            key,
            no_api_key,
            think,
        } => wire::Request::SetAssistant {
            update: api::AssistantUpdate {
                url,
                model,
                think,
                key: if no_api_key {
                    api::KeyChange::Remove
                } else {
                    key.read()?
                },
            },
        },
        AssistantAction::Models { url, key } => {
            let key = key.read()?;
            let update =
                (url.is_some() || key != api::KeyChange::Keep).then_some(api::AssistantUpdate {
                    url,
                    key,
                    ..Default::default()
                });

            wire::Request::AssistantModels { update }
        }
        AssistantAction::Test => wire::Request::AssistantTest { update: None },
        AssistantAction::Clear => wire::Request::ClearAssistant,
    };

    let response = call(conn, out, request).await?;
    let answered = response.is_some();
    if out.json {
        return Ok(answered);
    }

    match response {
        Some(wire::Response::Assistant {
            assistant: Some(info),
        }) => {
            println!("Model:  {}", info.model);
            println!("API:    {}", info.url);
            println!(
                "Key:    {}",
                match &info.key_hint {
                    Some(hint) => format!("set ({hint})"),
                    None => "none".into(),
                }
            );
            println!("Thinks: {}", if info.think { "yes" } else { "no" });
            if info.from_env {
                println!("(set by the environment: MATTER_PEEK_LLM_URL, MATTER_PEEK_LLM_MODEL)");
            }
        }
        Some(wire::Response::Assistant { assistant: None }) => {
            println!(
                "No assistant model. Give it one, e.g.\n  \
                 matter-peek assistant set --url http://localhost:11434/v1 --model qwen3.5:9b"
            );
        }
        Some(wire::Response::Models { models }) => {
            if models.is_empty() {
                println!("The API offers no models.");
            }
            for model in models {
                println!("{model}");
            }
        }
        Some(wire::Response::Tested {
            model,
            reply,
            millis,
        }) => {
            println!(
                "{model} answered in {:.1} s: {}",
                millis as f32 / 1000.0,
                reply.trim()
            );
        }
        _ => {}
    }

    Ok(answered)
}

/// Follow a device's health until the subscription stops or the process is
/// interrupted.
async fn watch(conn: &Connection, out: &Out, node_id: u64) -> io::Result<bool> {
    if call(conn, out, wire::Request::Subscribe { node_id })
        .await?
        .is_none()
    {
        return Ok(false);
    }

    while let Some(msg) = conn.recv().await {
        let wire::ServerMsg::Event { event } = msg else {
            continue;
        };

        let stopped = matches!(
            event,
            wire::Event::SubscriptionState {
                state: api::SubscriptionState::Stopped,
                ..
            }
        );
        out.event(&event);

        if stopped {
            break;
        }
    }

    Ok(true)
}

/// Print a retrieved log, or write it to `output`.
fn log(intent: api::LogIntent, data: &str, output: Option<PathBuf>) -> io::Result<bool> {
    let log = BASE64.decode(data).map_err(io::Error::other)?;

    if let Some(path) = output {
        std::fs::write(&path, &log)?;
        println!("Wrote {} bytes to {}.", log.len(), path.display());
        return Ok(true);
    }

    if log.is_empty() {
        println!("The device has no {intent:?} log.");
        return Ok(true);
    }

    match String::from_utf8(log) {
        Ok(text) => print!("{text}"),
        Err(e) => {
            eprintln!(
                "error: the log is {} bytes of binary data; write it to a file with --output",
                e.as_bytes().len()
            );
            return Ok(false);
        }
    }

    Ok(true)
}

/// Upload an OTA image file in chunks.
/// Write the store to `file`, encrypted with `passphrase` if given. Its
/// contents are not printed, not even with --json.
async fn export(
    conn: &Connection,
    out: &Out,
    file: &Path,
    passphrase: Option<String>,
) -> io::Result<bool> {
    let encrypted = passphrase.is_some();
    let request = wire::Request::ExportStore { passphrase };
    let data = match conn.call(request, |event| out.event(&event)).await? {
        wire::Response::StoreFile { data } => BASE64.decode(data).map_err(io::Error::other)?,
        wire::Response::Error { message } => {
            if out.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&wire::Response::Error { message })
                        .unwrap_or_default()
                );
            } else {
                eprintln!("error: {message}");
            }
            return Ok(false);
        }
        _ => return Err(io::Error::other("an unexpected answer")),
    };

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    // It holds our fabrics' keys.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(file)?.write_all(&data)?;

    if out.json {
        println!(
            "{}",
            serde_json::json!({ "file": file, "encrypted": encrypted })
        );
    } else if encrypted {
        println!("Exported to {}, encrypted.", file.display());
    } else {
        println!(
            "Exported to {}, not encrypted: keep it where nobody else reads it.",
            file.display()
        );
    }

    Ok(true)
}

/// The passphrase to encrypt the store with: none with `plain`, else read
/// from stdin, else asked for twice.
fn new_passphrase(plain: bool, stdin: bool) -> io::Result<Option<String>> {
    if plain {
        return Ok(None);
    }
    if stdin {
        return read_line().map(Some);
    }

    let passphrase = rpassword::prompt_password("Passphrase to encrypt it with: ")?;
    if passphrase.is_empty() {
        return Err(io::Error::other(
            "no passphrase given (--plain writes the store unencrypted)",
        ));
    }
    if rpassword::prompt_password("The same again: ")? != passphrase {
        return Err(io::Error::other("the two passphrases differ"));
    }

    Ok(Some(passphrase))
}

/// The first line of stdin, without its line ending; not empty.
fn read_line() -> io::Result<String> {
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;

    match line.trim_end_matches(['\r', '\n']) {
        "" => Err(io::Error::other("no passphrase given")),
        line => Ok(line.into()),
    }
}

async fn upload(conn: &Connection, out: &Out, file: &PathBuf) -> io::Result<bool> {
    let image = std::fs::read(file)?;

    let request = wire::Request::OtaUploadBegin {
        size: image.len() as u64,
    };
    let upload_id = match call(conn, out, request).await? {
        Some(wire::Response::OtaUploadStarted { upload_id }) => upload_id,
        response => return Ok(response.is_some()),
    };

    let mut offset = 0;
    for chunk in image.chunks(UPLOAD_CHUNK_LEN) {
        let request = wire::Request::OtaUploadChunk {
            upload_id,
            offset,
            data: BASE64.encode(chunk),
        };
        if call(conn, out, request).await?.is_none() {
            return Ok(false);
        }

        offset += chunk.len() as u64;
        if !out.json {
            eprint!("\rUploading: {}%", offset * 100 / image.len().max(1) as u64);
            let _ = io::stderr().flush();
        }
    }
    if !out.json {
        eprintln!();
    }

    images(
        out,
        call(conn, out, wire::Request::OtaUploadEnd { upload_id }).await?,
    )
}

/// Update a device to an image, following the update until it is applied or
/// the device declines it.
async fn update(conn: &Connection, out: &Out, node_id: u64, name: String) -> io::Result<bool> {
    if call(conn, out, wire::Request::OtaUpdate { node_id, name })
        .await?
        .is_none()
    {
        return Ok(false);
    }
    if !out.json {
        println!("The device was told about the update; following it (interrupt to stop).");
    }

    while let Some(msg) = conn.recv().await {
        let wire::ServerMsg::Event { event } = msg else {
            continue;
        };

        let done = matches!(
            &event,
            wire::Event::OtaProgress {
                node_id: id,
                progress: api::OtaProgress::Applied { .. }
                    | api::OtaProgress::NotOffered
                    | api::OtaProgress::Failed { .. },
            } if *id == node_id
        );
        out.event(&event);

        if done {
            break;
        }
    }

    Ok(true)
}

/// Up to 4 digits is a discriminator; anything else a setup code.
fn discover_filter(filter: Option<String>) -> (Option<u16>, Option<String>) {
    match filter {
        Some(f) if f.len() <= 4 && f.bytes().all(|b| b.is_ascii_digit()) => (f.parse().ok(), None),
        Some(f) => (None, Some(f)),
        None => (None, None),
    }
}

/// The API's change for `change`, naming its network by its ID; `None` when
/// the device does not know the named network (reported).
async fn network_change(
    conn: &Connection,
    node_id: u64,
    endpoint: u16,
    change: NetworkChange,
) -> io::Result<Option<api::NetworkChange>> {
    let network = match &change {
        NetworkChange::Add { .. } => None,
        NetworkChange::Remove { network }
        | NetworkChange::Connect { network }
        | NetworkChange::Reorder { network, .. } => Some(network.clone()),
    };

    let network_id = match network {
        Some(name) => {
            let request = wire::Request::ReadNetwork { node_id };
            let known = match conn.call(request, |_| ()).await? {
                wire::Response::Network { network, .. } => network
                    .configs
                    .into_iter()
                    .filter(|config| config.endpoint == endpoint)
                    .flat_map(|config| config.networks)
                    .find(|known| {
                        known.name.eq_ignore_ascii_case(&name)
                            || hex(&known.id).eq_ignore_ascii_case(name.trim_start_matches("0x"))
                    }),
                wire::Response::Error { message } => {
                    eprintln!("error: {message}");
                    return Ok(None);
                }
                _ => None,
            };
            let Some(known) = known else {
                eprintln!("error: the device knows no network {name}");
                return Ok(None);
            };
            known.id
        }
        None => Vec::new(),
    };

    Ok(Some(match change {
        NetworkChange::Add {
            wifi,
            password,
            thread_dataset,
        } => api::NetworkChange::AddOrUpdate {
            credentials: match (wifi, thread_dataset) {
                (Some(ssid), _) => api::NetworkCredentials::Wifi {
                    ssid,
                    password: password.unwrap_or_default(),
                },
                (None, dataset) => api::NetworkCredentials::Thread {
                    dataset: dataset.unwrap_or_default(),
                },
            },
        },
        NetworkChange::Remove { .. } => api::NetworkChange::Remove { network_id },
        NetworkChange::Connect { .. } => api::NetworkChange::Connect { network_id },
        NetworkChange::Reorder { index, .. } => api::NetworkChange::Reorder { network_id, index },
    }))
}

/// A device's networking, a few facts per line; facts the device does not
/// serve are left out.
fn network_text(info: &api::NetworkInfo) -> String {
    use std::fmt::Write as _;

    /// The facts that have a value, as `name value`, comma-separated.
    fn facts(facts: &[(&str, Option<String>)]) -> String {
        facts
            .iter()
            .filter_map(|(name, value)| Some(format!("{name} {}", value.as_ref()?)))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn some<T: ToString>(v: Option<T>) -> Option<String> {
        v.map(|v| v.to_string())
    }

    let mut out = String::new();

    for iface in &info.interfaces {
        let _ = writeln!(
            out,
            "Interface {} ({}): {}, hardware address {}",
            iface.name,
            iface.kind.label(),
            if iface.operational { "up" } else { "down" },
            iface.hardware_address
        );
        for addr in iface.ipv4_addresses.iter().chain(&iface.ipv6_addresses) {
            let _ = writeln!(out, "  {addr}");
        }
    }

    for config in &info.configs {
        let kind = config.kind.map(|kind| kind.label()).unwrap_or("Network");
        let room = config
            .max_networks
            .map(|max| format!(", room for {max}"))
            .unwrap_or_default();
        let _ = writeln!(out, "{kind} networks (endpoint {}{room}):", config.endpoint);
        for network in &config.networks {
            let mark = if network.connected {
                "  (connected)"
            } else {
                ""
            };
            // A Thread network's name, with its extended PAN ID.
            let id = hex(&network.id);
            let also = if config.kind == Some(api::NetworkKind::Thread) && network.name != id {
                format!("  {id}")
            } else {
                String::new()
            };
            let _ = writeln!(out, "  {}{also}{mark}", network.name);
        }
        if let Some(status) = config.last_status {
            let on = config
                .last_network
                .as_ref()
                .map(|name| format!(" on {name}"))
                .unwrap_or_default();
            let error = config
                .last_connect_error
                .map(|e| format!(" (error {e})"))
                .unwrap_or_default();
            let _ = writeln!(out, "  Last operation: {}{on}{error}", status.label());
        }
    }

    if let Some(wifi) = &info.wifi {
        let _ = writeln!(
            out,
            "Wi-Fi: {}",
            facts(&[
                ("access point", wifi.bssid.clone()),
                ("signal", wifi.rssi.map(|rssi| format!("{rssi} dBm"))),
                ("channel", some(wifi.channel)),
                ("security", some(wifi.security.map(|s| s.label()))),
                ("standard", some(wifi.version.map(|v| v.label()))),
                ("max rate", wifi.current_max_rate.map(display::bit_rate)),
            ])
        );
        let counters = facts(&[
            ("beacons lost", some(wifi.beacon_lost_count)),
            ("beacons received", some(wifi.beacon_rx_count)),
            ("packets received", some(wifi.packet_unicast_rx_count)),
            ("packets sent", some(wifi.packet_unicast_tx_count)),
            ("overruns", some(wifi.overrun_count)),
        ]);
        if !counters.is_empty() {
            let _ = writeln!(out, "  {counters}");
        }
    }

    if let Some(thread) = &info.thread {
        let _ = writeln!(
            out,
            "Thread: {}",
            facts(&[
                ("role", some(thread.role.map(|r| r.label()))),
                ("network", thread.network_name.clone()),
                ("channel", some(thread.channel)),
                ("PAN", thread.pan_id.map(display::hex_u16)),
                ("extended PAN", thread.extended_pan_id.map(display::hex_u64)),
                ("mesh-local prefix", thread.mesh_local_prefix.clone()),
                ("RLOC16", thread.rloc16.map(display::hex_u16)),
            ])
        );
        let stats = facts(&[
            ("partition", some(thread.partition_id)),
            ("leader", some(thread.leader_router_id)),
            ("attach attempts", some(thread.attach_attempt_count)),
            ("parent changes", some(thread.parent_change_count)),
            ("partition changes", some(thread.partition_id_change_count)),
            ("detached", some(thread.detached_role_count)),
            ("routes", some(thread.route_count)),
        ]);
        if !stats.is_empty() {
            let _ = writeln!(out, "  {stats}");
        }
        for n in &thread.neighbors {
            let _ = writeln!(
                out,
                "  Neighbor {} (RLOC16 {}){}: {}",
                display::hex_u64(n.ext_address),
                display::hex_u16(n.rloc16),
                if n.is_child { ", our child" } else { "" },
                facts(&[
                    ("LQI", some(Some(n.lqi))),
                    (
                        "average signal",
                        n.average_rssi.map(|rssi| format!("{rssi} dBm"))
                    ),
                    ("last signal", n.last_rssi.map(|rssi| format!("{rssi} dBm"))),
                    (
                        "seen",
                        Some(format!("{} ago", display::duration(u64::from(n.age_secs))))
                    ),
                ])
            );
        }
    }

    if let Some(eth) = &info.ethernet {
        let _ = writeln!(
            out,
            "Ethernet: {}",
            facts(&[
                (
                    "speed",
                    eth.phy_rate_mbps.map(|mbps| format!("{mbps} Mbit/s"))
                ),
                (
                    "duplex",
                    eth.full_duplex
                        .map(|full| if full { "full" } else { "half" }.to_string())
                ),
                (
                    "carrier",
                    eth.carrier_detect
                        .map(|c| if c { "yes" } else { "no" }.to_string())
                ),
                ("packets received", some(eth.packet_rx_count)),
                ("packets sent", some(eth.packet_tx_count)),
                ("send errors", some(eth.tx_err_count)),
                ("collisions", some(eth.collision_count)),
                ("up", eth.time_since_reset_secs.map(display::duration)),
            ])
        );
    }

    if out.is_empty() {
        out.push_str("The device reports no networking.\n");
    }

    out
}

fn health_text(health: &api::DeviceHealth) -> String {
    fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
        v.map(|v| v.to_string()).unwrap_or_else(|| "-".into())
    }

    let mut parts = Vec::new();
    if let Some(g) = &health.general {
        parts.push(format!(
            "uptime {}, reboots {}, operational {} h",
            g.up_time_secs
                .map(display::duration)
                .unwrap_or_else(|| "-".into()),
            opt(g.reboot_count),
            opt(g.total_operational_hours)
        ));
    }
    if let Some(s) = &health.software {
        parts.push(format!(
            "heap free {}, used {}, high watermark {}",
            opt(s.current_heap_free),
            opt(s.current_heap_used),
            opt(s.current_heap_high_watermark)
        ));
    }

    let mut text = if parts.is_empty() && health.fabrics.is_none() {
        "(no health data)".to_string()
    } else {
        parts.join("; ")
    };
    if let Some(threads) = health
        .software
        .as_ref()
        .and_then(|s| s.thread_metrics.as_ref())
    {
        text.push('\n');
        text.push_str(&threads_text(threads));
    }
    if let Some(fabrics) = &health.fabrics {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&fabrics_text(fabrics, health.supported_fabrics));
    }

    text
}

/// The fabrics a device is on: the controllers it belongs to.
fn fabrics_text(fabrics: &[api::DeviceFabric], supported: Option<u8>) -> String {
    let of = supported.map(|n| format!(" of {n}")).unwrap_or_default();
    let mut lines = vec![format!("Fabrics ({}{of}):", fabrics.len())];

    for fabric in fabrics {
        lines.push(format!(
            "  {:<3} {:<28} vendor {}  fabric {}  node {}",
            fabric
                .index
                .map(|index| format!("#{index}"))
                .unwrap_or_default(),
            fabric.name(),
            display::vendor_label(fabric.vendor_id, None),
            display::hex_u64(fabric.fabric_id),
            display::hex_u64(fabric.node_id)
        ));
    }

    lines.join("\n")
}

/// The threads (tasks) of a device's software: the one with the least stack
/// to spare first.
fn threads_text(threads: &[api::ThreadMetric]) -> String {
    if threads.is_empty() {
        return "Tasks: none reported".into();
    }

    let mut threads: Vec<&api::ThreadMetric> = threads.iter().collect();
    threads.sort_by_key(|t| t.spare_rank());

    let bytes = |v: Option<u32>| v.map(|v| v.to_string()).unwrap_or_else(|| "-".into());
    let mut lines = vec![format!(
        "Tasks ({}):\n  {:<10} {:>8} {:>15} {:>10} {:>9}",
        threads.len(),
        "NAME",
        "STACK",
        "PEAK USE",
        "LEAST FREE",
        "FREE NOW"
    )];
    for t in threads {
        let name = t.name.clone().unwrap_or_else(|| format!("#{}", t.id));
        let peak = match (t.stack_peak(), t.stack_size) {
            (Some(peak), Some(size)) if size > 0 => {
                format!("{peak} ({}%)", peak as u64 * 100 / size as u64)
            }
            (peak, _) => bytes(peak),
        };
        lines.push(format!(
            "  {name:<10} {:>8} {peak:>15} {:>10} {:>9}",
            bytes(t.stack_size),
            bytes(t.stack_free_minimum),
            bytes(t.stack_free_current)
        ));
    }

    lines.join("\n")
}

/// A device's data model: by endpoint, its device types, then each cluster's
/// attributes and commands.
fn print_model(model: &api::DeviceModel) {
    for endpoint in &model.endpoints {
        let types: Vec<String> = endpoint
            .device_types
            .iter()
            .map(|t| display::device_type_label(t.id))
            .collect();
        println!("Endpoint {}: {}", endpoint.id, types.join(", "));

        for cluster in &endpoint.clusters {
            println!(
                "  {}",
                named(cluster.name.as_deref(), display::hex_u32(cluster.id))
            );
            for a in &cluster.attributes {
                let value = match (&a.value, &a.error) {
                    (Some(value), _) => value.clone(),
                    (None, Some(e)) => format!("({e})"),
                    (None, None) => String::new(),
                };
                println!(
                    "    {}{}  {value}",
                    named(a.name.as_deref(), display::hex_u32(a.id)),
                    if a.writable { " *" } else { "" }
                );
            }
            if !cluster.commands.is_empty() {
                let commands: Vec<String> = cluster
                    .commands
                    .iter()
                    .map(|c| named(c.name.as_deref(), display::hex_u32(c.id)))
                    .collect();
                println!("    commands: {}", commands.join(", "));
            }
        }
    }
}

/// An event in one line: its number, time, priority, where, what, fields.
fn event_text(e: &api::EventModel) -> String {
    let time = match (e.utc_ms, e.uptime_ms) {
        (Some(ms), _) => display::utc_label(ms),
        (None, Some(ms)) => format!("+{} after start", display::duration(ms / 1000)),
        (None, None) => String::new(),
    };
    let priority = match e.priority {
        api::EventPriority::Debug => "debug",
        api::EventPriority::Info => "info",
        api::EventPriority::Critical => "CRITICAL",
    };
    let fields = e.fields.clone().or(e.error.clone()).unwrap_or_default();

    format!(
        "#{}  {time}  {priority}  {} {} {}  {fields}",
        e.number,
        e.endpoint,
        named(e.cluster_name.as_deref(), display::hex_u32(e.cluster)),
        named(e.name.as_deref(), display::hex_u32(e.event)),
    )
}

/// `name (id)`, or just the ID.
fn named(name: Option<&str>, id: String) -> String {
    match name {
        Some(name) => format!("{name} ({id})"),
        None => id,
    }
}

/// A device the network advertises: its addresses, then its fabrics.
fn print_advertised(d: &api::AdvertisedDevice) {
    println!(
        "{}{}",
        d.addresses.join(", "),
        if d.ours() {
            ""
        } else {
            "  (not on our fabrics)"
        }
    );

    for f in &d.fabrics {
        let whose = match (&f.our_name, f.vendor_id) {
            (Some(name), _) => format!("ours: {name}"),
            (None, Some(vid)) => format!("by {}", display::vendor_label(vid, None)),
            (None, None) => "unknown controller".to_string(),
        };
        let label = f
            .label
            .as_ref()
            .map(|label| format!(", \"{label}\""))
            .unwrap_or_default();
        println!(
            "  fabric {:016X}  node {}  {whose}{label}",
            f.compressed_fabric_id,
            display::hex_u64(f.node_id)
        );
    }
}

fn ota_progress_text(progress: &api::OtaProgress) -> String {
    match progress {
        api::OtaProgress::Offered { name } => format!("Offered {name}"),
        api::OtaProgress::NotOffered => "No update for the device".into(),
        api::OtaProgress::Downloading { percent } => format!("Downloading: {percent}%"),
        api::OtaProgress::Applying { version } => {
            format!("Applying version {}...", display::hex_u32(*version))
        }
        api::OtaProgress::Applied { version } => {
            format!("Updated: runs version {}", display::hex_u32(*version))
        }
        api::OtaProgress::Failed { reason } => format!("Not updated: {reason}"),
    }
}

/// `bytes` in uppercase hex.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}
