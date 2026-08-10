//! `--from-sitemap`, driven as the command it is: a process, its output, and its exit code.
//!
//! It opens a socket for the reason `cli_capture.rs` does: the binary builds the real engine,
//! and whether the flag actually reaches a fetch, a parse and an archive is not something a
//! scripted engine can prove. Nothing here leaves the machine, needs setup, or answers
//! differently tomorrow, because the server answering it is in this file.
//!
//! What is not here is the ceilings on the sitemap's own URL count, the gzip refusal and the
//! sitemap index refusal: those are properties of `sitemap::read_sitemap` and are proven
//! against a fake host in `src/sitemap.rs`, without paying for a round trip through a real
//! server to say the same thing.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output};
use std::sync::Arc;
use std::thread;

use archeion::CanonicalUrl;
use archeion::storage::Archive;
use tempfile::TempDir;

fn archeion() -> Command {
    Command::new(env!("CARGO_BIN_EXE_archeion"))
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// One path this server answers, and what it answers with. Built after the listener is bound
/// so a route can embed the port the server ended up on, which is what a `robots.txt` or a
/// sitemap of this server's own posts needs to do.
struct Route {
    path: &'static str,
    status: &'static str,
    media_type: &'static str,
    body: String,
}

/// A server on loopback answering exactly the routes it is given, and a 404 for anything
/// else. One thread per connection, because the client keeps a pool of them and answering in
/// turn would hold every later request behind whichever connection was opened first.
struct Site {
    port: u16,
}

impl Site {
    /// Binds a port without serving anything yet, so the routes below can name the address
    /// the server ended up on before the accept loop starts.
    fn bind() -> (TcpListener, Self) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let port = listener.local_addr().expect("the bound address").port();
        (listener, Self { port })
    }

    fn serve(listener: TcpListener, routes: Vec<Route>) {
        let routes = Arc::new(routes);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let routes = Arc::clone(&routes);
                thread::spawn(move || answer(stream, &routes));
            }
        });
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

fn route(path: &'static str, media_type: &'static str, body: impl Into<String>) -> Route {
    Route {
        path,
        status: "200 OK",
        media_type,
        body: body.into(),
    }
}

fn answer(mut stream: TcpStream, routes: &[Route]) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    // The whole request has to be consumed before the answer, or the client sees the close as
    // a reset rather than as a response.
    let mut header = String::new();
    while reader.read_line(&mut header)? > 2 {
        header.clear();
    }

    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let found = routes.iter().find(|route| route.path == path);
    let (status, media_type, body): (&str, &str, &str) = match found {
        Some(route) => (route.status, route.media_type, &route.body),
        None => ("404 Not Found", "text/plain", "not here"),
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {media_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

fn urlset(locs: &[String]) -> String {
    let entries: String = locs
        .iter()
        .map(|loc| format!("<url><loc>{loc}</loc></url>"))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">{entries}</urlset>"#
    )
}

const LONE_PAGE: &str = "<html><head><title>A post</title></head><body>\
    <p>Nothing on this page links anywhere else, which is the whole point of the sitemap.</p>\
    </body></html>";

/// The seed of the two phase run below: a page that does link, so the ordinary crawl has
/// something to spend the ceiling on before the sitemap phase begins.
const LINKING_PAGE: &str = "<html><head><title>An index</title></head><body>\
    <p>Two posts a crawl can reach: <a href=\"/posts/a\">a</a> and <a href=\"/posts/b\">b</a>.</p>\
    </body></html>";

fn capture_command(archive_path: &std::path::Path, seed_url: &str) -> Command {
    let mut command = archeion();
    command
        .arg("capture")
        .arg(archive_path)
        .arg(seed_url)
        .args([
            "--max-pages",
            "10",
            "--concurrency",
            "4",
            "--max-retries",
            "0",
        ])
        .args(["--deadline", "30s", "--allow-private-addresses"]);
    command
}

/// The measured shape this feature exists for: a seed whose own page links nowhere, and a
/// sitemap that lists three posts a crawl would never reach on its own. The directive naming
/// it is spelled in upper case, which is the spelling a real file was found using.
#[test]
fn a_sitemap_lists_pages_nothing_links_to_and_all_of_them_are_archived() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let (listener, site) = Site::bind();
    let posts = ["/posts/1", "/posts/2", "/posts/3"];
    let listed: Vec<String> = posts.iter().map(|path| site.url(path)).collect();
    let mut routes = vec![
        route("/index.html", "text/html; charset=utf-8", LONE_PAGE),
        route(
            "/robots.txt",
            "text/plain",
            format!("User-agent: *\nSITEMAP: {}\n", site.url("/sitemap.xml")),
        ),
        route("/sitemap.xml", "application/xml", urlset(&listed)),
    ];
    for path in posts {
        routes.push(route(path, "text/html; charset=utf-8", LONE_PAGE));
    }
    Site::serve(listener, routes);

    let output = capture_command(&archive_path, &site.url("/index.html"))
        .arg("--from-sitemap")
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(stderr_of(&output), "");
    assert!(
        stdout_of(&output).contains("archived 4 capture(s)"),
        "{}",
        stdout_of(&output)
    );
    assert!(
        stdout_of(&output).contains("sitemap       3 taken, 0 refused, 3 listed"),
        "{}",
        stdout_of(&output)
    );

    let archive = Archive::open_existing(&archive_path).expect("the archive exists");
    for path in posts {
        let url = CanonicalUrl::parse(&site.url(path)).expect("valid url");
        assert!(
            !archive
                .list_captures(&url)
                .expect("captures are listed")
                .is_empty(),
            "{path} was listed in the sitemap and never archived"
        );
    }
}

