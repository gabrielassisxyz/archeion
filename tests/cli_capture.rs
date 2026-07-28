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
use std::time::Duration;

use archeion::CanonicalUrl;
use archeion::storage::Archive;
use tempfile::TempDir;

const INDEX: &str = r#"<html><head><title>An index</title></head>
    <body><ul><li><a href="/article.html">the article</a></li></ul></body></html>"#;
const STYLESHEET: &[u8] = b"body { color: rebeccapurple }";
/// An index whose entry is served as Markdown beside nothing else, which is the shape the
/// `llms.txt` convention produces and the one a real capture found.
const MARKDOWN_INDEX: &str = r#"<html><head><title>An index</title></head>
    <body><ul><li><a href="/post.md">the post</a></li></ul></body></html>"#;
const POST_MARKDOWN: &[u8] = b"# The oven is fixed\n\nThe element went in this morning.\n";
/// Two links one hop from the seed, one and two path segments deep, plus a third link two
/// hops away through the deeper of the two. A depth budget counting path segments instead
/// of hops takes the first sibling and refuses the second at the same distance; one that
/// counts hops takes both and still stops before the page two hops out.
/// The post is linked with a fragment on purpose. The engine drops one before it queues a
/// link, so the page comes back identified without it, and a depth map keyed on the
/// characters the page wrote would fail to place the page it just queued itself.
const DEPTH_INDEX: &str = r#"<html><head><title>Section</title></head>
    <body><ul>
        <li><a href="/shallow">shallow</a></li>
        <li><a href="/p/deep-post#top">a post</a></li>
    </ul></body></html>"#;
const SHALLOW_PAGE: &str =
    "<html><head><title>Shallow</title></head><body>nothing further</body></html>";
const DEEP_POST_PAGE: &str = r#"<html><head><title>A post</title></head>
    <body><a href="/p/nested/too-deep">further in</a></body></html>"#;
const TOO_DEEP_PAGE: &str =
    "<html><head><title>Too deep</title></head><body>two hops from the seed</body></html>";

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

/// A page the site published as Markdown, crawled the way one is really reached: through a
/// link on an ordinary HTML index.
///
/// It opens a socket for the reason the test above does, and it is the only thing that can
/// answer the question the scripted engines cannot. Whether the crawl engine follows a link to
/// a document that is not markup, and hands the response over as a page rather than dropping
/// it, is a property of the engine and its configuration: it compiles either way, and every
/// test that builds its own events would pass either way.
#[test]
fn a_post_the_site_serves_as_markdown_is_archived_as_the_article_it_already_is() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let site = Site::start();

    let output = archeion()
        .arg("capture")
        .arg(&archive_path)
        .arg(site.url("/markdown-index.html"))
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
    assert!(
        stdout_of(&output).contains("articles      1 extracted"),
        "{}",
        stdout_of(&output)
    );

    let archive = Archive::open_existing(&archive_path).expect("the archive exists");
    let url = CanonicalUrl::parse(&site.url("/post.md")).expect("valid url");
    let captures = archive.list_captures(&url).expect("captures are listed");
    let article = archive
        .read_article(&url, &captures[0])
        .expect("the prose is stored")
        .expect("the served document is the article");
    assert!(
        article
            .markdown
            .contains("The element went in this morning")
    );
    // The record says the site published this rather than that anything scored it, which is
    // what a reader comparing two articles has to be able to tell.
    assert_eq!(
        article.record.rules,
        archeion::readability::ExtractionRules::Served
    );
}

