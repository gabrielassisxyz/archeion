//! The subresource pass driven end to end against a server this test starts.
//!
//! It opens a socket for the same reason the redirect guard does: what it covers lives inside
//! a dependency and has no reachable entry point. A subresource is fetched from the page
//! callback of a crawl that is already running, which means from a thread already driving the
//! engine's runtime, and a runtime cannot be entered from there. Nothing about that rule can
//! be asserted without running one, and getting it wrong is a panic on the first subresource
//! of the first page, invisible to every other test in this project.
//!
//! The socket is on loopback and the server is in this file, so nothing here reaches the web,
//! needs setup, or answers differently tomorrow. What it also pins down is the traffic: the
//! test asserts which paths were asked for, so a pass that starts consulting something else,
//! or asking twice, fails here.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use archeion::CanonicalUrl;
use archeion::capture::capture_seed;
use archeion::crawl::{Seed, SpiderEngine};
use archeion::readability::SiteRules;
use archeion::storage::Archive;
use tempfile::TempDir;

const PAGE: &str = r#"<html><head><title>A styled page</title>
    <link rel="stylesheet" href="/style.css"></head>
    <body><img src="/logo.png"></body></html>"#;
const STYLESHEET: &[u8] = b"body { color: rebeccapurple }";
/// Bytes that are not text, because a subresource is usually not text and a path that
/// transcoded them would corrupt every image in the archive while passing every other test.
const LOGO: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x01\xff\xfe";

#[test]
fn a_page_is_archived_with_the_files_it_referenced_fetched_for_real() {
    let dir = TempDir::new().expect("temp dir");
    let archive = Archive::open(dir.path()).expect("the archive opens");
    let site = Site::start();

    let run = capture_seed(
        &SpiderEngine::default(),
        &archive,
        &site.seed(),
        &SiteRules::default(),
    )
    .expect("the run completes");

    assert_eq!(run.captures_written, 1, "{run:#?}");
    assert_eq!(run.assets_stored, 2, "{run:#?}");
    assert_eq!(run.asset_fetches, 2, "{run:#?}");
    assert_eq!(run.assets_missed, 0, "{run:#?}");

    let url = CanonicalUrl::parse(&site.url("/index.html")).expect("valid url");
    let captures = archive.list_captures(&url).expect("captures are listed");
    let capture = archive
        .read_capture(&url, &captures[0])
        .expect("the capture reads back");
    let stored: Vec<(&str, Option<&str>, Vec<u8>)> = capture
        .assets
        .iter()
        .map(|asset| {
            (
                asset.final_url.as_str(),
                asset.media_type.as_deref(),
                archive
                    .read_body(&asset.body.sha256)
                    .expect("the subresource reads back"),
            )
        })
        .collect();
    assert_eq!(
        stored,
        [
            (
                site.url("/style.css").as_str(),
                Some("text/css"),
                STYLESHEET.to_vec()
            ),
            (
                site.url("/logo.png").as_str(),
                Some("image/png"),
                LOGO.to_vec()
            ),
        ]
    );

    // The traffic, and not only the result: one request each, and nothing consulted for a
    // subresource beyond the subresource. A robots file is asked for once, by the crawl.
    let mut asked = site.paths_asked_for();
    asked.sort();
    assert_eq!(
        asked,
        ["/index.html", "/logo.png", "/robots.txt", "/style.css"],
        "the pass sent traffic nobody asked it to send"
    );
}

/// The widest candidate points off loopback entirely, at port 1, which needs privileges
/// nothing in this suite has and which nothing can therefore be listening on. The address is
/// fixed rather than a socket this test bound and released, for the reason
/// `capture_from_a_closed_port` in `tests/cli_capture.rs` is: a released ephemeral port can be
/// reused by anything else on the machine before the fetch reaches it, and the failure would
/// then look like the fallback being wrong rather than like a port coming back to life.
const SRCSET_PAGE: &str = r#"<html><head><title>A responsive page</title></head>
    <body><img srcset="http://127.0.0.1:1/wide.jpg 1600w, /narrow.jpg 800w"></body></html>"#;
const NARROW_JPEG: &[u8] = b"\xff\xd8\xff\xe0a narrower rendition of the same photograph";