/// With no `Sitemap:` directive in `robots.txt`, `/sitemap.xml` is what is tried, which is
/// where a browser would look next.
#[test]
fn with_no_directive_the_fallback_sitemap_is_read() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let (listener, site) = Site::bind();
    let listed = vec![site.url("/posts/only")];
    Site::serve(
        listener,
        vec![
            route("/index.html", "text/html; charset=utf-8", LONE_PAGE),
            route("/robots.txt", "text/plain", "User-agent: *\nAllow: /\n"),
            route("/sitemap.xml", "application/xml", urlset(&listed)),
            route("/posts/only", "text/html; charset=utf-8", LONE_PAGE),
        ],
    );

    let output = capture_command(&archive_path, &site.url("/index.html"))
        .arg("--from-sitemap")
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));
    let archive = Archive::open_existing(&archive_path).expect("the archive exists");
    let url = CanonicalUrl::parse(&site.url("/posts/only")).expect("valid url");
    assert!(
        !archive
            .list_captures(&url)
            .expect("captures are listed")
            .is_empty(),
        "the fallback sitemap's only post was never archived"
    );
}

/// A sitemap naming a URL on another host has that URL refused, and the run still archives
/// what the rest of it lists: one address answering for a site does not get to decide what
/// this run fetches next.
#[test]
fn a_url_on_another_host_is_refused_and_the_rest_are_archived() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let (listener, site) = Site::bind();
    let listed = vec![
        site.url("/posts/a"),
        "https://elsewhere.example/stolen".to_owned(),
        site.url("/posts/b"),
    ];
    Site::serve(
        listener,
        vec![
            route("/index.html", "text/html; charset=utf-8", LONE_PAGE),
            route("/sitemap.xml", "application/xml", urlset(&listed)),
            route("/posts/a", "text/html; charset=utf-8", LONE_PAGE),
            route("/posts/b", "text/html; charset=utf-8", LONE_PAGE),
        ],
    );

    let output = capture_command(&archive_path, &site.url("/index.html"))
        .arg("--json")
        .arg("--from-sitemap")
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));
    let report: serde_json::Value =
        serde_json::from_str(&stdout_of(&output)).expect("one object and nothing else");
    assert_eq!(report["sitemap"]["urls_listed"], 3);
    assert_eq!(report["sitemap"]["urls_taken"], 2);
    assert_eq!(report["sitemap"]["urls_refused"], 1);

    let archive = Archive::open_existing(&archive_path).expect("the archive exists");
    for path in ["/posts/a", "/posts/b"] {
        let url = CanonicalUrl::parse(&site.url(path)).expect("valid url");
        assert!(
            !archive
                .list_captures(&url)
                .expect("captures are listed")
                .is_empty(),
            "{path} shared its sitemap with an off-host URL and was not archived"
        );
    }
}