/// `--max-depth` bounds hops from the seed, not path segments of the URL: a sibling two
/// segments deep is taken at the same distance as one segment deep, and a page genuinely
/// two hops out is still refused. Measured on a real site this looked like a publication
/// with section pages and no posts, because every post lived one path segment deeper than
/// its section.
#[test]
fn a_max_depth_of_one_takes_every_link_one_hop_from_the_seed_and_no_further() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let site = Site::start();

    let output = archeion()
        .arg("capture")
        .arg(&archive_path)
        .arg(site.url("/depth-index.html"))
        .args([
            "--max-pages",
            "10",
            "--max-depth",
            "1",
            // More than one in flight: a page with two sibling links crawled at
            // concurrency one loses one of them to a pre-existing scheduling defect
            // in the engine that has nothing to do with depth, and would make this
            // test flaky for a reason it is not the one asserting on.
            "--concurrency",
            "4",
            "--max-retries",
            "0",
        ])
        .args(["--deadline", "30s", "--allow-private-addresses"])
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));
    assert!(
        stdout_of(&output).contains("archived 3 capture(s)"),
        "{}",
        stdout_of(&output)
    );

    let archive = Archive::open_existing(&archive_path).expect("the archive exists");
    for path in ["/shallow", "/p/deep-post"] {
        let url = CanonicalUrl::parse(&site.url(path)).expect("valid url");
        assert!(
            !archive
                .list_captures(&url)
                .expect("captures are listed")
                .is_empty(),
            "{path} is one hop from the seed and was not archived at a depth of one"
        );
    }

    let too_deep = CanonicalUrl::parse(&site.url("/p/nested/too-deep")).expect("valid url");
    assert!(
        archive
            .list_captures(&too_deep)
            .expect("captures are listed")
            .is_empty(),
        "a page two hops from the seed was archived at a depth of one"
    );
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
    // Opening an archive is what creates one, so a seed screened afterwards would leave an
    // empty collection on a path the run never had a reason to touch, and the line that
    // announces a new archive would not have been printed to say so.
    assert!(
        !archive_path.exists(),
        "a run that fetched nothing left an archive behind"
    );
}

/// The refusal above has to come from the seed and not from the archive, or a run refused for
/// its seed while pointed at a valid collection would still be reported as something else.
#[test]
fn a_refused_seed_is_refused_even_when_the_archive_is_fine() {
    let dir = TempDir::new().expect("temp dir");
    Archive::open(dir.path()).expect("the archive is created up front");

    let output = archeion()
        .arg("capture")
        .arg(dir.path())
        .arg("file:///etc/passwd")
        .output()
        .expect("the binary runs");

    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("file is not a scheme this archive fetches"),
        "{}",
        stderr_of(&output)
    );
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

/// A fetch that reaches no server, without a socket ever being opened for it.
///
/// The address is port 1 rather than an ephemeral port this test bound and released. That
/// dance leaves a window in which anything on the machine, including the other tests in this
/// file, can take the port back and answer, and the failure it produces then looks like the
/// warning being wrong rather than like a port being reused. Port 1 needs privileges nothing
/// in this suite has, so nothing can move into it.
fn capture_from_a_closed_port(archive: &std::path::Path) -> Output {
    archeion()
        .arg("capture")
        .arg(archive)
        .arg("http://127.0.0.1:1/index.html")
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
                // One thread per connection, because the client keeps a pool of them: a
                // server that answered them in turn would hold every later request behind
                // whichever connection was opened first and left idle, and the run would
                // then produce nothing until its deadline rather than fail.
                thread::spawn(move || answer(stream));
            }
        });
        Self { port }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

fn answer(mut stream: TcpStream) -> std::io::Result<()> {
    // A connection that goes quiet mid-request gives up its thread instead of holding it for
    // as long as the client feels like keeping it open.
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
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
        "/markdown-index.html" => (
            "200 OK",
            "text/html; charset=utf-8",
            MARKDOWN_INDEX.as_bytes(),
        ),
        "/post.md" => ("200 OK", "text/markdown; charset=utf-8", POST_MARKDOWN),
        "/depth-index.html" => ("200 OK", "text/html; charset=utf-8", DEPTH_INDEX.as_bytes()),
        "/shallow" => (
            "200 OK",
            "text/html; charset=utf-8",
            SHALLOW_PAGE.as_bytes(),
        ),
        "/p/deep-post" => (
            "200 OK",
            "text/html; charset=utf-8",
            DEEP_POST_PAGE.as_bytes(),
        ),
        "/p/nested/too-deep" => (
            "200 OK",
            "text/html; charset=utf-8",
            TOO_DEEP_PAGE.as_bytes(),
        ),
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