/// `arch-3ff`, end to end: the pass that would recover a lost rendition runs above the crawl
/// adapter, so nothing about a connection a real client actually refuses to make is visible
/// to a scripted engine. Nothing answers the widest candidate at all, the way a transformation
/// network that has not generated it yet would leave a request with no response, and the
/// narrower one is served normally.
#[test]
fn a_widest_srcset_candidate_nothing_answers_leaves_the_narrower_one_archived() {
    let dir = TempDir::new().expect("temp dir");
    let archive = Archive::open(dir.path()).expect("the archive opens");
    let site = SrcsetSite::start();

    let run = capture_seed(
        &SpiderEngine::default(),
        &archive,
        &site.seed(),
        &SiteRules::default(),
    )
    .expect("the run completes");

    assert_eq!(run.captures_written, 1, "{run:#?}");
    assert_eq!(run.assets_stored, 1, "{run:#?}");
    assert_eq!(run.assets_missed, 1, "{run:#?}");
    assert_eq!(
        run.asset_fetches, 2,
        "the widest candidate and its one fallback, and nothing past that: {run:#?}"
    );

    let url = CanonicalUrl::parse(&site.url("/index.html")).expect("valid url");
    let captures = archive.list_captures(&url).expect("captures are listed");
    let capture = archive
        .read_capture(&url, &captures[0])
        .expect("the capture reads back");

    assert_eq!(capture.assets.len(), 1, "{:?}", capture.assets);
    assert_eq!(capture.assets[0].final_url, site.url("/narrow.jpg"));
    assert!(
        capture.assets[0].is_fallback,
        "the rendition kept because the widest could not be fetched is marked as one"
    );
    assert_eq!(
        capture
            .assets_missed
            .iter()
            .map(|missed| missed.url.as_str())
            .collect::<Vec<_>>(),
        ["http://127.0.0.1:1/wide.jpg"],
        "the widest candidate's own miss is still named"
    );
}

/// A site on loopback that offers one picture at two sizes, one of which nothing answers.
struct SrcsetSite {
    port: u16,
}

impl SrcsetSite {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let port = listener.local_addr().expect("the bound address").port();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = answer_srcset(stream);
            }
        });
        Self { port }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn seed(&self) -> Seed {
        let mut seed = Seed::new(self.url("/index.html"));
        seed.allow_private_addresses = true;
        seed.max_pages = 4;
        seed.concurrency = 1;
        seed.max_retries = 0;
        seed.deadline = Some(Duration::from_secs(15));
        seed
    }
}

fn answer_srcset(mut stream: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut header = String::new();
    while reader.read_line(&mut header)? > 2 {
        header.clear();
    }

    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let (status, media_type, body): (&str, &str, &[u8]) = match path.as_str() {
        "/robots.txt" => ("404 Not Found", "text/plain", b""),
        "/index.html" => ("200 OK", "text/html; charset=utf-8", SRCSET_PAGE.as_bytes()),
        "/narrow.jpg" => ("200 OK", "image/jpeg", NARROW_JPEG),
        _ => ("404 Not Found", "text/plain", b"not here"),
    };

    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {media_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// A site on loopback, and the record of what was asked of it.
struct Site {
    port: u16,
    asked: Arc<Mutex<Vec<String>>>,
}

impl Site {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let port = listener.local_addr().expect("the bound address").port();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let recording = Arc::clone(&asked);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = answer(stream, &recording);
            }
        });
        Self { port, asked }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn seed(&self) -> Seed {
        let mut seed = Seed::new(self.url("/index.html"));
        // The server is on loopback, which is exactly the range a run is refused for unless it
        // says it meant it. It is also the only way this path is exercised at all.
        seed.allow_private_addresses = true;
        seed.max_pages = 4;
        seed.concurrency = 1;
        seed.max_retries = 0;
        // So a guard that stopped working fails this test instead of hanging the suite.
        seed.deadline = Some(Duration::from_secs(15));
        seed
    }

    fn paths_asked_for(&self) -> Vec<String> {
        self.asked.lock().expect("the recording survived").clone()
    }
}

fn answer(mut stream: TcpStream, asked: &Mutex<Vec<String>>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    // The whole request has to be consumed before the answer, or the client sees the close
    // as a reset rather than as a response.
    let mut header = String::new();
    while reader.read_line(&mut header)? > 2 {
        header.clear();
    }

    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let (status, media_type, body): (&str, &str, &[u8]) = match path.as_str() {
        // A 404 is the answer that permits every path, and the crawl asks for this first.
        "/robots.txt" => ("404 Not Found", "text/plain", b""),
        "/index.html" => ("200 OK", "text/html; charset=utf-8", PAGE.as_bytes()),
        "/style.css" => ("200 OK", "text/css", STYLESHEET),
        "/logo.png" => ("200 OK", "image/png", LOGO),
        _ => ("404 Not Found", "text/plain", b"not here"),
    };
    asked.lock().expect("the recording survived").push(path);

    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {media_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}
