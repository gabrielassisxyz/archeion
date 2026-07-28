//! `archeion capture` driven as the command it is: a process, its output, and its exit code.
//!
//! One test crawls a server this file starts on loopback. It opens a socket for the reason
//! the other two socket tests in this project do, which is that what it covers has no
//! reachable entry point: the binary builds the real engine, so nothing a scripted engine
//! can prove says whether the verb archives anything at all. Nothing here leaves the
//! machine, needs setup, or answers differently tomorrow.
//!
//! The rest never opens one. A seed pointed inside a network is refused before anything is
//! dialled, and a closed port on loopback is a fetch that reached no server, which is the
//! difference the exit codes are built around: the archive being wrong is a failure, the web
//! being the web is not.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output};
use std::thread;

use archeion::CanonicalUrl;
use archeion::storage::Archive;
use tempfile::TempDir;

const INDEX: &str = r#"<html><head><title>An index</title></head>
    <body><ul><li><a href="/article.html">the article</a></li></ul></body></html>"#;
const STYLESHEET: &[u8] = b"body { color: rebeccapurple }";

fn article_page() -> String {
    format!(
        r#"<html><head><title>Bread</title>
        <link rel="stylesheet" href="/style.css"></head>
        <body><nav><a href="/index.html">home</a></nav><article>{}</article></body></html>"#,
        "<p>Bread is mostly patience, and the dough will tell you when it is ready.</p>".repeat(8)
    )
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

/// The whole verb, from a command line to records on disk. The counts are asserted as the
/// report prints them, because the report is the only thing a person running this sees and a
/// number that stops being true silently is the failure the report exists to prevent.
#[test]
fn a_seed_is_crawled_into_an_archive_that_the_run_creates() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let site = Site::start();

    let output = archeion()
        .arg("capture")
        .arg(&archive_path)
        .arg(site.url("/index.html"))
        .args([
            "--max-pages",
            "4",
            "--concurrency",
            "1",
            "--max-retries",
            "0",
        ])
        .args(["--deadline", "30s", "--allow-private-addresses"])
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(stderr_of(&output), "");
    assert_eq!(
        stdout_of(&output),
        format!(
            "created an archive at {archive}\n\
             archived 2 capture(s) from {seed} into {archive}\n  \
             articles      1 extracted, 0 refused\n  \
             assets        1 stored, 0 missed, 1 request(s)\n  \
             pages dropped 0\n  \
             stopped       nothing was left to fetch\n",
            archive = archive_path.display(),
            seed = site.url("/index.html"),
        )
    );

    // The report is a claim about the archive, so the archive is what settles it.
    let archive = Archive::open_existing(&archive_path).expect("the archive exists");
    let url = CanonicalUrl::parse(&site.url("/article.html")).expect("valid url");
    let captures = archive.list_captures(&url).expect("captures are listed");
    let article = archive
        .read_article(&url, &captures[0])
        .expect("the prose is stored")
        .expect("the article page produced prose");
    assert!(article.markdown.contains("Bread is mostly patience"));
}

/// A run creating the archive is the only way to get a first one, and a path typed wrong is
/// the price of that. It is paid with a line saying what happened rather than in silence.
#[test]
fn an_archive_that_already_exists_is_not_reported_as_created() {
    let dir = TempDir::new().expect("temp dir");
    Archive::open(dir.path()).expect("the archive is created up front");

    let output = capture_from_a_closed_port(dir.path());

    assert!(output.status.success(), "{}", stderr_of(&output));
    assert!(
        !stdout_of(&output).contains("created an archive"),
        "{}",
        stdout_of(&output)
    );
}

/// A URL nobody answered is the web, not a damaged archive. It is reported on stderr and the
/// run still leaves with a zero, which is what keeps a pipeline from stopping on a dead link.
#[test]
fn a_url_that_answered_nothing_is_warned_about_and_is_not_a_failure() {
    let dir = TempDir::new().expect("temp dir");

    let output = capture_from_a_closed_port(dir.path());

    assert!(output.status.success(), "{}", stderr_of(&output));
    assert!(
        stdout_of(&output).contains("archived 0 capture(s)"),
        "{}",
        stdout_of(&output)
    );
    assert!(
        stderr_of(&output).starts_with("warning: no response from http://127.0.0.1:"),
        "{}",
        stderr_of(&output)
    );
}

