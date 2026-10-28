//! Takes the web UI into the executable, if it is built: the files of
//! `MATTER_PEEK_WEB_DIST`, else of `../matter-peek-web/dist` (`trunk build
//! --release` in `matter-peek-web/`), else of `web-dist`, the copy kept in git
//! and in the package (`scripts/vendor-web.sh`), each gzipped. Without them,
//! the server says how to add it.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use flate2::write::GzEncoder;
use flate2::Compression;

fn main() {
    println!("cargo:rerun-if-env-changed=MATTER_PEEK_WEB_DIST");

    let crate_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let built = crate_dir.join("../matter-peek-web/dist");
    let dist = env::var_os("MATTER_PEEK_WEB_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if built.join("index.html").is_file() {
                built.clone()
            } else {
                crate_dir.join("web-dist")
            }
        });
    println!("cargo:rerun-if-changed={}", built.display());
    println!("cargo:rerun-if-changed={}", dist.display());

    let mut files = Vec::new();
    if dist.join("index.html").is_file() {
        collect(&dist, &dist, &mut files);
    } else {
        println!(
            "cargo:warning=no web UI in {}: the server will say how to add it",
            dist.display()
        );
    }
    files.sort();

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let mut out = String::from(
        "/// The files of the web UI: their paths, content types and gzipped\n\
         /// contents.\n\
         pub(super) static FILES: &[(&str, &str, &[u8])] = &[\n",
    );
    for (i, (name, path)) in files.iter().enumerate() {
        let gzipped = out_dir.join(format!("web-{i}.gz"));
        let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&fs::read(path).unwrap()).unwrap();
        fs::write(&gzipped, encoder.finish().unwrap()).unwrap();

        writeln!(
            out,
            "    ({name:?}, {:?}, include_bytes!({:?})),",
            content_type(name),
            gzipped.display().to_string()
        )
        .unwrap();
    }
    out.push_str("];\n");

    fs::write(out_dir.join("web_files.rs"), out).unwrap();
}

/// The files under `dir`, by their paths relative to `root`.
fn collect(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, files);
        } else if let Ok(name) = path.strip_prefix(root) {
            let name = name.to_string_lossy().replace('\\', "/");
            files.push((name, path.canonicalize().unwrap()));
        }
    }
}

fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or_default() {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "json" => "application/json",
        "webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    }
}
