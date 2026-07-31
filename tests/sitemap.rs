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

fn capture_command(archive_path: &std::path::Path, seed_url: &str) -> Command {
    capture_command_with_max_pages(archive_path, seed_url, 10)
}

fn capture_command_with_max_pages(
    archive_path: &std::path::Path,
    seed_url: &str,
    max_pages: usize,
) -> Command {
    let mut command = archeion();
    command
        .arg("capture")
        .arg(archive_path)
        .arg(seed_url)
        .args([
            "--max-pages",
            &max_pages.to_string(),
            "--concurrency",
            "4",
            "--max-retries",
            "0",
        ])
        .args(["--deadline", "30s", "--allow-private-addresses"]);
    command
}

/// A crawl followed by a sitemap is one run. The first phase used to archive its full page
/// allowance and the second phase then started the same allowance over from zero.
#[test]
fn a_sitemap_phase_uses_only_the_page_count_the_crawl_left() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let (listener, site) = Site::bind();
    let listed = vec![site.url("/sitemap-only")];
    let seed_page = format!(
        "<html><body><a href=\"{}\">linked</a></body></html>",
        site.url("/linked")
    );
    Site::serve(
        listener,
        vec![
            route("/index.html", "text/html; charset=utf-8", seed_page),
            route("/linked", "text/html; charset=utf-8", LONE_PAGE),
            route("/sitemap.xml", "application/xml", urlset(&listed)),
            route("/sitemap-only", "text/html; charset=utf-8", LONE_PAGE),
        ],
    );

    let output = capture_command_with_max_pages(&archive_path, &site.url("/index.html"), 2)
        .args([
            "--from-sitemap",
            &site.url("/sitemap.xml"),
            "--max-depth",
            "1",
        ])
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));
    assert!(
        stdout_of(&output).contains("archived 2 capture(s)"),
        "{}",
        stdout_of(&output)
    );
    assert!(
        stdout_of(&output).contains("sitemap       1 taken, 0 refused, 1 listed"),
        "{}",
        stdout_of(&output)
    );
    assert!(
        stdout_of(&output).contains("stopped       the run's page limit was reached"),
        "{}",
        stdout_of(&output)
    );
    let archive = Archive::open_existing(&archive_path).expect("the archive exists");
    let sitemap_only = CanonicalUrl::parse(&listed[0]).expect("valid url");
    assert!(
        archive
            .list_captures(&sitemap_only)
            .expect("captures are listed")
            .is_empty(),
        "the sitemap phase exceeded the run's page count"
    );
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
