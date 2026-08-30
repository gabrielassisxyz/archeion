//! `--resume`: reading `owed.json` and asking only for what it names.
//!
//! The private address refusal and the redirect screen are proven here the way
//! `fetch_hardening.rs` proves them for a seed, directly against `capture_owed` rather than
//! through the binary: what is being checked is the guard, not the command line, and the
//! library call gets there with no process to spawn. The byte ceiling, the robots decision
//! and everything about the flag itself are proven through the binary below, because the
//! byte ceiling can only be settled once per process and the robots decision depends on the
//! real engine's own crawl of a real seed.
//!
//! Every socket here is on loopback and every server is in this file, so nothing reaches the
//! web, needs setup, or answers differently tomorrow.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use archeion::CanonicalUrl;
use archeion::capture::{capture_owed, owed_but_not_yet_filed};
use archeion::crawl::{Seed, SpiderEngine};
use archeion::readability::SiteRules;
use archeion::storage::{Archive, OwedAddress, OwedReason};
use tempfile::TempDir;

/// Where a cloud instance keeps its credentials, and an address refused before anything is
/// dialled: no server has to answer here for the guard to be exercised.
const METADATA_URL: &str = "http://169.254.169.254/latest/meta-data/";

fn owed_refused(url: &str) -> OwedAddress {
    OwedAddress {
        url: url.to_owned(),
        reason: OwedReason::Refused {
            status: 429,
            retry_after: None,
        },
    }
}

fn local_seed() -> Seed {
    let mut seed = Seed::new(String::new());
    seed.max_pages = 4;
    seed.concurrency = 1;
    seed.max_retries = 0;
    seed.deadline = Some(Duration::from_secs(15));
    seed
}

/// The first guard at this door: an owed address naming a network this run was not pointed
/// at is refused before anything is dialled, exactly as a seed is.
#[test]
fn a_resumed_address_pointing_inside_a_network_is_refused_without_allow_private_addresses() {
    let dir = TempDir::new().expect("temp dir");
    let archive = Archive::open(dir.path()).expect("the archive opens");
    archive
        .write_owed(&HashSet::new(), &[owed_refused(METADATA_URL)])
        .expect("the owed record is written");

    let owed = archive.read_owed().expect("the owed record reads back");
    let urls = owed_but_not_yet_filed(&archive, &owed);

    let run = capture_owed(
        &SpiderEngine::default(),
        &archive,
        &local_seed(),
        &SiteRules::default(),
        &urls,
    )
    .expect("the run completes");

    assert_eq!(
        run.captures_written, 0,
        "an address inside a network was archived"
    );
    let metadata = CanonicalUrl::parse(METADATA_URL).expect("valid url");
    assert!(
        !archive.has_captures(&metadata),
        "nothing was ever stored for the refused address"
    );
    assert_eq!(
        run.failed_fetches.len(),
        1,
        "the refusal was not reported: {run:#?}"
    );
    assert!(
        run.failed_fetches[0].reason.contains("network"),
        "the address failed for some reason other than its network: {}",
        run.failed_fetches[0].reason
    );
}

/// The other half: the same address, resumed with the flag that says the run means to
/// reach a local address, is fetched exactly as an ordinary seed on loopback would be.
#[test]
fn a_resumed_address_pointing_inside_a_network_is_reached_with_allow_private_addresses() {
    let dir = TempDir::new().expect("temp dir");
    let archive = Archive::open(dir.path()).expect("the archive opens");
    let port = serve_a_page();
    let url = format!("http://127.0.0.1:{port}/");
    archive
        .write_owed(&HashSet::new(), &[owed_refused(&url)])
        .expect("the owed record is written");

    let owed = archive.read_owed().expect("the owed record reads back");
    let urls = owed_but_not_yet_filed(&archive, &owed);
    let mut seed = local_seed();
    seed.allow_private_addresses = true;

    let run = capture_owed(
        &SpiderEngine::default(),
        &archive,
        &seed,
        &SiteRules::default(),
        &urls,
    )
    .expect("the run completes");

    assert_eq!(run.captures_written, 1, "{run:#?}");
    let canonical = CanonicalUrl::parse(&url).expect("valid url");
    assert!(archive.has_captures(&canonical));

    // `capture_owed` only drives the fetch; folding what it learned back into `owed.json`
    // is `--resume`'s own job, which the binary tests below exercise end to end. Done by
    // hand here so this test can stay a library call with no process to spawn.
    archive
        .write_owed(&run.archived_urls, &archeion::capture::owed_addresses(&run))
        .expect("the owed record is written");
    assert!(
        archive
            .read_owed()
            .expect("the owed record reads back")
            .is_empty(),
        "the address is no longer owed once it is archived"
    );
}

