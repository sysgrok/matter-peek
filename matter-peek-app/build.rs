//! The app serves the web UI from `web-dist`: a copy of the built
//! `../matter-peek-web/dist` (`trunk build --release` in `matter-peek-web/`),
//! refreshed here in a checkout and carried by a packaged crate
//! (`scripts/vendor-web.sh`). Without either, a page says how to add it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The page of an app built without its web UI.
const NO_UI: &str = "<!doctype html><meta charset=\"utf-8\"><title>matter-peek</title>
<p style=\"font: 16px system-ui; margin: 2rem\">This matter-peek was built without its
web UI. Build it with <code>( cd matter-peek-web &amp;&amp; trunk build --release )</code>,
then build the app again.</p>
";

fn main() {
    let crate_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let built = crate_dir.join("../matter-peek-web/dist");
    let dist = crate_dir.join("web-dist");
    println!("cargo:rerun-if-changed={}", built.display());

    if built.join("index.html").is_file() {
        let _ = fs::remove_dir_all(&dist);
        copy(&built, &dist).expect("cannot copy the web UI");
        // Git keeps the directory, which the Tauri CLI looks for.
        fs::write(dist.join(".gitkeep"), "").unwrap();
    } else if !dist.join("index.html").is_file() {
        println!(
            "cargo:warning=no web UI in {}: the app will say how to add it",
            built.display()
        );
        fs::create_dir_all(&dist).unwrap();
        fs::write(dist.join("index.html"), NO_UI).unwrap();
    }

    tauri_build::build()
}

/// Copy directory `from` to `to`.
fn copy(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }

    Ok(())
}
