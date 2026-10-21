//! matter-peek — the command line: everything the web UI does, from a terminal.
//!
//! Every command goes through the matter-peek API: to a running
//! `matter-peek-server` with `--server <ws-url>`, else to a controller started
//! in this process (which needs the store to itself, so not while a server runs).
//!
//! ```text
//! matter-peek [--server ws://127.0.0.1:8087] [--json] <command> [args...]
//! matter-peek --help
//! ```

use std::io::{self, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use clap::{Parser, Subcommand, ValueEnum};

use matter_peek_core::api::{self, wire};
use matter_peek_server::client::{self, Connection};

/// How much of an OTA image goes in one upload request.
const UPLOAD_CHUNK_LEN: usize = 256 * 1024;

#[derive(Parser)]
#[command(
    name = "matter-peek",
    version,
    about = "Commission, inspect and update Matter devices"
)]
struct Cli {
    /// Use the matter-peek server at this URL (e.g. ws://127.0.0.1:8087),
    /// instead of a controller in this process.
    #[arg(long, global = true, value_name = "URL")]
    server: Option<String>,
    /// Print the API's responses and events as JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the devices on our fabric. Without --server, that is the remembered
    /// ones: the others are found by `sync`, which a server runs on its own.
    List,
    /// Sync the device list with the devices on our fabric.
    Sync,
    /// Find the commissionable devices on the local network.
    Discover {
        /// A discriminator or a setup code, to narrow to one device.
        filter: Option<String>,
        /// The most devices to list.
        #[arg(long, default_value_t = 16)]
        max: u8,
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
    /// Replace the tags of a remembered device (none clears them).
    Tag {
        node: Node,
        /// The tags, as separate arguments or comma-separated.
        tags: Vec<String>,
    },
    /// Read the health of a device.
    Peek { node: Node },
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
    /// Make a device identify itself (e.g. blink).
    Identify {
        node: Node,
        #[arg(default_value_t = 3)]
        secs: u16,
    },
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
}

#[derive(Subcommand)]
enum Ota {
    /// List the stored OTA images.
    Images,
    /// Store an OTA image file.
    Upload { file: PathBuf },
    /// Delete a stored OTA image.
    Delete { name: String },
    /// Update a device to a stored image, and follow the update.
    Update { node: Node, name: String },
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
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let cli = Cli::parse();

    match matter_peek_server::block_on(run(cli)) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Run the command; `Ok(false)` when the API answered with an error.
async fn run(cli: Cli) -> io::Result<bool> {
    let conn = match &cli.server {
        Some(url) => client::connect(url).await?,
        // Short-lived: no periodic sync, which the process could exit in the
        // middle of.
        None => client::connect_in_process(api::PeekConfig {
            store_dir: None,
            periodic_sync: false,
        })?,
    };

    let out = Out { json: cli.json };

    match cli.command {
        Command::List => devices(&out, call(&conn, &out, wire::Request::ListDevices).await?),
        Command::Sync => devices(&out, call(&conn, &out, wire::Request::Sync).await?),
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
                            "{}  commissionable id {:#018x}",
                            d.addr, d.commissionable_id
                        );
                    }
                    for d in ble_devices {
                        println!(
                            "Bluetooth {}  discriminator {}  vendor {:#06x}  product {:#06x}",
                            d.addr, d.discriminator, d.vendor_id, d.product_id
                        );
                    }
                    true
                }
                response => response.is_some(),
            })
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
            };

            Ok(match call(&conn, &out, request).await? {
                Some(wire::Response::Commissioned(result)) if !out.json => {
                    println!("Commissioned as node {:#018x}.", result.handle.node_id);
                    if let Some(node_id) = result.rematch_candidate {
                        println!(
                            "Note: the same model as device {node_id:#018x}; maybe the same unit."
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
        Command::Ota(Ota::Upload { file }) => upload(&conn, &out, &file).await,
        Command::Ota(Ota::Delete { name }) => images(
            &out,
            call(&conn, &out, wire::Request::OtaDelete { name }).await?,
        ),
        Command::Ota(Ota::Update { node, name }) => update(&conn, &out, node.0, name).await,
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

fn ok(out: &Out, response: Option<wire::Response>, text: &str) -> io::Result<bool> {
    if response.is_some() && !out.json {
        println!("{text}");
    }

    Ok(response.is_some())
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

    for d in &devices {
        let mut line = format!(
            "{:#018x}  {}  {}",
            d.handle.node_id,
            device_title(d),
            d.vendor_display
        );
        if d.is_test_vendor {
            line.push_str(" (test vendor)");
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
            "{}  {:04x}:{:04x}  version {} ({})  {} bytes",
            image.name,
            image.vendor_id,
            image.product_id,
            image.version_str,
            image.version,
            image.size
        );
    }

    Ok(true)
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
                progress: api::OtaProgress::Applied { .. } | api::OtaProgress::NotOffered,
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

/// A human label for a device: the user-set NodeLabel, else the device's own
/// ProductName, else the stable identifiers, else its node ID.
fn device_title(d: &api::DeviceInfo) -> String {
    if !d.node_label.is_empty() {
        d.node_label.clone()
    } else if !d.product_name.is_empty() {
        d.product_name.clone()
    } else if !d.unique_id.is_empty() {
        format!("uid {}", d.unique_id)
    } else if !d.serial_number.is_empty() {
        format!("sn {}", d.serial_number)
    } else {
        format!("node {:#018x}", d.handle.node_id)
    }
}

fn health_text(health: &api::DeviceHealth) -> String {
    fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
        v.map(|v| v.to_string()).unwrap_or_else(|| "-".into())
    }

    let mut parts = Vec::new();
    if let Some(g) = &health.general {
        parts.push(format!(
            "uptime {} s, reboots {}, operational {} h",
            opt(g.up_time_secs),
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

    if parts.is_empty() {
        "(no health data)".into()
    } else {
        parts.join("; ")
    }
}

fn ota_progress_text(progress: &api::OtaProgress) -> String {
    match progress {
        api::OtaProgress::Offered { name } => format!("Offered {name}"),
        api::OtaProgress::NotOffered => "No update for the device".into(),
        api::OtaProgress::Downloading { percent } => format!("Downloading: {percent}%"),
        api::OtaProgress::Applying { version } => format!("Applying version {version}..."),
        api::OtaProgress::Applied { version } => format!("Updated: runs version {version}"),
    }
}
