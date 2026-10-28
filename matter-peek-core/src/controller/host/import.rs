//! Importing a store: staged in the store's directory, and applied when the
//! controller starts next, before it loads anything (a running controller
//! cannot swap its fabrics).

use std::fs;
use std::io::{self, Write as _};
use std::path::{Component, Path, PathBuf};

use alloc::format;
use alloc::string::ToString;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use log::{error, info};

use crate::api::{PeekError, StoreArchive};
use crate::persist::is_controller_store;

/// The archive staged for the next start, in the store's directory.
const STAGED: &str = "import-pending.json";

/// Whether `archive` is a controller's store that can be imported.
pub(super) fn check(archive: &StoreArchive) -> Result<(), PeekError> {
    if archive.format != StoreArchive::FORMAT || archive.version != StoreArchive::VERSION {
        return Err(PeekError::failed(
            "not a matter-peek store, or one of another version",
        ));
    }
    if !is_controller_store(archive.entries.iter().map(|entry| entry.key)) {
        return Err(PeekError::failed("the store has no fabrics of ours"));
    }

    let entries = archive.entries.iter().map(|entry| &entry.data);
    let files = archive.files.iter().map(|file| &file.data);
    if entries
        .chain(files)
        .any(|data| BASE64.decode(data).is_err())
    {
        return Err(PeekError::failed("the store is damaged"));
    }
    if archive
        .files
        .iter()
        .any(|file| relative(Path::new(""), &file.path).is_none())
    {
        return Err(PeekError::failed("the store names a file outside of it"));
    }

    Ok(())
}

/// Stage `archive` to replace the store in `dir` at the next start.
pub(super) fn stage(dir: &Path, archive: &StoreArchive) -> Result<(), PeekError> {
    let staged = || -> io::Result<()> {
        let json = serde_json::to_vec(archive).map_err(invalid)?;

        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        // It holds the fabrics' keys.
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options.open(dir.join(STAGED))?.write_all(&json)
    };

    staged().map_err(|e| PeekError::failed(format!("cannot stage the import: {e}")))
}

/// Apply the import staged in `dir`, if any: the store's key-value entries
/// become the archive's, and its files are added.
pub(super) fn apply(dir: &Path) {
    let path = dir.join(STAGED);
    let Ok(json) = fs::read(&path) else {
        return;
    };

    match serde_json::from_slice::<StoreArchive>(&json) {
        Ok(archive) => match replace(dir, &archive) {
            Ok(()) => info!(
                "host: imported a store: {} entries, {} other files",
                archive.entries.len(),
                archive.files.len()
            ),
            Err(e) => error!("host: the import failed, the store may be incomplete: {e}"),
        },
        Err(e) => error!("host: the staged import is not a store: {e}"),
    }

    // It holds the fabrics' keys: not left behind.
    if let Err(e) = fs::remove_file(&path) {
        error!("host: cannot remove the staged import: {e}");
    }
}

/// Replace the key-value entries of the store in `dir` with those of
/// `archive` (rs-matter's `DirKvBlobStore` keeps each in `k_<key>`), and add
/// its files.
fn replace(dir: &Path, archive: &StoreArchive) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with("k_") {
            fs::remove_file(entry.path())?;
        }
    }

    for entry in &archive.entries {
        let data = BASE64.decode(&entry.data).map_err(invalid)?;
        fs::write(dir.join(format!("k_{:04x}", entry.key)), data)?;
    }

    for file in &archive.files {
        let Some(path) = relative(dir, &file.path) else {
            continue;
        };
        let data = BASE64.decode(&file.data).map_err(invalid)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, data)?;
    }

    Ok(())
}

/// `path` (`/`-separated) in `dir`, unless it leads out of it.
fn relative(dir: &Path, path: &str) -> Option<PathBuf> {
    let path = Path::new(path);
    let inside = !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)));

    inside.then(|| dir.join(path))
}

fn invalid(e: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_stay_in_the_store() {
        let dir = Path::new("/store");
        assert_eq!(
            relative(dir, "chats/1.json"),
            Some(PathBuf::from("/store/chats/1.json"))
        );
        assert_eq!(relative(dir, "../k_0001"), None);
        assert_eq!(relative(dir, "chats/../../x"), None);
        assert_eq!(relative(dir, "/etc/passwd"), None);
        assert_eq!(relative(dir, ""), None);
    }
}