/// A sitemap no parser can read leaves the run reporting that clearly, rather than the run
/// silently archiving nothing at all: the seed's own capture already happened, and that is
/// what still stands.
#[test]
fn an_unparseable_sitemap_is_warned_about_and_the_seed_is_still_archived() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let (listener, site) = Site::bind();
    Site::serve(
        listener,
        vec![
            route("/index.html", "text/html; charset=utf-8", LONE_PAGE),
            route(
                "/sitemap.xml",
                "application/xml",
                // The closing tag names something other than what it closes, which is ill
                // formed rather than merely unusual, and every reader refuses it.
                "<urlset><url><loc>https://example.com/a</loc></nested></urlset>",
            ),
        ],
    );

    let output = capture_command(&archive_path, &site.url("/index.html"))
        .arg("--from-sitemap")
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));
    assert!(
        stdout_of(&output).contains("archived 1 capture(s)"),
        "{}",
        stdout_of(&output)
    );
    assert!(
        stderr_of(&output).contains("sitemap"),
        "the unreadable sitemap was not mentioned: {}",
        stderr_of(&output)
    );

    let archive = Archive::open_existing(&archive_path).expect("the archive exists");
    let url = CanonicalUrl::parse(&site.url("/index.html")).expect("valid url");
    assert!(
        !archive
            .list_captures(&url)
            .expect("captures are listed")
            .is_empty()
    );
}

/// The run's ceiling is the run's, and `--max-depth` beside `--from-sitemap` is what makes a run
/// two phases. Driven through the binary because the defect was in the wiring rather than in
/// either phase: both of them read the budget correctly and each read it from its own zero, so a
/// run told to archive four pages archived four and then four more, and said `page ceiling` while
/// doing it.
///
/// Four is the ceiling, three are reachable by crawling (the index and the two it links) and
/// three more are listed by the sitemap and linked from nowhere. A run that honours its ceiling
/// takes exactly one of the three listed.
#[test]
fn a_ceiling_is_the_whole_run_s_and_not_each_phase_s() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let (listener, site) = Site::bind();
    let listed: Vec<String> = ["/s/1", "/s/2", "/s/3"]
        .iter()
        .map(|path| site.url(path))
        .collect();
    let mut routes = vec![
        route("/index.html", "text/html; charset=utf-8", LINKING_PAGE),
        route("/posts/a", "text/html; charset=utf-8", LONE_PAGE),
        route("/posts/b", "text/html; charset=utf-8", LONE_PAGE),
        route("/sitemap.xml", "application/xml", urlset(&listed)),
    ];
    for path in ["/s/1", "/s/2", "/s/3"] {
        routes.push(route(path, "text/html; charset=utf-8", LONE_PAGE));
    }
    Site::serve(listener, routes);

    let output = archeion()
        .arg("capture")
        .arg(&archive_path)
        .arg(site.url("/index.html"))
        .args(["--max-pages", "4", "--max-depth", "1"])
        // Four, like the rest of this file: a crawl at a concurrency of one loses a link inside
        // the engine's own frontier, which is a different defect with its own guard and would
        // fail this run before it ever reached the ceiling.
        .args(["--concurrency", "4", "--max-retries", "0"])
        .args(["--deadline", "30s", "--allow-private-addresses"])
        .args(["--from-sitemap", &site.url("/sitemap.xml")])
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));
    assert!(
        stdout_of(&output).contains("archived 4 capture(s)"),
        "the ceiling of four was spent across both phases, not once per phase: {}",
        stdout_of(&output)
    );
}