/// The guard that has to hold before anything is dialled. The seed names loopback and the run
/// did not ask for it, so there is no crawl, no archive written, and a code a script can read.
#[test]
fn a_seed_pointed_inside_a_network_is_refused_before_anything_is_fetched() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");

    let output = archeion()
        .arg("capture")
        .arg(&archive_path)
        .arg("http://169.254.169.254/latest/meta-data/")
        .output()
        .expect("the binary runs");

    assert!(!output.status.success());
    assert_eq!(
        stderr_of(&output),
        "http://169.254.169.254/latest/meta-data/ cannot be crawled: \
         169.254.169.254 is inside a network rather than on the web\n"
    );
    assert_eq!(stdout_of(&output), "");
}

#[test]
fn a_path_holding_something_else_is_refused_rather_than_archived_into() {
    let dir = TempDir::new().expect("temp dir");
    std::fs::write(dir.path().join("notes.txt"), b"a directory of my own")
        .expect("the path holds something else");

    let output = archeion()
        .arg("capture")
        .arg(dir.path())
        .arg("https://example.com/")
        .output()
        .expect("the binary runs");

    assert!(!output.status.success());
    assert_eq!(
        stderr_of(&output),
        format!(
            "{} holds something else, not an Archeion archive\n",
            dir.path().display()
        )
    );
}

#[test]
fn the_run_reports_itself_as_one_json_object() {
    let dir = TempDir::new().expect("temp dir");
    let site = Site::start();

    let output = archeion()
        .arg("capture")
        .arg("--json")
        .arg(dir.path())
        .arg(site.url("/index.html"))
        .args([
            "--max-pages",
            "1",
            "--concurrency",
            "1",
            "--max-retries",
            "0",
        ])
        .args(["--deadline", "30s", "--allow-private-addresses"])
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));
    let report: serde_json::Value =
        serde_json::from_str(&stdout_of(&output)).expect("one object and nothing else");
    assert_eq!(report["seed_url"], site.url("/index.html").as_str());
    assert_eq!(report["archive"], dir.path().display().to_string().as_str());
    // An empty directory holds no archive, so this run brought one into existence too.
    assert_eq!(report["archive_created"], true);
    assert_eq!(report["captures_written"], 1);
    assert_eq!(report["stopped"], "exhausted");
    assert_eq!(report["failed_fetches"], serde_json::json!([]));
}

/// A port nothing is listening on, which is a fetch that reached no server without a socket
/// ever being served. The archive path is handed in so the same run can answer two questions.
fn capture_from_a_closed_port(archive: &std::path::Path) -> Output {
    let closed = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = closed.local_addr().expect("the bound address").port();
    drop(closed);

    archeion()
        .arg("capture")
        .arg(archive)
        .arg(format!("http://127.0.0.1:{port}/index.html"))
        .args(["--max-retries", "0", "--deadline", "30s"])
        .arg("--allow-private-addresses")
        .output()
        .expect("the binary runs")
}

/// A site on loopback: an index that links to an article, and a stylesheet only the article
/// needs, so the counts in the report say which capture the subresource belonged to.
struct Site {
    port: u16,
}

impl Site {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let port = listener.local_addr().expect("the bound address").port();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = answer(stream);
            }
        });
        Self { port }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

fn answer(mut stream: TcpStream) -> std::io::Result<()> {
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
    let article = article_page();
    let (status, media_type, body): (&str, &str, &[u8]) = match path.as_str() {
        // A 404 is the answer that permits every path, and the crawl asks for this first.
        "/robots.txt" => ("404 Not Found", "text/plain", b""),
        "/index.html" => ("200 OK", "text/html; charset=utf-8", INDEX.as_bytes()),
        "/article.html" => ("200 OK", "text/html; charset=utf-8", article.as_bytes()),
        "/style.css" => ("200 OK", "text/css", STYLESHEET),
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