/// The third guard: a resumed address that redirects into a private range is refused by the
/// same policy that screens a seed's own redirects, whatever `--allow-private-addresses`
/// says about the seed's own address. Nothing here is served over the redirect, because
/// nothing should ever be dialled for it.
#[test]
fn a_resumed_address_that_redirects_into_a_private_range_is_refused() {
    let dir = TempDir::new().expect("temp dir");
    let archive = Archive::open(dir.path()).expect("the archive opens");
    let port = serve_one_redirect_to(METADATA_URL);
    let url = format!("http://127.0.0.1:{port}/");
    archive
        .write_owed(&HashSet::new(), &[owed_refused(&url)])
        .expect("the owed record is written");

    let owed = archive.read_owed().expect("the owed record reads back");
    let urls = owed_but_not_yet_filed(&archive, &owed);
    let mut seed = local_seed();
    // The seed's own address is on loopback and has to be allowed to be dialled at all; the
    // redirect target is refused by a different, unconditional policy, which is the one
    // this test is about.
    seed.allow_private_addresses = true;

    let run = capture_owed(
        &SpiderEngine::default(),
        &archive,
        &seed,
        &SiteRules::default(),
        &urls,
    )
    .expect("the run completes");

    assert_eq!(
        run.captures_written, 0,
        "a hop into a private range was archived: {run:#?}"
    );
    let canonical = CanonicalUrl::parse(&url).expect("valid url");
    assert!(!archive.has_captures(&canonical));
    assert_eq!(run.failed_fetches.len(), 1, "{run:#?}");
    assert!(
        run.failed_fetches[0].reason.contains("redirect"),
        "the address failed for some reason other than its redirect: {}",
        run.failed_fetches[0].reason
    );
}

fn serve_a_page() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the bound address").port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || answer_a_page(stream));
        }
    });
    port
}

fn answer_a_page(mut stream: TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
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
    let body: &[u8] = if path == "/robots.txt" {
        b""
    } else {
        b"<html><head><title>Paid down</title></head><body>paid down</body></html>"
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Answers every path but `/robots.txt` with one redirect to `target`, for as long as the
/// test process lives.
fn serve_one_redirect_to(target: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the bound address").port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = answer_one_redirect_to(stream, target);
        }
    });
    port
}

fn answer_one_redirect_to(mut stream: TcpStream, target: &str) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut header = String::new();
    while reader.read_line(&mut header)? > 2 {
        header.clear();
    }
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

fn archeion() -> Command {
    Command::new(env!("CARGO_BIN_EXE_archeion"))
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// One route's answer: status, media type and body, mutable after the server starts so a
/// test can serve a page as refused on one invocation and as ordinary on the next, on the
/// same address, the way a real host answers a rate limit differently once it has passed.
#[derive(Clone)]
struct RouteResponse {
    status: &'static str,
    media_type: &'static str,
    body: Vec<u8>,
}

fn ok(body: impl Into<Vec<u8>>) -> RouteResponse {
    RouteResponse {
        status: "200 OK",
        media_type: "text/html; charset=utf-8",
        body: body.into(),
    }
}

fn refused(body: impl Into<Vec<u8>>) -> RouteResponse {
    RouteResponse {
        status: "429 Too Many Requests",
        media_type: "text/plain",
        body: body.into(),
    }
}

/// A loopback site whose routes a test can read and rewrite while it runs, and whose
/// request log is what the guard and pacing tests below read to prove what the resumed run
/// did and did not ask for.
struct Site {
    port: u16,
    routes: Arc<Mutex<std::collections::HashMap<String, RouteResponse>>>,
    requests: Arc<Mutex<Vec<(String, Instant)>>>,
}

impl Site {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let port = listener.local_addr().expect("the bound address").port();
        let routes = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let site = Self {
            port,
            routes,
            requests,
        };
        let routes = Arc::clone(&site.routes);
        let requests = Arc::clone(&site.requests);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let routes = Arc::clone(&routes);
                let requests = Arc::clone(&requests);
                thread::spawn(move || answer_from_routes(stream, &routes, &requests));
            }
        });
        site
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn set_route(&self, path: &str, response: RouteResponse) {
        self.routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.to_owned(), response);
    }

    /// Every path asked for since the server started, in the order it was asked for.
    fn requested_paths(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|(path, _)| path.clone())
            .collect()
    }

    /// When `path` was first asked for, so a test can measure the gap between two requests
    /// rather than only that both happened.
    fn requested_at(&self, path: &str) -> Instant {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|(seen, _)| seen == path)
            .unwrap_or_else(|| panic!("{path} was never requested"))
            .1
    }

    fn clear_requests(&self) {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
}

