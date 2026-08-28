//! The fetch path, driven end to end against a server this test starts.
//!
//! It is the one test here that opens a socket, and the reason is that the guard it covers
//! lives nowhere this project can reach: whether a redirect into a private address is
//! followed is decided inside the engine's HTTP client, by a policy with no public entry
//! point and no constructible input. Asserting which policy was configured proves the
//! configuration and not the refusal.
//!
//! The socket is on loopback and the server is in this file, so nothing here reaches the
//! web, needs setup, or answers differently tomorrow. That is the line the rule draws: a
//! test that crawls the web is a crawl, and this one crawls a server it wrote itself.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use archeion::CanonicalUrl;
use archeion::capture::capture_seed;
use archeion::crawl::{CrawlError, Seed, SpiderEngine};
use archeion::readability::SiteRules;
use archeion::storage::Archive;
use tempfile::TempDir;

/// Where a cloud instance keeps the credentials of whatever is running on it, and the
/// first address an attacker-controlled redirect aims at.
const METADATA_URL: &str = "http://169.254.169.254/latest/meta-data/";

#[test]
fn a_redirect_into_the_metadata_service_is_refused_and_nothing_of_it_is_archived() {
    let dir = TempDir::new().expect("temp dir");
    let archive = Archive::open(dir.path()).expect("the archive opens");
    let port = serve_one_redirect_to(METADATA_URL);

    let run = capture_seed(
        &SpiderEngine::default(),
        &archive,
        &local_seed(port),
        &SiteRules::default(),
    )
    .expect("the run completes");

    assert_eq!(
        run.captures_written, 0,
        "a hop into the metadata service produced a record"
    );
    let metadata = CanonicalUrl::parse(METADATA_URL).expect("valid url");
    assert!(
        archive
            .list_captures(&metadata)
            .expect("captures are listed")
            .is_empty()
    );
    assert_eq!(
        run.failed_fetches.len(),
        1,
        "the refusal was not reported as a URL that produced nothing: {run:#?}"
    );
    assert!(
        run.failed_fetches[0].reason.contains("redirect"),
        "the seed failed for some reason other than its redirect: {}",
        run.failed_fetches[0].reason
    );
}

/// The other half of the same guard, and the half the engine cannot cover: it screens the
/// hops and dials the seed straight, so a seed pointed at the metadata service is stopped
/// here or not at all. Nothing is served in this test because nothing should be dialled.
#[test]
fn a_seed_pointed_at_the_metadata_service_never_reaches_a_socket() {
    let dir = TempDir::new().expect("temp dir");
    let archive = Archive::open(dir.path()).expect("the archive opens");

    let error = capture_seed(
        &SpiderEngine::default(),
        &archive,
        &Seed::new(METADATA_URL),
        &SiteRules::default(),
    )
    .expect_err("the seed is refused");

    assert!(
        matches!(
            error,
            archeion::capture::CaptureError::Crawl(CrawlError::UnusableSeed { .. })
        ),
        "the seed was not refused for its address: {error}"
    );
}

fn local_seed(port: u16) -> Seed {
    let mut seed = Seed::new(format!("http://127.0.0.1:{port}/"));
    // The server is on loopback, which is exactly the range a seed is refused for unless
    // the run says it meant it.
    seed.allow_private_addresses = true;
    seed.max_pages = 4;
    seed.concurrency = 1;
    seed.max_retries = 0;
    // So a guard that stopped working fails this test instead of hanging the suite.
    seed.deadline = Some(Duration::from_secs(15));
    seed
}

/// Answers every path with one redirect to `target`, for as long as the test process lives.
fn serve_one_redirect_to(target: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the bound address").port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = answer(stream, target);
        }
    });
    port
}

fn answer(mut stream: TcpStream, target: &str) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    // The whole request has to be consumed before the answer, or the client sees the close
    // as a reset rather than as a response.
    let mut header = String::new();
    while reader.read_line(&mut header)? > 2 {
        header.clear();
    }

    // The crawl asks for this before anything else, and a 404 is the answer that permits
    // every path.
    let response = if request_line.starts_with("GET /robots.txt") {
        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
    } else {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {target}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
    };
    stream.write_all(response.as_bytes())?;
    stream.flush()
}
