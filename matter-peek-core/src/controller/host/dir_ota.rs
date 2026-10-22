//! The OTA images, as files in a directory.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use log::warn;

use rs_matter::error::{Error, ErrorCode};
use rs_matter::ota::OtaImageHeader;

use alloc::format;
use alloc::vec::Vec;

use crate::ota::{OtaImage, OtaImageStore, OtaStore};

/// How much of a file to read for its header; any valid header fits.
const MAX_HEADER_LEN: u64 = 1024;

/// The OTA images: `<name>` files in a directory, named by [`OtaImage::file_name`].
pub struct DirOtaStore {
    dir: PathBuf,
}

impl DirOtaStore {
    /// Keep the images in `dir`, creating it if needed, dropping the partial
    /// uploads a previous run left, and naming each image as its header does
    /// (older versions named them differently).
    pub fn new(dir: PathBuf) -> Self {
        if let Err(e) = fs::create_dir_all(&dir) {
            warn!("ota: cannot create {}: {e}", dir.display());
        }

        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();

            if path.extension().is_some_and(|ext| ext == "partial") {
                let _ = fs::remove_file(&path);
            } else if let Some(image) = read_image(&path) {
                let named = dir.join(&image.name);

                if named != path {
                    if let Err(e) = fs::rename(&path, &named) {
                        warn!(
                            "ota: cannot rename {} to {}: {e}",
                            path.display(),
                            image.name
                        );
                    }
                }
            }
        }

        Self { dir }
    }

    /// The path of image `name`; `None` for a name that is not an image file's.
    fn path(&self, name: &str) -> Option<PathBuf> {
        let valid = name.ends_with(".ota") && !name.starts_with('.') && !name.contains(['/', '\\']);

        valid.then(|| self.dir.join(name))
    }

    /// The image in file `name`, if it is one.
    fn image(&self, name: &str) -> Option<OtaImage> {
        let image = read_image(&self.path(name)?)?;

        // A file whose name is not its header's is not one we stored.
        (image.name == name).then_some(image)
    }
}

/// An image file being received.
pub struct PartialImage {
    file: File,
    path: PathBuf,
}

impl OtaStore for DirOtaStore {
    fn images(&self) -> Vec<OtaImage> {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };

        let mut images: Vec<_> = entries
            .filter_map(|entry| self.image(entry.ok()?.file_name().to_str()?))
            .collect();

        images.sort_by(|a, b| a.name.cmp(&b.name));
        images
    }

    fn size(&self, name: &str) -> Option<u64> {
        Some(fs::metadata(self.path(name)?).ok()?.len())
    }

    fn read(&self, name: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        let mut file = File::open(self.path(name).ok_or(ErrorCode::NotFound)?)?;
        file.seek(SeekFrom::Start(offset))?;

        Ok(file.read(buf)?)
    }
}

impl OtaImageStore for DirOtaStore {
    type Partial = PartialImage;

    fn begin(&self, id: u32) -> Result<PartialImage, Error> {
        let path = self.dir.join(format!("upload-{id}.partial"));

        Ok(PartialImage {
            file: File::create(&path)?,
            path,
        })
    }

    fn write(&self, partial: &mut PartialImage, data: &[u8]) -> Result<(), Error> {
        partial.file.write_all(data)?;

        Ok(())
    }

    fn finish(&self, partial: PartialImage, image: &OtaImage) -> Result<(), Error> {
        let stored = self
            .path(&image.name)
            .ok_or(ErrorCode::InvalidData.into())
            .and_then(|path| {
                partial.file.sync_all()?;
                fs::rename(&partial.path, path)?;

                Ok(())
            });

        if stored.is_err() {
            let _ = fs::remove_file(&partial.path);
        }

        stored
    }

    fn discard(&self, partial: PartialImage) {
        let _ = fs::remove_file(&partial.path);
    }

    fn remove(&self, name: &str) -> Result<(), Error> {
        let path = self.path(name).ok_or(ErrorCode::NotFound)?;
        fs::remove_file(path)?;

        Ok(())
    }
}

/// The image in the `.ota` file at `path`, as its header describes it.
fn read_image(path: &Path) -> Option<OtaImage> {
    if path.extension().is_none_or(|ext| ext != "ota") {
        return None;
    }

    let mut file = File::open(path).ok()?;
    let size = file.metadata().ok()?.len();

    let mut head = Vec::new();
    (&mut file)
        .take(MAX_HEADER_LEN)
        .read_to_end(&mut head)
        .ok()?;

    let (header, _) = OtaImageHeader::decode(&head).ok()?;

    Some(OtaImage::new(&header, size))
}