fn answer_from_routes(
    mut stream: TcpStream,
    routes: &Arc<Mutex<std::collections::HashMap<String, RouteResponse>>>,
    requests: &Arc<Mutex<Vec<(String, Instant)>>>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
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
    requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push((path.clone(), Instant::now()));

    let default_not_found = RouteResponse {
        status: "404 Not Found",
        media_type: "text/plain",
        body: Vec::new(),
    };
    let response = routes
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&path)
        .cloned()
        .unwrap_or(default_not_found);
    let head = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        response.media_type,
        response.body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()
}

const ROUND_TRIP_INDEX: &str = r#"<html><head><title>Index</title></head>
    <body><ul>
        <li><a href="/served">served</a></li>
        <li><a href="/refused">refused</a></li>
    </ul></body></html>"#;
const ROUND_TRIP_SERVED: &str =
    "<html><head><title>Served</title></head><body>served</body></html>";
const ROUND_TRIP_REFUSED_PAGE: &str =
    "<html><head><title>Paid down</title></head><body>paid down</body></html>";

/// The bead's own "done when": a host that refuses one of two linked pages, then a resume
/// that pays exactly that page down and asks for nothing else. `--max-pages` bounds the
/// first run at five so the index and the served page both fit beside the refusal.
#[test]
fn a_resumed_run_asks_only_for_what_the_archive_still_owes() {
    let dir = TempDir::new().expect("temp dir");
    let site = Site::start();
    site.set_route(
        "/robots.txt",
        RouteResponse {
            status: "404 Not Found",
            media_type: "text/plain",
            body: Vec::new(),
        },
    );
    site.set_route("/index.html", ok(ROUND_TRIP_INDEX));
    site.set_route("/served", ok(ROUND_TRIP_SERVED));
    site.set_route("/refused", refused("Too Many Requests"));

    let first = archeion()
        .arg("capture")
        .arg(dir.path())
        .arg(site.url("/index.html"))
        .args([
            "--max-pages",
            "5",
            "--max-depth",
            "1",
            "--max-retries",
            "0",
            "--deadline",
            "30s",
            "--allow-private-addresses",
        ])
        .output()
        .expect("the binary runs");
    assert!(first.status.success(), "{}", stderr_of(&first));

    let archive = Archive::open_existing(dir.path()).expect("the first run created an archive");
    let served = CanonicalUrl::parse(&site.url("/served")).expect("valid url");
    let refused_url = CanonicalUrl::parse(&site.url("/refused")).expect("valid url");
    assert!(archive.has_captures(&served));
    assert!(!archive.has_captures(&refused_url));
    let owed = archive.read_owed().expect("the owed record reads back");
    assert_eq!(owed.len(), 1);
    assert_eq!(owed[0].url, site.url("/refused"));

    site.set_route("/refused", ok(ROUND_TRIP_REFUSED_PAGE));
    site.clear_requests();

    let second = archeion()
        .arg("capture")
        .arg(dir.path())
        .arg("--resume")
        .args(["--deadline", "30s", "--allow-private-addresses"])
        .output()
        .expect("the binary runs");
    assert!(second.status.success(), "{}", stderr_of(&second));
    assert!(
        stdout_of(&second).contains("1 requested, 0 still owed"),
        "{}",
        stdout_of(&second)
    );

    assert!(
        archive
            .read_owed()
            .expect("the owed record reads back")
            .is_empty(),
        "the paid address is no longer owed"
    );
    assert!(archive.has_captures(&refused_url));

    let asked = site.requested_paths();
    assert!(
        !asked.contains(&"/index.html".to_owned()),
        "the resume re-asked for the seed: {asked:?}"
    );
    assert!(
        !asked.contains(&"/served".to_owned()),
        "the resume re-asked for a page it already held: {asked:?}"
    );
    assert!(
        asked.contains(&"/refused".to_owned()),
        "the resume never asked for the one address it owed: {asked:?}"
    );
}