/// The ceiling reaching the engine, which the test above does not prove: its listed pages link
/// nowhere, so every sub-crawl archives one page whatever budget it was handed. Here a listed
/// page links three more, so the number on the sub-crawl's seed is what decides.
///
/// Four is the ceiling, three go to the crawl (the index and the two it links), and one is left
/// when the sitemap phase starts. A sub-crawl handed the whole ceiling again takes `/s/1` and
/// all three of its children, for seven.
#[test]
fn a_sub_crawl_from_a_listed_url_is_bounded_by_what_the_run_has_left() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let (listener, site) = Site::bind();
    let branching = "<html><head><title>A listed page</title></head><body><p>Three more: \
         <a href=\"/s/1a\">a</a> <a href=\"/s/1b\">b</a> <a href=\"/s/1c\">c</a></p></body></html>";
    let mut routes = vec![
        route("/index.html", "text/html; charset=utf-8", LINKING_PAGE),
        route("/posts/a", "text/html; charset=utf-8", LONE_PAGE),
        route("/posts/b", "text/html; charset=utf-8", LONE_PAGE),
        route("/s/1", "text/html; charset=utf-8", branching),
        route(
            "/sitemap.xml",
            "application/xml",
            urlset(&[site.url("/s/1")]),
        ),
    ];
    for path in ["/s/1a", "/s/1b", "/s/1c"] {
        routes.push(route(path, "text/html; charset=utf-8", LONE_PAGE));
    }
    Site::serve(listener, routes);

    let output = archeion()
        .arg("capture")
        .arg(&archive_path)
        .arg(site.url("/index.html"))
        .args(["--max-pages", "4", "--max-depth", "1"])
        .args(["--concurrency", "4", "--max-retries", "0"])
        .args(["--deadline", "30s", "--allow-private-addresses"])
        .args(["--from-sitemap", &site.url("/sitemap.xml")])
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));
    assert!(
        stdout_of(&output).contains("archived 4 capture(s)"),
        "the sub-crawl was handed the run's leftover page and not the whole ceiling: {}",
        stdout_of(&output)
    );
}

/// A run whose crawl filled the ceiling exactly has nothing the listing can add, and reading one
/// costs the host a request that archives nothing. Visible in the report: a phase that never ran
/// prints no sitemap row.
#[test]
fn a_run_with_nothing_left_to_spend_does_not_ask_for_a_sitemap() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let (listener, site) = Site::bind();
    let listed: Vec<String> = ["/s/1", "/s/2", "/s/3"]
        .iter()
        .map(|path| site.url(path))
        .collect();
    let mut routes = vec![
        route("/index.html", "text/html; charset=utf-8", LINKING_PAGE),
        route("/posts/a", "text/html; charset=utf-8", LONE_PAGE),
        route("/posts/b", "text/html; charset=utf-8", LONE_PAGE),
        route("/sitemap.xml", "application/xml", urlset(&listed)),
    ];
    for path in ["/s/1", "/s/2", "/s/3"] {
        routes.push(route(path, "text/html; charset=utf-8", LONE_PAGE));
    }
    Site::serve(listener, routes);

    let output = archeion()
        .arg("capture")
        .arg(&archive_path)
        .arg(site.url("/index.html"))
        .args(["--max-pages", "3", "--max-depth", "1"])
        .args(["--concurrency", "4", "--max-retries", "0"])
        .args(["--deadline", "30s", "--allow-private-addresses"])
        .args(["--from-sitemap", &site.url("/sitemap.xml")])
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));
    assert!(
        stdout_of(&output).contains("archived 3 capture(s)"),
        "{}",
        stdout_of(&output)
    );
    assert!(
        !stdout_of(&output).contains("sitemap"),
        "the listing was read by a run that could not archive a page of it: {}",
        stdout_of(&output)
    );
}
