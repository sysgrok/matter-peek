//! The web UI's files, built into the executable (see `build.rs`), over HTTP.

use std::borrow::Cow;
use std::io::{self, Read as _};

use async_net::TcpStream;
use flate2::read::GzDecoder;
use futures_lite::{AsyncReadExt, AsyncWriteExt};

use log::debug;

include!(concat!(env!("OUT_DIR"), "/web_files.rs"));

/// What a build without the web UI answers.
const NO_WEB_UI: &str = "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<title>matter-peek</title></head><body style=\"font-family: sans-serif; max-width: 40rem; margin: 3rem auto\">\
<h1>matter-peek</h1><p>This build has no web UI. Build it (<code>trunk build --release</code> \
in <code>matter-peek-web/</code>), then build matter-peek again: it takes the UI in. The API is at \
<code>/ws</code>, for the command line (<code>matter-peek --server ws://&lt;this address&gt;</code>).</p>\
</body></html>";

/// Answer the request whose head is `head` with a file of the web UI. The
/// connection closes after.
pub(super) async fn respond(mut stream: TcpStream, head: &str) -> io::Result<()> {
    // The head was only peeked at.
    let mut consumed = vec![0; head.len()];
    stream.read_exact(&mut consumed).await?;

    let mut request = head.lines().next().unwrap_or_default().split_whitespace();
    let (method, target) = (
        request.next().unwrap_or_default(),
        request.next().unwrap_or("/"),
    );
    let path = target.split(['?', '#']).next().unwrap_or("/");
    debug!("server: {method} {path}");
    let gzip = accepts_gzip(head);

    let mut encoding = "";
    let (status, content_type, cache, body): (&str, &str, &str, Cow<[u8]>) = match method {
        "GET" | "HEAD" => match file(path) {
            Some((name, content_type, gzipped)) => (
                "200 OK",
                content_type,
                if name == "index.html" {
                    "no-cache"
                } else {
                    "public, max-age=86400"
                },
                if gzip {
                    encoding = "Content-Encoding: gzip\r\n";
                    Cow::Borrowed(gzipped)
                } else {
                    Cow::Owned(gunzip(gzipped)?)
                },
            ),
            None if FILES.is_empty() => (
                "200 OK",
                "text/html; charset=utf-8",
                "no-cache",
                Cow::Borrowed(NO_WEB_UI.as_bytes()),
            ),
            None => (
                "404 Not Found",
                "text/plain",
                "no-cache",
                Cow::Borrowed(b"Not found"),
            ),
        },
        _ => (
            "405 Method Not Allowed",
            "text/plain",
            "no-cache",
            Cow::Borrowed(b"Method not allowed"),
        ),
    };

    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n{encoding}Content-Length: {}\r\n\
         Cache-Control: {cache}\r\nVary: Accept-Encoding\r\nX-Content-Type-Options: nosniff\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    if method != "HEAD" {
        stream.write_all(&body).await?;
    }
    stream.flush().await
}

/// The web UI's file at `path` (the page itself for `/`): its content type
/// and contents, as the app's window shows them. A build without the web UI
/// has a page saying how to add it.
pub fn web_file(path: &str) -> Option<(&'static str, Vec<u8>)> {
    match file(path) {
        Some((_, content_type, gzipped)) => Some((content_type, gunzip(gzipped).ok()?)),
        None if FILES.is_empty() => {
            Some(("text/html; charset=utf-8", NO_WEB_UI.as_bytes().to_vec()))
        }
        None => None,
    }
}

/// Whether the request takes gzipped contents.
fn accepts_gzip(head: &str) -> bool {
    head.lines().skip(1).any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.trim().eq_ignore_ascii_case("accept-encoding")
                && value.split(',').any(|coding| {
                    let coding = coding.split(';').next().unwrap_or_default().trim();
                    coding.eq_ignore_ascii_case("gzip") || coding == "*"
                })
        })
    })
}

fn gunzip(gzipped: &[u8]) -> io::Result<Vec<u8>> {
    let mut contents = Vec::new();
    GzDecoder::new(gzipped).read_to_end(&mut contents)?;

    Ok(contents)
}

/// The file at `path`: the page itself for `/`, and for a path that names no
/// file (the page's own navigation).
fn file(path: &str) -> Option<(&'static str, &'static str, &'static [u8])> {
    let name = path.trim_start_matches('/');
    let name = if name.is_empty() || !name.contains('.') {
        "index.html"
    } else {
        name
    };

    FILES.iter().find(|(file, _, _)| *file == name).copied()
}