/// The fourth guard, and the one no test anywhere else can pin at this door: a resumed
/// address `robots.txt` disallows is never dialled, not merely left unarchived. Only
/// `engine.crawl` reads the file at all, which is why `capture_owed` goes through the same
/// branch a seed's own crawl does rather than a plain fetch; the request log below is what
/// tells "refused before being asked" apart from "asked and refused".
#[test]
fn a_resumed_address_robots_txt_disallows_is_never_asked_for() {
    let dir = TempDir::new().expect("temp dir");
    let site = Site::start();
    site.set_route(
        "/robots.txt",
        ok("User-agent: *\nDisallow: /private\n".as_bytes().to_vec()),
    );
    site.set_route("/private", ok("<html><body>never served</body></html>"));

    let archive = Archive::open(dir.path()).expect("the archive opens");
    archive
        .write_owed(&HashSet::new(), &[owed_refused(&site.url("/private"))])
        .expect("the owed record is written");

    let output = archeion()
        .arg("capture")
        .arg(dir.path())
        .arg("--resume")
        .args(["--deadline", "15s", "--allow-private-addresses"])
        .output()
        .expect("the binary runs");
    assert!(output.status.success(), "{}", stderr_of(&output));

    assert!(
        !site.requested_paths().contains(&"/private".to_owned()),
        "a page robots.txt disallows was still asked for: {:?}",
        site.requested_paths()
    );
    let disallowed = CanonicalUrl::parse(&site.url("/private")).expect("valid url");
    assert!(!archive.has_captures(&disallowed));
    let owed = archive.read_owed().expect("the owed record reads back");
    assert_eq!(
        owed.iter()
            .map(|address| address.url.as_str())
            .collect::<Vec<_>>(),
        vec![site.url("/private")],
        "an address the run never even reached must stay owed"
    );
}

/// A page whose response outgrows `--max-response-bytes` is recorded up to the ceiling and
/// marked short, the same as a seed's own page is: the ceiling is process-wide and can only
/// be settled once, so this drives the binary rather than the library directly.
#[test]
fn a_resumed_address_over_the_byte_ceiling_is_stored_short() {
    let dir = TempDir::new().expect("temp dir");
    let site = Site::start();
    site.set_route(
        "/robots.txt",
        RouteResponse {
            status: "404 Not Found",
            media_type: "text/plain",
            body: Vec::new(),
        },
    );
    let oversized = vec![b'a'; 2 * archeion::crawl::SMALLEST_MAX_RESPONSE_BYTES];
    site.set_route("/big", ok(oversized));

    let archive = Archive::open(dir.path()).expect("the archive opens");
    archive
        .write_owed(&HashSet::new(), &[owed_refused(&site.url("/big"))])
        .expect("the owed record is written");

    let output = archeion()
        .arg("capture")
        .arg(dir.path())
        .arg("--resume")
        .args([
            "--deadline",
            "30s",
            "--allow-private-addresses",
            "--max-response-bytes",
            &archeion::crawl::SMALLEST_MAX_RESPONSE_BYTES.to_string(),
        ])
        .output()
        .expect("the binary runs");
    assert!(output.status.success(), "{}", stderr_of(&output));

    let big = CanonicalUrl::parse(&site.url("/big")).expect("valid url");
    let captures = archive.list_captures(&big).expect("captures are listed");
    assert_eq!(
        captures.len(),
        1,
        "the oversized page was not archived at all"
    );
    let capture = archive
        .read_capture(&big, &captures[0])
        .expect("the capture reads back");
    assert!(
        capture.body_truncated,
        "a body over the ceiling was not marked short"
    );
    assert!(
        capture.body.byte_len <= archeion::crawl::SMALLEST_MAX_RESPONSE_BYTES as u64,
        "the stored body is larger than the ceiling: {} bytes",
        capture.body.byte_len
    );
}

