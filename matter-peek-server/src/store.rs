//! The store as a file, a backup, and back: the controller's entries
//! ([`StoreArchive`]) and the assistant's chats, as gzipped JSON after a short
//! header. With a passphrase, the gzipped JSON is sealed with AES-256-GCM,
//! under a key derived from the passphrase with PBKDF2 (HMAC-SHA256).
//!
//! The header is `mpeek`, the format's version, and whether the file is
//! sealed. A sealed file goes on with PBKDF2's rounds (big-endian), the salt
//! and the nonce; the seal covers the header too.
//!
//! An import replaces the store from the controller's next start: a server
//! restarts for it, once it has answered.

use std::fs;
use std::io::{self, Read as _, Write as _};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_io::Timer;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use log::{error, info};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::pbkdf2;

use matter_peek_core::api::wire::{ClientMsg, Request, Response, ServerMsg};
use matter_peek_core::api::{MatterPeek, StoreArchive, StoreFile};

/// What a backup starts with, then its version and its kind.
const MAGIC: &[u8] = b"mpeek";
const VERSION: u8 = 1;
const PLAIN: u8 = 0;
const SEALED: u8 = 1;
const HEAD_LEN: usize = MAGIC.len() + 2;

/// The salt's length; a sealed file's header holds the rounds, the salt and
/// the nonce.
const SALT_LEN: usize = 16;
const SEALED_HEAD_LEN: usize = HEAD_LEN + 4 + SALT_LEN + NONCE_LEN;

/// The most JSON a backup may unpack to.
const MAX_JSON_LEN: u64 = 256 * 1024 * 1024;

/// PBKDF2's rounds, as OWASP recommends them for HMAC-SHA256; a file asking
/// for more than `MAX_ROUNDS` is not opened.
const ROUNDS: u32 = 600_000;
const MAX_ROUNDS: u32 = 10_000_000;

/// Where the chats are, in the store's directory.
pub(crate) const CHATS_DIR: &str = "chats";

/// How long a server waits after answering an import before it restarts.
const RESTART_DELAY: Duration = Duration::from_secs(1);

/// The store's files, for the connections.
pub(crate) struct StoreFiles {
    peek: Arc<MatterPeek>,
    dir: PathBuf,
    restart: bool,
}

impl StoreFiles {
    /// The files of `peek`'s store, in `dir`. `restart`: the process restarts
    /// after an import, with the store imported.
    pub(crate) fn new(peek: Arc<MatterPeek>, dir: PathBuf, restart: bool) -> Self {
        Self { peek, dir, restart }
    }

    /// Answer an `ExportStore` or an `ImportStore`.
    pub(crate) async fn handle(&self, msg: ClientMsg, out: async_channel::Sender<ServerMsg>) {
        let response = match msg.request {
            Request::ExportStore { passphrase } => self.export(passphrase).await,
            Request::ImportStore { data, passphrase } => self.import(data, passphrase).await,
            _ => Err("not a request for the store".into()),
        };

        let imported = matches!(response, Ok(Response::Imported { restarts: true }));
        let response = response.unwrap_or_else(|message| Response::Error { message });
        let _ = out
            .send(ServerMsg::Response {
                req_id: msg.req_id,
                response,
            })
            .await;

        if imported {
            info!("store: imported; restarting with it");
            Timer::after(RESTART_DELAY).await;
            restart();
        }
    }

    async fn export(&self, passphrase: Option<String>) -> Result<Response, String> {
        let mut archive = self.peek.export_store().await.map_err(|e| e.to_string())?;
        let dir = self.dir.clone();

        blocking::unblock(move || {
            archive.files =
                chat_files(&dir).map_err(|e| format!("cannot read the assistant's chats: {e}"))?;
            let passphrase = passphrase.filter(|passphrase| !passphrase.is_empty());
            let file = pack(&archive, passphrase.as_deref())?;

            Ok(Response::StoreFile {
                data: BASE64.encode(file),
            })
        })
        .await
    }

    async fn import(&self, data: String, passphrase: Option<String>) -> Result<Response, String> {
        let archive = blocking::unblock(move || {
            let file = BASE64.decode(data).map_err(|_| NOT_A_BACKUP)?;
            let passphrase = passphrase.filter(|passphrase| !passphrase.is_empty());
            unpack(&file, passphrase.as_deref())
        })
        .await?;
        self.peek
            .import_store(archive)
            .await
            .map_err(|e| e.to_string())?;

        Ok(Response::Imported {
            restarts: self.restart,
        })
    }
}

/// Whether `file`, a backup, is encrypted, and needs its passphrase.
pub fn is_sealed(file: &[u8]) -> bool {
    file.len() >= HEAD_LEN && file.starts_with(MAGIC) && file[HEAD_LEN - 1] == SEALED
}

/// The chats in the store in `dir`, as its files.
fn chat_files(dir: &Path) -> io::Result<Vec<StoreFile>> {
    let entries = match fs::read_dir(dir.join(CHATS_DIR)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if entry.file_type()?.is_file() {
            files.push(StoreFile {
                path: format!("{CHATS_DIR}/{name}"),
                data: BASE64.encode(fs::read(entry.path())?),
            });
        }
    }

    Ok(files)
}

const NOT_A_BACKUP: &str = "not a matter-peek backup";