/// Execution policy, the whole entry: with no `--delay` given, two owed addresses on a host
/// declaring a `Crawl-delay` are still spaced apart, since the wait between them reads
/// whichever of the run's own delay and the host's own is longer.
#[test]
fn a_resume_waits_the_hosts_crawl_delay_between_addresses() {
    let dir = TempDir::new().expect("temp dir");
    let site = Site::start();
    site.set_route(
        "/robots.txt",
        ok("User-agent: *\nAllow: /\nCrawl-delay: 1\n"
            .as_bytes()
            .to_vec()),
    );
    site.set_route("/first", ok("<html><body>first</body></html>"));
    site.set_route("/second", ok("<html><body>second</body></html>"));

    let archive = Archive::open(dir.path()).expect("the archive opens");
    archive
        .write_owed(
            &HashSet::new(),
            &[
                owed_refused(&site.url("/first")),
                owed_refused(&site.url("/second")),
            ],
        )
        .expect("the owed record is written");

    let output = archeion()
        .arg("capture")
        .arg(dir.path())
        .arg("--resume")
        .args(["--deadline", "15s", "--allow-private-addresses"])
        .output()
        .expect("the binary runs");
    assert!(output.status.success(), "{}", stderr_of(&output));

    let gap = site
        .requested_at("/second")
        .duration_since(site.requested_at("/first"));
    assert!(
        gap >= Duration::from_millis(800),
        "the two owed addresses were not paced by the host's Crawl-delay: {gap:?}"
    );
}

/// The defect the grouping fix exists for: an owed record naming two hosts, one that
/// declares no `Crawl-delay` and one that does, resolved once from whichever host's address
/// happened to be first would leave the second host paced by nothing at all. Two loopback
/// servers on different ports are two different origins, which is what a multi-host owed
/// record actually is; the first host's address is asked for before either of the second
/// host's, so it cannot be what supplies the gap this test measures.
#[test]
fn a_resume_paces_each_host_by_its_own_crawl_delay_not_the_firsts() {
    let dir = TempDir::new().expect("temp dir");
    let quiet_host = Site::start();
    quiet_host.set_route(
        "/robots.txt",
        RouteResponse {
            status: "404 Not Found",
            media_type: "text/plain",
            body: Vec::new(),
        },
    );
    quiet_host.set_route("/from-quiet", ok("<html><body>quiet</body></html>"));

    let paced_host = Site::start();
    paced_host.set_route(
        "/robots.txt",
        ok("User-agent: *\nAllow: /\nCrawl-delay: 1\n"
            .as_bytes()
            .to_vec()),
    );
    paced_host.set_route("/from-paced-1", ok("<html><body>one</body></html>"));
    paced_host.set_route("/from-paced-2", ok("<html><body>two</body></html>"));

    let archive = Archive::open(dir.path()).expect("the archive opens");
    archive
        .write_owed(
            &HashSet::new(),
            &[
                owed_refused(&quiet_host.url("/from-quiet")),
                owed_refused(&paced_host.url("/from-paced-1")),
                owed_refused(&paced_host.url("/from-paced-2")),
            ],
        )
        .expect("the owed record is written");

    let output = archeion()
        .arg("capture")
        .arg(dir.path())
        .arg("--resume")
        .args(["--deadline", "15s", "--allow-private-addresses"])
        .output()
        .expect("the binary runs");
    assert!(output.status.success(), "{}", stderr_of(&output));

    let gap = paced_host
        .requested_at("/from-paced-2")
        .duration_since(paced_host.requested_at("/from-paced-1"));
    assert!(
        gap >= Duration::from_millis(800),
        "the second host's own addresses were not paced by its own Crawl-delay: {gap:?}"
    );
}

/// Report honesty, exit and `stopped`: a resume that pays down nothing still exits zero, on
/// the same reasoning a host answering 429 to a seed already does, but the report has to say
/// what is still missing rather than let "exhausted" read as "paid in full".
#[test]
fn a_resume_refused_again_reports_what_is_still_owed() {
    let dir = TempDir::new().expect("temp dir");
    let site = Site::start();
    site.set_route(
        "/robots.txt",
        RouteResponse {
            status: "404 Not Found",
            media_type: "text/plain",
            body: Vec::new(),
        },
    );
    site.set_route("/still-limited", refused("Too Many Requests"));

    let archive = Archive::open(dir.path()).expect("the archive opens");
    archive
        .write_owed(
            &HashSet::new(),
            &[owed_refused(&site.url("/still-limited"))],
        )
        .expect("the owed record is written");

    let output = archeion()
        .arg("capture")
        .arg(dir.path())
        .arg("--resume")
        .args([
            "--deadline",
            "15s",
            "--allow-private-addresses",
            "--max-retries",
            "0",
        ])
        .output()
        .expect("the binary runs");
    assert!(
        output.status.success(),
        "a host refusing again is the web misbehaving, not a failure: {}",
        stderr_of(&output)
    );
    assert!(
        stdout_of(&output).contains("1 requested, 1 still owed"),
        "a resume that paid nothing down read as though it had: {}",
        stdout_of(&output)
    );

    let owed = archive.read_owed().expect("the owed record reads back");
    assert_eq!(owed.len(), 1);
    assert_eq!(owed[0].url, site.url("/still-limited"));
}

/// `--resume` against an archive owed nothing exits zero and says so, rather than treating
/// an empty request list as a reason to fail.
#[test]
fn resume_against_an_archive_owed_nothing_exits_zero_and_says_so() {
    let dir = TempDir::new().expect("temp dir");
    let archive = Archive::open(dir.path()).expect("the archive opens");
    archive
        .write_owed(&HashSet::new(), &[])
        .expect("an empty owed record is written");

    let output = archeion()
        .arg("capture")
        .arg(dir.path())
        .arg("--resume")
        .args(["--deadline", "15s"])
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));
    assert!(
        stdout_of(&output).contains("archive owed nothing"),
        "{}",
        stdout_of(&output)
    );
}

/// The same answer for the archive `arch-qs9` predates: nothing in `owed.json` because
/// nothing ever wrote the file, which reads exactly like owing nothing rather than like a
/// panic or a silent empty success, on the terms `arch-9j5` already settled for such an
/// archive.
#[test]
fn resume_against_an_archive_with_no_owed_record_exits_saying_so() {
    let dir = TempDir::new().expect("temp dir");
    Archive::open(dir.path()).expect("the archive opens");
    assert!(
        !dir.path().join("owed.json").exists(),
        "this archive must carry no owed record for the case being tested"
    );

    let output = archeion()
        .arg("capture")
        .arg(dir.path())
        .arg("--resume")
        .args(["--deadline", "15s"])
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));
    assert!(
        stdout_of(&output).contains("archive owed nothing"),
        "{}",
        stdout_of(&output)
    );
}

/// Execution policy, the whole entry: `--max-pages` bounds a resume exactly as it bounds
/// any other run, so a resume given only enough budget for one of two owed addresses stops
/// there and leaves the other exactly as owed as it was before this run started.
#[test]
fn a_resume_bounded_by_max_pages_stops_early_and_leaves_the_rest_owed() {
    let dir = TempDir::new().expect("temp dir");
    let site = Site::start();
    site.set_route(
        "/robots.txt",
        RouteResponse {
            status: "404 Not Found",
            media_type: "text/plain",
            body: Vec::new(),
        },
    );
    site.set_route("/one", ok("<html><body>one</body></html>"));
    site.set_route("/two", ok("<html><body>two</body></html>"));

    let archive = Archive::open(dir.path()).expect("the archive opens");
    archive
        .write_owed(
            &HashSet::new(),
            &[
                owed_refused(&site.url("/one")),
                owed_refused(&site.url("/two")),
            ],
        )
        .expect("the owed record is written");

    let output = archeion()
        .arg("capture")
        .arg(dir.path())
        .arg("--resume")
        .args([
            "--deadline",
            "15s",
            "--allow-private-addresses",
            "--max-pages",
            "1",
        ])
        .output()
        .expect("the binary runs");
    assert!(output.status.success(), "{}", stderr_of(&output));

    let owed = archive.read_owed().expect("the owed record reads back");
    assert_eq!(
        owed.len(),
        1,
        "a resume bounded to one page paid down more than its budget: {owed:?}"
    );
}