/// `archive` as a backup, sealed with `passphrase` if given.
fn pack(archive: &StoreArchive, passphrase: Option<&str>) -> Result<Vec<u8>, String> {
    let json = serde_json::to_vec(archive).map_err(|e| e.to_string())?;
    let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
    gzip.write_all(&json).map_err(|e| e.to_string())?;
    let mut data = gzip.finish().map_err(|e| e.to_string())?;

    let mut file = MAGIC.to_vec();
    file.push(VERSION);
    let Some(passphrase) = passphrase else {
        file.push(PLAIN);
        file.append(&mut data);
        return Ok(file);
    };

    let salt: [u8; SALT_LEN] = rand::random();
    let nonce: [u8; NONCE_LEN] = rand::random();
    file.push(SEALED);
    file.extend(ROUNDS.to_be_bytes());
    file.extend(salt);
    file.extend(nonce);
    key(passphrase, &salt, ROUNDS)
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(&file[..]),
            &mut data,
        )
        .map_err(|_| "cannot encrypt the store")?;
    file.append(&mut data);

    Ok(file)
}

/// The store in backup `file`, opened with `passphrase` if it is sealed.
fn unpack(file: &[u8], passphrase: Option<&str>) -> Result<StoreArchive, String> {
    if file.len() < HEAD_LEN || !file.starts_with(MAGIC) {
        return Err(NOT_A_BACKUP.into());
    }
    if file[MAGIC.len()] != VERSION {
        return Err("a backup of another version of matter-peek".into());
    }

    let mut data = file[HEAD_LEN..].to_vec();
    let gzip = match file[HEAD_LEN - 1] {
        PLAIN => &data[..],
        SEALED => {
            let Some(passphrase) = passphrase else {
                return Err("the backup is encrypted: it needs its passphrase".into());
            };
            if file.len() < SEALED_HEAD_LEN {
                return Err("the backup is damaged".into());
            }
            let (head, rest) = file.split_at(SEALED_HEAD_LEN);
            let rounds = u32::from_be_bytes(head[HEAD_LEN..HEAD_LEN + 4].try_into().unwrap());
            let salt = &head[HEAD_LEN + 4..HEAD_LEN + 4 + SALT_LEN];
            let nonce: [u8; NONCE_LEN] = head[SEALED_HEAD_LEN - NONCE_LEN..].try_into().unwrap();
            if !(1..=MAX_ROUNDS).contains(&rounds) {
                return Err(
                    "the backup is encrypted in a way this matter-peek does not know".into(),
                );
            }

            data = rest.to_vec();
            key(passphrase, salt, rounds)
                .open_in_place(
                    Nonce::assume_unique_for_key(nonce),
                    Aad::from(head),
                    &mut data,
                )
                .map_err(|_| "the passphrase is wrong, or the backup is damaged")?
        }
        _ => return Err(NOT_A_BACKUP.into()),
    };

    let mut json = Vec::new();
    GzDecoder::new(gzip)
        .take(MAX_JSON_LEN)
        .read_to_end(&mut json)
        .map_err(|_| "the backup is damaged")?;

    serde_json::from_slice(&json).map_err(|_| NOT_A_BACKUP.into())
}

/// The AES-256-GCM key of `passphrase` and `salt`.
fn key(passphrase: &str, salt: &[u8], rounds: u32) -> LessSafeKey {
    let rounds = NonZeroU32::new(rounds).unwrap_or(NonZeroU32::MIN);
    let mut key = [0; 32];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        rounds,
        salt,
        passphrase.as_bytes(),
        &mut key,
    );

    LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key).expect("a 256-bit key"))
}

/// Start this process again, as it was started. On Windows, a new process
/// starts, and this one ends; the new one waits for it to let the store go
/// (see [`restarted`]).
fn restart() {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            error!("store: cannot restart, start matter-peek again: {e}");
            std::process::exit(1);
        }
    };
    let mut command = std::process::Command::new(exe);
    command.args(std::env::args_os().skip(1));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;

        let e = command.exec();
        error!("store: cannot restart, start matter-peek again: {e}");
        std::process::exit(1);
    }

    #[cfg(not(unix))]
    {
        command.env(RESTARTED, "1");
        if let Err(e) = command.spawn() {
            error!("store: cannot restart, start matter-peek again: {e}");
            std::process::exit(1);
        }
        std::process::exit(0);
    }
}

/// Set for the process [`restart`] starts, where it starts another.
#[cfg(not(unix))]
const RESTARTED: &str = "MATTER_PEEK_RESTARTED";

/// Whether this process was started by [`restart`], while the one before it
/// may still hold the store.
pub(crate) fn restarted() -> bool {
    #[cfg(unix)]
    return false;

    #[cfg(not(unix))]
    std::env::var_os(RESTARTED).is_some()
}

#[cfg(test)]
mod tests {
    use matter_peek_core::api::StoreEntry;

    use super::*;

    fn archive() -> StoreArchive {
        StoreArchive {
            format: StoreArchive::FORMAT.into(),
            version: StoreArchive::VERSION,
            entries: vec![StoreEntry {
                key: 1,
                data: BASE64.encode([1, 2, 3]),
            }],
            files: Vec::new(),
        }
    }

    #[test]
    fn sealed_opens_with_its_passphrase_only() {
        let file = pack(&archive(), Some("one two")).unwrap();
        assert!(is_sealed(&file));

        let opened = unpack(&file, Some("one two")).unwrap();
        assert_eq!(opened.entries[0].data, archive().entries[0].data);
        assert!(unpack(&file, Some("one three")).is_err());
        assert!(unpack(&file, None).is_err());

        let mut changed = file.clone();
        changed[HEAD_LEN] ^= 1;
        assert!(unpack(&changed, Some("one two")).is_err());
    }

    #[test]
    fn plain_opens_without_a_passphrase() {
        let file = pack(&archive(), None).unwrap();
        assert!(!is_sealed(&file));
        assert_eq!(unpack(&file, None).unwrap().entries.len(), 1);
        assert!(unpack(b"{}", None).is_err());
    }
}
