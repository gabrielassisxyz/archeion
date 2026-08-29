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
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use archeion::CanonicalUrl;
use archeion::crawl::DEFAULT_USER_AGENT;
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
/// Two hrefs spelled the way pages in the wild spell them: one with the HTML entity a
/// browser decodes before it ever reaches the network, `&amp;` rather than a literal `&`,
/// one with a non-ASCII character already percent-encoded rather than written literally.
/// The guard's own comparison keys one side on the href as the page wrote it and the other
/// on whatever the engine actually requested; if the engine's link extraction and this
/// project's own resolution of the same href ever disagreed on which of the two that is,
/// either link would be reported lost despite being archived.
const ENTITY_INDEX: &str = r#"<html><head><title>Entity link</title></head>
    <body><a href="/entity-target?x=1&amp;y=2">read more</a>
    <a href="/caf%C3%A9">percent encoded</a></body></html>"#;
const ENTITY_TARGET_PAGE: &str =
    "<html><head><title>Target</title></head><body>found through the entity</body></html>";
const OTHER_SCHEME_TARGET_PAGE: &str =
    "<html><head><title>Other scheme</title></head><body>reached anyway</body></html>";
/// A page one hop deeper than `/base-href-index.html`, and the one that actually declares
/// the tag: `<base href>` only changes what the page's own links resolve against, so it has
/// no effect written on the index that merely links here.
const INTRO_PAGE: &str =
    "<html><head><title>Intro</title></head><body>reached through the rewritten base</body></html>";
const FTP_TARGET_PAGE: &str =
    "<html><head><title>Reached over http</title></head><body>the ordinary link</body></html>";

/// An absolute self link spelled in the other scheme from the one the seed was typed with,
/// which is ordinary on real sites and is exactly what `push_link` in the dependency
/// rewrites to the seed's own scheme before the link ever reaches its frontier. The engine
/// still fetches this over plain HTTP, since `answer` never looks at the scheme a request
/// claimed to have come through, only at the path.
fn other_scheme_index(port: u16) -> String {
    format!(
        r#"<html><head><title>Other scheme</title></head>
        <body><a href="https://127.0.0.1:{port}/other-scheme-target">the other scheme</a></body></html>"#
    )
}

/// A page one hop from the seed that declares an absolute `<base href>` and then links
/// relatively against it, which resolves to the site's root rather than to a sibling of this
/// page's own path. `hop_depth_guard` has no way to read `<base href>` back out of `Page`
/// without a second HTML pass of its own, which is the open question this bead's tests pin
/// the chosen answer to.
fn base_href_guide_page(port: u16) -> String {
    format!(
        r#"<html><head><title>Guide</title>
        <base href="http://127.0.0.1:{port}/"></head>
        <body><a href="intro.html">intro</a></body></html>"#
    )
}

const BASE_HREF_INDEX: &str = r#"<html><head><title>Base href</title></head>
    <body><a href="/docs/guide.html">the guide</a></body></html>"#;

/// A same-host link in a scheme the engine will never dial. `validate_link` in the
/// dependency drops anything that is not `http` or `https` before it ever reaches the
/// frontier, so nothing here has to answer an FTP request for the guard to be exercised: the
/// question is only whether this project's own bookkeeping learns not to expect one either.
const FTP_SCHEME_INDEX: &str = r#"<html><head><title>FTP link</title></head>
    <body><a href="/ftp-target">the ordinary link</a>
    <a href="ftp://127.0.0.1:1/pub/x">a link the engine will not dial</a></body></html>"#;

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
             host refused  none\n  \
             articles      1 extracted, 0 refused\n  \
             assets        1 stored, 0 missed, 1 request(s)\n  \
             pages dropped 0\n  \
             links lost    0\n  \
             recovered     0\n  \
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

/// The scheduling defect the test above works around, pinned directly rather than dodged.
/// A page with two sibling links crawled at a concurrency of one still loses one of them to
/// the vendored crawl engine's own frontier about half the time; `docs/crawl-boundary.md`
/// has the mechanism. What no longer follows from that is a failed run: `recover_lost_links`
/// fetches directly whatever this project's own robots decision would have let through and
/// the frontier never asked the site for, so every run below leaves with a clean exit code
/// holding every page, whether or not the race actually struck on that particular run.
/// Thirty runs, at a coin flip this race resolved to about half the time before recovery
/// existed, are what keep a version of the recovery that stopped fetching anything from
/// passing by accident: the assertion inside the loop is where that version would fail, not
/// a count taken afterward. Each run is three tiny pages over loopback, so the whole test
/// stays well inside the budget of `cargo test`.
///
/// This site's own `/robots.txt` answers 404, which the engine reads as permission to fetch
/// everything, so this exercises the recovery on its own, with no rule anywhere near the
/// decision. `a_link_the_named_group_leaves_unmentioned_is_not_lost_to_the_frontier_race`
/// covers the other reason a link reaches `recover_lost_links`: a `robots.txt` naming the
/// identity a run is using, which removes the `Disallow` that would otherwise keep this same
/// race from reaching every single run.
///
/// `links_recovered`, read off `--json`, is the self-check the loop needs now that a link
/// the race drops no longer produces a visible symptom on its own: a version of this test
/// that never actually reached the race, because a vendor bump quietly closed it, would
/// still see every page archived by the ordinary frontier and would pass while proving
/// nothing. Summed across the thirty attempts rather than asserted per attempt, since which
/// runs the race strikes on is exactly the coin flip this loop exists to ride out.
#[test]
fn a_concurrency_of_one_recovers_a_link_the_frontier_never_queued() {
    let site = Site::start();
    let mut links_recovered_total = 0u64;

    for _ in 0..30 {
        let dir = TempDir::new().expect("temp dir");
        let archive_path = dir.path().join("collection");

        let output = archeion()
            .arg("capture")
            .arg("--json")
            .arg(&archive_path)
            .arg(site.url("/depth-index.html"))
            .args([
                "--max-pages",
                "10",
                "--max-depth",
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

        let archive = Archive::open_existing(&archive_path).expect("the archive exists");
        let captures_of = |path: &str| {
            let url = CanonicalUrl::parse(&site.url(path)).expect("valid url");
            archive
                .list_captures(&url)
                .expect("captures are listed")
                .len()
        };
        let archived = captures_of("/depth-index.html")
            + captures_of("/shallow")
            + captures_of("/p/deep-post");

        assert_eq!(
            archived,
            3,
            "a run that exited clean was holding fewer pages than it discovered: {}",
            stdout_of(&output)
        );

        let report: serde_json::Value =
            serde_json::from_str(&stdout_of(&output)).expect("one object and nothing else");
        links_recovered_total += report["links_recovered"].as_u64().expect("a count");
    }

    assert!(
        links_recovered_total > 0,
        "thirty runs at a concurrency of one never asked recover_lost_links for a single link, so this loop never exercised the race it exists to ride out"
    );
}

/// A recovered page is not a leaf: `record_discovered_links` folds its own outbound links
/// into the same `--max-depth` bookkeeping a page the crawl queued itself would have gone
/// through. This site's seed links two siblings, `/reachable` and `/half-open`, and only
/// `/half-open` itself links further, to a second port on the same loopback host with
/// nothing listening on it, a connection this project's own resolution reads as same-host
/// (a scheme and a host, never a port) and is therefore recorded and pursued exactly like
/// any other in-scope link.
///
/// Whichever sibling the frontier's own race drops at a concurrency of one, the run below
/// still holds `/reachable`: recovered directly if the race dropped it, fetched by the
/// ordinary frontier if it did not. What differs is `/half-open`'s own child. If the race
/// spared `/half-open`, the ordinary frontier discovers and fails to fetch the unreachable
/// address itself, an everyday failed fetch that does not fail the run. If the race dropped
/// `/half-open` instead, recovery fetches it directly, discovers the same address from its
/// freshly-fetched body, and now has to answer for it itself: a connection nothing accepts
/// is not recoverable, so it lands in `still_missing`, the run exits non-zero, and stderr
/// names it, restoring the failure path a version of recovery that never retried, paced or
/// bounded itself would have no way to reach.
///
/// Twenty attempts are what let both branches show up: the two failure-path assertions
/// only run on an attempt where the race actually dropped `/half-open`, which happens on
/// roughly half of them.
#[test]
fn a_recovered_page_s_own_child_is_fetched_or_reported_never_silently_dropped() {
    let dir = TempDir::new().expect("temp dir");
    // Closed the instant it is bound, so every fetch aimed at this port for the rest of the
    // test finds nothing listening and is refused rather than merely slow.
    let closed_port = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        listener.local_addr().expect("the bound address").port()
    };
    let port = serve_a_site_with_a_half_open_grandchild(closed_port);
    let reachable_url =
        CanonicalUrl::parse(&format!("http://127.0.0.1:{port}/reachable")).expect("valid url");
    let half_open_url =
        CanonicalUrl::parse(&format!("http://127.0.0.1:{port}/half-open")).expect("valid url");
    let unreachable_url = format!("http://127.0.0.1:{closed_port}/unreachable");
    let mut saw_the_failure_path = false;

    for attempt in 0..20 {
        let seed = format!("http://127.0.0.1:{port}/");
        let archive_path = dir.path().join(format!("attempt-{attempt}"));
        let output = archeion()
            .arg("capture")
            .arg("--json")
            .arg(&archive_path)
            .arg(&seed)
            .args([
                "--max-pages",
                "10",
                "--max-depth",
                "2",
                "--concurrency",
                "1",
                "--max-retries",
                "0",
                "--deadline",
                "20s",
                "--allow-private-addresses",
            ])
            .output()
            .expect("the binary runs");

        let archive = Archive::open_existing(&archive_path).expect("the archive exists");
        assert!(
            !archive
                .list_captures(&reachable_url)
                .expect("captures are listed")
                .is_empty(),
            "attempt {attempt}: the sibling with no children of its own was not archived"
        );

        let report: serde_json::Value =
            serde_json::from_str(&stdout_of(&output)).expect("one object and nothing else");
        // Whichever of the two branches below this attempt lands in, the run has to have
        // actually discovered the unreachable address at all: named as a plain failed
        // fetch when the ordinary frontier reached `/half-open` itself, or named as a lost
        // link when recovery did and could not reach its child. A version that silently
        // never discovered the child in the first place, the failure `--max-depth`
        // expansion inside recovery exists to close, would leave it out of both and pass
        // neither assertion below, so this is checked before either branch runs at all.
        let named_as_a_failed_fetch = report["failed_fetches"]
            .as_array()
            .expect("a list")
            .iter()
            .any(|failure| failure["url"] == unreachable_url);
        let named_as_a_lost_link = report["links_never_followed"]
            .as_array()
            .expect("a list")
            .iter()
            .any(|url| url == &unreachable_url);
        assert!(
            named_as_a_failed_fetch || named_as_a_lost_link,
            "attempt {attempt}: the unreachable grandchild was never discovered at all: {}",
            stdout_of(&output)
        );

        if output.status.success() {
            // The race spared `/half-open`, or recovered it with nothing left over: either
            // way the ordinary run holds it, and its own child is this run's problem to
            // answer for, not a silent gap.
            assert!(
                !archive
                    .list_captures(&half_open_url)
                    .expect("captures are listed")
                    .is_empty(),
                "attempt {attempt}: a clean exit did not hold the page with a child of its own"
            );
        } else {
            saw_the_failure_path = true;
            assert!(
                stderr_of(&output).contains(&format!(
                    "{unreachable_url} was discovered and never fetched"
                )),
                "attempt {attempt}: {}",
                stderr_of(&output)
            );
            assert!(
                stderr_of(&output).contains("1 link(s) the crawl discovered were never fetched"),
                "attempt {attempt}: {}",
                stderr_of(&output)
            );
        }
    }

    assert!(
        saw_the_failure_path,
        "twenty attempts never once had the race drop the page whose own child recovery \
         cannot reach, so the restored failure path was never exercised"
    );
}

fn serve_a_site_with_a_half_open_grandchild(closed_port: u16) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the bound address").port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || answer_with_a_half_open_grandchild(stream, closed_port));
        }
    });
    port
}

fn answer_with_a_half_open_grandchild(
    mut stream: TcpStream,
    closed_port: u16,
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
    let root_page = r#"<html><head><title>Root</title></head><body>
        <a href="/reachable">a leaf</a>
        <a href="/half-open">a page with its own child</a>
        </body></html>"#
        .to_owned();
    let half_open_page = format!(
        r#"<html><head><title>Half open</title></head><body>
        <a href="http://127.0.0.1:{closed_port}/unreachable">nothing answers here</a>
        </body></html>"#
    );
    let (media_type, body): (&str, String) = match path.as_str() {
        "/" => ("text/html; charset=utf-8", root_page),
        "/reachable" => (
            "text/html; charset=utf-8",
            "<html><head><title>Reachable</title></head><body>a leaf</body></html>".to_owned(),
        ),
        "/half-open" => ("text/html; charset=utf-8", half_open_page),
        "/robots.txt" => ("text/plain", String::new()),
        _ => ("text/plain", "not here".to_owned()),
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {media_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

/// `--max-pages` is the run's, not a fresh budget recovery hands itself: three siblings are
/// governed by an `Allow` longer than the `Disallow` above it, so the vendored engine's own
/// first-match rule refuses all three at the frontier while this project's own longest-match
/// reading of RFC 9309 allows every one of them, deterministically, on every single run, with
/// no race and no coin flip anywhere in this test. `--max-pages 2` leaves the seed one page
/// of budget once it is spent on the seed itself, and `recover_lost_links` checks that
/// budget before every fetch rather than once for the batch of three, so it stops after the
/// first and never touches the other two.
#[test]
fn recovery_stops_at_the_page_ceiling_instead_of_spending_the_whole_batch() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let port = serve_a_site_whose_allow_outranks_a_shorter_disallow();
    let seed = format!("http://127.0.0.1:{port}/");

    let output = archeion()
        .arg("capture")
        .arg("--json")
        .arg(&archive_path)
        .arg(&seed)
        .args([
            "--max-pages",
            "2",
            "--max-depth",
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
    assert_eq!(
        report["captures_written"],
        2,
        "the run held more captures than its own page ceiling allowed: {}",
        stdout_of(&output)
    );
    assert_eq!(
        report["links_recovered"],
        1,
        "recovery did not stop at the one page the ceiling left it: {}",
        stdout_of(&output)
    );
    // A link the page ceiling left behind is the budget working as asked, not a loss:
    // `links_discovered_but_never_fetched` already excludes it on the same reasoning for
    // the crawl phase, and recovery's own per-candidate check exists to keep that true here.
    assert_eq!(
        report["links_never_followed"],
        serde_json::json!([]),
        "a page the ceiling left behind was reported as lost rather than left silent: {}",
        stdout_of(&output)
    );

    let archive = Archive::open_existing(&archive_path).expect("the archive exists");
    let captures_of = |path: &str| {
        let url =
            CanonicalUrl::parse(&format!("http://127.0.0.1:{port}{path}")).expect("valid url");
        archive
            .list_captures(&url)
            .expect("captures are listed")
            .len()
    };
    // Sorted order is what `links_discovered_but_never_fetched` already hands recovery, so
    // `/p/keep1` is deterministically the one candidate the remaining page went to.
    assert!(
        captures_of("/p/keep1") > 0,
        "the one page the ceiling allowed was not archived"
    );
    assert_eq!(
        captures_of("/p/keep2") + captures_of("/p/keep3"),
        0,
        "recovery spent the ceiling on more than the one page it had left"
    );
}

/// A `robots.txt` naming this run's own identity, per arch-ugh, is the polite site this
/// mechanism exists for, and pacing is the thing a polite site is owed back. `--delay` is
/// honoured between the crawl's own requests already; this pins that recovery honours it
/// too, over the same three siblings `recovery_stops_at_the_page_ceiling_instead_of_spending_the_whole_batch`
/// uses so the entry into `recover_lost_links` is deterministic rather than raced.
/// `--max-pages` is left high enough for all three, so the whole measured wall clock is the
/// three waits recovery owes and whatever the loopback round trips themselves cost, which is
/// negligible beside three hundred milliseconds.
#[test]
fn recovery_waits_the_configured_delay_between_its_own_fetches() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let port = serve_a_site_whose_allow_outranks_a_shorter_disallow();
    let seed = format!("http://127.0.0.1:{port}/");

    let started = std::time::Instant::now();
    let output = archeion()
        .arg("capture")
        .arg("--json")
        .arg(&archive_path)
        .arg(&seed)
        .args([
            "--max-pages",
            "10",
            "--max-depth",
            "1",
            "--concurrency",
            "1",
            "--max-retries",
            "0",
            "--delay",
            "500ms",
        ])
        .args(["--deadline", "30s", "--allow-private-addresses"])
        .output()
        .expect("the binary runs");
    let elapsed = started.elapsed();

    assert!(output.status.success(), "{}", stderr_of(&output));
    let report: serde_json::Value =
        serde_json::from_str(&stdout_of(&output)).expect("one object and nothing else");
    assert_eq!(
        report["links_recovered"],
        3,
        "all three siblings should have been recovered directly: {}",
        stdout_of(&output)
    );
    // A lower bound, which is the only kind a sleep can be held to: it never returns early,
    // and asserting an upper bound would be asserting that this machine was not busy. Half a
    // second is large enough that the three waits this pins, a second and a half together,
    // are not lost in the noise a real socket and a fresh runtime per fetch already cost.
    assert!(
        elapsed >= Duration::from_millis(1_200),
        "recovery fetched three pages under a 500ms delay in {elapsed:?}, which is not three \
         waits' worth of time"
    );
}

/// A 429 is an answer, not a recovery: `filter_and_forward` still archives it, because a
/// capture is what the server answered, but `recover_lost_links` does not let a status the
/// crawl's own retry policy would have repeated clear a URL off `still_missing`. One
/// candidate, the same deterministic "Allow outranks a shorter Disallow" shape used above so
/// there is no race to wait out, answers 429 to every request it receives; `--max-retries 2`
/// is what lets this also pin that recovery spends that budget rather than giving up on the
/// first attempt, since the request count the server keeps has to reach three, one initial
/// attempt and two retries, before the run gives up on it.
#[test]
fn recovery_answered_with_a_status_the_crawl_would_have_retried_stays_lost() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let requests = Arc::new(Mutex::new(0u32));
    let port = serve_a_site_that_always_answers_429(Arc::clone(&requests));
    let seed = format!("http://127.0.0.1:{port}/");
    let only_url =
        CanonicalUrl::parse(&format!("http://127.0.0.1:{port}/p/only")).expect("valid url");

    let output = archeion()
        .arg("capture")
        .arg("--json")
        .arg(&archive_path)
        .arg(&seed)
        .args([
            "--max-pages",
            "10",
            "--max-depth",
            "1",
            "--concurrency",
            "1",
            "--max-retries",
            "2",
        ])
        .args(["--deadline", "30s", "--allow-private-addresses"])
        .output()
        .expect("the binary runs");

    assert!(
        !output.status.success(),
        "a link stuck at 429 exited clean: {}",
        stdout_of(&output)
    );
    let report: serde_json::Value =
        serde_json::from_str(&stdout_of(&output)).expect("one object and nothing else");
    assert_eq!(
        report["links_never_followed"],
        serde_json::json!([only_url.as_str()]),
        "a 429 recovery never stopped repeating was not left in still_missing: {}",
        stdout_of(&output)
    );
    assert_eq!(
        report["links_recovered"],
        0,
        "a link that never answered under 400 was counted as recovered: {}",
        stdout_of(&output)
    );
    assert_eq!(
        *requests.lock().expect("no poison"),
        3,
        "the retry budget was not spent: one attempt plus two retries is three requests"
    );

    // Still archived, because a capture is what the server answered: `filter_and_forward`
    // gates on this project's own robots decision, not on the status recovery already
    // refused to call a success.
    let archive = Archive::open_existing(&archive_path).expect("the archive exists");
    assert!(
        !archive
            .list_captures(&only_url)
            .expect("captures are listed")
            .is_empty(),
        "the 429 response itself was not stored"
    );
}

fn serve_a_site_that_always_answers_429(requests: Arc<Mutex<u32>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the bound address").port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let requests = Arc::clone(&requests);
            thread::spawn(move || answer_always_429(stream, requests));
        }
    });
    port
}

fn answer_always_429(mut stream: TcpStream, requests: Arc<Mutex<u32>>) -> std::io::Result<()> {
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
    let robots_body = "User-agent: *\nDisallow: /p/\nAllow: /p/only\n";
    let seed_body = "<html><body><a href='/p/only'>only</a></body></html>";
    let (status, media_type, body): (&str, &str, &[u8]) = match path.as_str() {
        "/robots.txt" => ("200 OK", "text/plain", robots_body.as_bytes()),
        "/" => ("200 OK", "text/html; charset=utf-8", seed_body.as_bytes()),
        "/p/only" => {
            *requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) += 1;
            (
                "429 Too Many Requests",
                "text/html; charset=utf-8",
                b"<html><body>slow down</body></html>",
            )
        }
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

const ROBOTS_TXT_ALLOW_OUTRANKING_A_SHORTER_DISALLOW: &str = "User-agent: *\n\
    Disallow: /p/\n\
    Allow: /p/keep1\n\
    Allow: /p/keep2\n\
    Allow: /p/keep3\n";
const ALLOW_OUTRANKS_SEED_PAGE: &str = r#"<html><head><title>Home</title></head><body>
    <a href="/p/keep1">first</a>
    <a href="/p/keep2">second</a>
    <a href="/p/keep3">third</a>
    </body></html>"#;

fn serve_a_site_whose_allow_outranks_a_shorter_disallow() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the bound address").port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || answer_with_an_allow_outranking_a_shorter_disallow(stream));
        }
    });
    port
}

fn answer_with_an_allow_outranking_a_shorter_disallow(
    mut stream: TcpStream,
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
    let page = |title: &str| {
        format!("<html><head><title>{title}</title></head><body>a page</body></html>")
    };
    let (media_type, body): (&str, Vec<u8>) = match path.as_str() {
        "/robots.txt" => (
            "text/plain",
            ROBOTS_TXT_ALLOW_OUTRANKING_A_SHORTER_DISALLOW
                .as_bytes()
                .to_vec(),
        ),
        "/" => (
            "text/html; charset=utf-8",
            ALLOW_OUTRANKS_SEED_PAGE.as_bytes().to_vec(),
        ),
        _ => ("text/html; charset=utf-8", page(&path).into_bytes()),
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {media_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&body)?;
    stream.flush()
}

const ROBOTS_TXT_DISALLOWING_PRIVATE: &str = "User-agent: *\nDisallow: /private\n";
const ROBOTS_SEED_PAGE: &str = r#"<html><head><title>Home</title></head>
    <body><a href="/allowed">allowed</a><a href="/private">private</a></body></html>"#;
const ROBOTS_ALLOWED_PAGE: &str =
    "<html><head><title>Allowed</title></head><body>fine to read</body></html>";
const ROBOTS_PRIVATE_PAGE: &str =
    "<html><head><title>Private</title></head><body>the site said not to</body></html>";

/// The false positive a real `robots.txt` creates for the guard two tests up: a page linking
/// a path the site's own rules disallow is not the frontier dropping a link, and a run that
/// respects the rule and still exhausts everything else has archived exactly what it should
/// have. Concurrency four keeps this clear of the race the earlier test pins, so the only
/// thing that can make this one fail is the guard disagreeing with what `robots.txt` said.
#[test]
fn a_link_disallowed_by_robots_txt_is_not_reported_as_a_lost_link() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let port = serve_a_site_that_disallows_one_path();
    let seed = format!("http://127.0.0.1:{port}/");

    let output = archeion()
        .arg("capture")
        .arg(&archive_path)
        .arg(&seed)
        .args([
            "--max-pages",
            "10",
            "--max-depth",
            "1",
            "--concurrency",
            "4",
            "--max-retries",
            "0",
        ])
        .args(["--deadline", "30s", "--allow-private-addresses"])
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(stderr_of(&output), "");
    assert!(
        stdout_of(&output).contains("links lost    0"),
        "{}",
        stdout_of(&output)
    );

    let archive = Archive::open_existing(&archive_path).expect("the archive exists");
    let allowed = CanonicalUrl::parse(&format!("{seed}allowed")).expect("valid url");
    assert!(
        !archive
            .list_captures(&allowed)
            .expect("captures are listed")
            .is_empty(),
        "the page robots.txt actually allows was not archived"
    );
    let private = CanonicalUrl::parse(&format!("{seed}private")).expect("valid url");
    assert!(
        archive
            .list_captures(&private)
            .expect("captures are listed")
            .is_empty(),
        "a path robots.txt disallows was fetched anyway"
    );
}

/// A site whose own rules disallow one of its two linked pages, answered by a server this
/// test starts and nothing else: `/robots.txt` is a real 200 here rather than the 404 every
/// other test in this file relies on, since the whole point is a rule the crawl has to read.
fn serve_a_site_that_disallows_one_path() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the bound address").port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || answer_with_a_robots_rule(stream));
        }
    });
    port
}

fn answer_with_a_robots_rule(mut stream: TcpStream) -> std::io::Result<()> {
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
    let (media_type, body): (&str, &[u8]) = match path.as_str() {
        "/robots.txt" => ("text/plain", ROBOTS_TXT_DISALLOWING_PRIVATE.as_bytes()),
        "/" => ("text/html; charset=utf-8", ROBOTS_SEED_PAGE.as_bytes()),
        "/allowed" => ("text/html; charset=utf-8", ROBOTS_ALLOWED_PAGE.as_bytes()),
        "/private" => ("text/html; charset=utf-8", ROBOTS_PRIVATE_PAGE.as_bytes()),
        _ => ("text/plain", b"not here"),
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {media_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// A `robots.txt` written the way real ones are, with a wildcard in the middle of a pattern
/// and an anchor at the end of another. The crawl engine's own matcher reads a `Disallow`
/// with an interior wildcard as a literal prefix no path begins with, so it fetches every
/// matching page; what this pins is that none of them is archived, and that the plain
/// prefixes beside them are not traded away to get there.
const ROBOTS_TXT_WITH_WILDCARDS: &str = "User-agent: *\n\
    Disallow: /p/*/comment/*\n\
    Allow: /p/an-essay/comment/pinned\n\
    Disallow: /subscribe\n\
    Disallow: /action/\n\
    Disallow: /*.pdf$\n";
const WILDCARD_SEED_PAGE: &str = r#"<html><head><title>Home</title></head><body>
    <a href="/p/an-essay">an essay</a>
    <a href="/p/an-essay/comment/298986227">a comment on it</a>
    <a href="/p/an-essay/comment/pinned">the comment the site pinned</a>
    <a href="/subscribe">subscribe</a>
    <a href="/action/follow">follow</a>
    <a href="/report.pdf">the report</a>
    <a href="/report.pdf.html">the report, as a page</a>
    </body></html>"#;

/// The defect itself: a pattern with a wildcard anywhere but at its end, honoured end to end
/// through the binary and the real engine. The four refused addresses are each a shape the
/// rules cover differently, and `/report.pdf.html` is there because an anchored pattern that
/// swallowed it would be refusing more than the site asked for.
///
/// `/p/an-essay/comment/pinned` is the precedence half of RFC 9309 asked of the run rather
/// than of the matcher alone: it is covered by the wildcard `Disallow` and by a longer
/// `Allow`, and the longer one wins. It is observable here because it is the one shape both
/// matchers reach, the engine reading the `Allow` as an exact path and never matching the
/// wildcard at all. Read first rule first, as the engine reads a file, the page would be
/// refused and this would fail.
#[test]
fn a_disallow_with_an_interior_wildcard_keeps_its_paths_out_of_the_archive() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let port = serve_a_site_whose_rules_use_wildcards();
    let seed = format!("http://127.0.0.1:{port}/");

    let output = archeion()
        .arg("capture")
        .arg(&archive_path)
        .arg(&seed)
        .args([
            "--max-pages",
            "10",
            "--max-depth",
            "1",
            "--concurrency",
            "4",
            "--max-retries",
            "0",
        ])
        .args(["--deadline", "30s", "--allow-private-addresses"])
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(stderr_of(&output), "");
    assert!(
        stdout_of(&output).contains("links lost    0"),
        "{}",
        stdout_of(&output)
    );

    let archive = Archive::open_existing(&archive_path).expect("the archive exists");
    let archived = |path: &str| {
        let url = CanonicalUrl::parse(&format!("{seed}{path}")).expect("valid url");
        !archive
            .list_captures(&url)
            .expect("captures are listed")
            .is_empty()
    };
    assert!(archived("p/an-essay"), "a page no rule covers was not kept");
    assert!(
        archived("report.pdf.html"),
        "an anchored rule refused a path that does not end where it says"
    );
    assert!(
        archived("p/an-essay/comment/pinned"),
        "the longer Allow lost to the wildcard Disallow it sits under"
    );
    assert!(
        !archived("p/an-essay/comment/298986227"),
        "a path refused by a wildcard in the middle of a pattern was archived"
    );
    assert!(!archived("subscribe"), "a plain prefix stopped being read");
    assert!(
        !archived("action/follow"),
        "a plain prefix ending in a slash stopped being read"
    );
    assert!(
        !archived("report.pdf"),
        "a rule anchored with a dollar did not refuse the path it ends on"
    );
}

fn serve_a_site_whose_rules_use_wildcards() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the bound address").port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || answer_with_wildcard_robots_rules(stream));
        }
    });
    port
}

fn answer_with_wildcard_robots_rules(mut stream: TcpStream) -> std::io::Result<()> {
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
    let page = |title: &str| {
        format!("<html><head><title>{title}</title></head><body>a page</body></html>")
    };
    let (media_type, body): (&str, Vec<u8>) = match path.as_str() {
        "/robots.txt" => ("text/plain", ROBOTS_TXT_WITH_WILDCARDS.as_bytes().to_vec()),
        "/" => (
            "text/html; charset=utf-8",
            WILDCARD_SEED_PAGE.as_bytes().to_vec(),
        ),
        _ => ("text/html; charset=utf-8", page(&path).into_bytes()),
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {media_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&body)?;
    stream.flush()
}

const ROBOTS_TXT_WITH_A_PERCENT_ENCODED_WILDCARD: &str =
    "User-agent: *\nDisallow: /file-%2A.html\n";
const PERCENT_ENCODED_WILDCARD_SEED_PAGE: &str = r#"<html><head><title>Home</title></head><body>
    <a href="/file-%2A.html">the literal path the rule names</a>
    <a href="/file-anything.html">a path only a wildcard misreading would catch</a>
    </body></html>"#;

/// The over-refusal this bead exists to close: the vendored parser's own percent-decode
/// collapses `%2A` into the wildcard operator's character before this project's matcher ever
/// sees the rule, so a `Disallow` written against one literal path started refusing every
/// path the operator would have matched. Driven end to end through the binary and the real
/// engine, past the vendored parser's decode, which is where the collapse happens; a unit
/// test against the matcher alone would not exercise it.
#[test]
fn a_percent_encoded_wildcard_in_a_disallow_rule_refuses_only_the_literal_path() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let port = serve_a_site_whose_robots_escapes_a_wildcard();
    let seed = format!("http://127.0.0.1:{port}/");

    let output = archeion()
        .arg("capture")
        .arg(&archive_path)
        .arg(&seed)
        .args([
            "--max-pages",
            "10",
            "--max-depth",
            "1",
            "--concurrency",
            "4",
            "--max-retries",
            "0",
        ])
        .args(["--deadline", "30s", "--allow-private-addresses"])
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(stderr_of(&output), "");

    let archive = Archive::open_existing(&archive_path).expect("the archive exists");
    let archived = |path: &str| {
        let url = CanonicalUrl::parse(&format!("{seed}{path}")).expect("valid url");
        !archive
            .list_captures(&url)
            .expect("captures are listed")
            .is_empty()
    };
    assert!(
        !archived("file-%2A.html"),
        "the literal path the rule names was archived anyway"
    );
    assert!(
        archived("file-anything.html"),
        "a percent-encoded wildcard was read as the operator and over-refused an unrelated path"
    );
}

fn serve_a_site_whose_robots_escapes_a_wildcard() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the bound address").port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || answer_with_an_escaped_wildcard_robots_rule(stream));
        }
    });
    port
}

fn answer_with_an_escaped_wildcard_robots_rule(mut stream: TcpStream) -> std::io::Result<()> {
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
    let page = |title: &str| {
        format!("<html><head><title>{title}</title></head><body>a page</body></html>")
    };
    let (media_type, body): (&str, Vec<u8>) = match path.as_str() {
        "/robots.txt" => (
            "text/plain",
            ROBOTS_TXT_WITH_A_PERCENT_ENCODED_WILDCARD
                .as_bytes()
                .to_vec(),
        ),
        "/" => (
            "text/html; charset=utf-8",
            PERCENT_ENCODED_WILDCARD_SEED_PAGE.as_bytes().to_vec(),
        ),
        _ => ("text/html; charset=utf-8", page(&path).into_bytes()),
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {media_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&body)?;
    stream.flush()
}

/// A page linking to a second, so the header check below has more than one request to hold
/// against `--user-agent`: a client that sent the flag's value on the seed and reverted to
/// its own default on every request after would pass a check that only read the first.
const USER_AGENT_INDEX: &str = r#"<html><head><title>Index</title></head>
    <body><a href="/second">a second page</a></body></html>"#;
const USER_AGENT_SECOND_PAGE: &str =
    "<html><head><title>Second</title></head><body>reached from the index</body></html>";

/// `--user-agent`, honoured on the HTTP client of a real, multi-page crawl through the
/// binary: every request the run makes carries the string the flag named, not only the one
/// that fetched the seed.
#[test]
fn capture_sends_the_configured_user_agent_on_every_request() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let port = serve_recording_user_agent(Arc::clone(&requests));
    let seed = format!("http://127.0.0.1:{port}/");

    let output = archeion()
        .arg("capture")
        .arg(&archive_path)
        .arg(&seed)
        .args([
            "--max-pages",
            "2",
            "--max-depth",
            "1",
            "--concurrency",
            "1",
            "--max-retries",
            "0",
            "--user-agent",
            "archive-bot/9.0",
        ])
        .args(["--deadline", "30s", "--allow-private-addresses"])
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));
    assert!(
        stdout_of(&output).contains("archived 2 capture(s)"),
        "{}",
        stdout_of(&output)
    );

    let seen = requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(
        seen.len() >= 2,
        "the crawl made fewer requests than the pages it archived: {seen:?}"
    );
    assert!(
        seen.iter().all(|agent| agent == "archive-bot/9.0"),
        "not every request carried the configured identity: {seen:?}"
    );
}

fn serve_recording_user_agent(requests: Arc<Mutex<Vec<String>>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the bound address").port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let recorded = Arc::clone(&requests);
            thread::spawn(move || answer_recording_user_agent(stream, recorded));
        }
    });
    port
}

fn answer_recording_user_agent(
    mut stream: TcpStream,
    requests: Arc<Mutex<Vec<String>>>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut agent = None;
    let mut header = String::new();
    while reader.read_line(&mut header)? > 2 {
        if let Some(value) = header
            .strip_prefix("User-Agent:")
            .or_else(|| header.strip_prefix("user-agent:"))
        {
            agent = Some(value.trim().to_owned());
        }
        header.clear();
    }
    if let Some(agent) = agent {
        requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(agent);
    }

    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let (media_type, body): (&str, &[u8]) = match path.as_str() {
        "/robots.txt" => ("text/plain", b""),
        "/" => ("text/html; charset=utf-8", USER_AGENT_INDEX.as_bytes()),
        "/second" => (
            "text/html; charset=utf-8",
            USER_AGENT_SECOND_PAGE.as_bytes(),
        ),
        _ => ("text/plain", b"not here"),
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {media_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// A run given no `--user-agent` sends the same string the library compiles into
/// `DEFAULT_USER_AGENT`, byte for byte, rather than a copy of it typed into this test.
#[test]
fn capture_with_no_user_agent_flag_sends_the_compiled_default() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let port = serve_recording_user_agent(Arc::clone(&requests));
    let seed = format!("http://127.0.0.1:{port}/");

    let output = archeion()
        .arg("capture")
        .arg(&archive_path)
        .arg(&seed)
        .args([
            "--max-pages",
            "1",
            "--max-depth",
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

    let seen = requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(
        seen.first().map(String::as_str),
        Some(DEFAULT_USER_AGENT),
        "omitting the flag did not send the compiled default byte for byte"
    );
}

/// A value carrying a raw `\r\n` is refused before the process ever dials the seed: let
/// through, it reaches a client the vendored engine builds with `unwrap_unchecked`, which
/// aborts the process outright rather than reporting a failure this project defines. The
/// exit code and the message are `--cookie-file`'s own for the same class of value; no
/// archive is left behind, matching a seed refused for any other reason.
#[test]
fn capture_refuses_a_user_agent_carrying_a_control_character() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");

    let output = archeion()
        .arg("capture")
        .arg(&archive_path)
        .arg("http://127.0.0.1:1/")
        .args(["--deadline", "5s", "--allow-private-addresses"])
        .args([
            "--user-agent",
            "bad
X-Injected: 1",
        ])
        .output()
        .expect("the binary runs");

    assert!(
        !output.status.success(),
        "a header injection in --user-agent was accepted"
    );
    assert!(
        stderr_of(&output).contains("--user-agent"),
        "the refusal did not name the flag: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("cannot be sent in a header"),
        "{}",
        stderr_of(&output)
    );
    assert!(
        !archive_path.exists(),
        "a seed refused before the run started still left an archive behind"
    );
}

/// The rule is what a header can carry, not what ASCII can spell: an operator's own name
/// with an accent in it is ordinary and `--user-agent` must keep sending it rather than
/// refusing every byte a control character is not.
#[test]
fn capture_accepts_a_user_agent_carrying_a_non_ascii_character() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let port = serve_recording_user_agent(Arc::clone(&requests));
    let seed = format!("http://127.0.0.1:{port}/");

    let output = archeion()
        .arg("capture")
        .arg(&archive_path)
        .arg(&seed)
        .args([
            "--max-pages",
            "1",
            "--max-depth",
            "1",
            "--concurrency",
            "1",
            "--max-retries",
            "0",
        ])
        .args(["--deadline", "30s", "--allow-private-addresses"])
        .args(["--user-agent", "café/1.0"])
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));

    let seen = requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(
        seen.first().map(String::as_str),
        Some("café/1.0"),
        "a non-ASCII identity was not sent as given"
    );
}

/// A `robots.txt` naming this run's own identity and a `*` group that disagrees with it:
/// RFC 9309 reads whichever group names the requester and never the other one, so whether a
/// path either one mentions is refused turns entirely on the identity the run announced.
///
/// Each crawl below is pointed at a page linking to exactly one target rather than at one
/// page linking to both. The vendored engine's own frontier occasionally drops one of two
/// links discovered on the same page before either is fetched, a race in its own concurrent
/// link handling this project does not own and cannot fix from this side of the boundary.
/// Two seed pages, `/from-special` and `/from-general`, keep every crawl below to one
/// discovered link while still reaching both targets, which is what lets this cover both
/// halves of the claim: that the named group's own rule is obeyed, and that a path outside
/// it still falls to `*` or to nothing exactly as before this flag existed.
const ROBOTS_TXT_NAMING_ONE_AGENT: &str = "User-agent: archive-bot\n\
    Disallow: /special-only\n\n\
    User-agent: *\n\
    Disallow: /general-only\n";
const FROM_SPECIAL_SEED_PAGE: &str = r#"<html><head><title>From special</title></head><body>
    <a href="/special-only">the named group's rule</a>
    </body></html>"#;
const FROM_GENERAL_SEED_PAGE: &str = r#"<html><head><title>From general</title></head><body>
    <a href="/general-only">the wildcard's rule</a>
    </body></html>"#;
/// The shape `arch-ugh` was filed against: one page linking both targets, which is exactly
/// what `captures_robots_group_named_for_the_configured_user_agent`'s own two-seed split was
/// built to avoid, on purpose, for that test's own concurrency-of-one race, this test now
/// exists to cover directly. No other test here requests `/`, so adding it costs the two
/// tests above nothing.
const ROOT_PAGE_LINKING_BOTH: &str = r#"<html><head><title>Root</title></head><body>
    <a href="/special-only">the named group's rule</a>
    <a href="/general-only">the wildcard's rule</a>
    </body></html>"#;

/// `--user-agent` reaches the robots matcher, not only the HTTP client: run under the
/// identity a `robots.txt` names, a crawl is judged against that named group's own rule
/// rather than against `*`, a path the named group does not mention still falls to `*`, and
/// the same two paths swap verdicts when the run falls back to the compiled default exactly
/// as it did before this flag existed.
#[test]
fn captures_robots_group_named_for_the_configured_user_agent() {
    let dir = TempDir::new().expect("temp dir");
    let port = serve_a_site_naming_one_agent_in_robots();

    let run = |case: &str, from: &str, target: &str, agent: Option<&str>| -> bool {
        let seed = format!("http://127.0.0.1:{port}/{from}");
        let archive_path = dir.path().join(case);
        let mut command = archeion();
        command.arg("capture").arg(&archive_path).arg(&seed).args([
            "--max-pages",
            "10",
            "--max-depth",
            "1",
            "--concurrency",
            "1",
            "--max-retries",
            "0",
        ]);
        if let Some(agent) = agent {
            command.args(["--user-agent", agent]);
        }
        let output = command
            .args(["--deadline", "30s", "--allow-private-addresses"])
            .output()
            .expect("the binary runs");
        assert!(output.status.success(), "{case}: {}", stderr_of(&output));

        let archive = Archive::open_existing(&archive_path).expect("the archive exists");
        let url =
            CanonicalUrl::parse(&format!("http://127.0.0.1:{port}/{target}")).expect("valid url");
        !archive
            .list_captures(&url)
            .expect("captures are listed")
            .is_empty()
    };

    assert!(
        !run(
            "named-agent-on-its-own-rule",
            "from-special",
            "special-only",
            Some("archive-bot/9.0"),
        ),
        "the named group's own Disallow rule was not applied under the identity it names"
    );
    assert!(
        run(
            "named-agent-outside-its-rule",
            "from-general",
            "general-only",
            Some("archive-bot/9.0"),
        ),
        "the wildcard's rule governed a path the named group leaves unmentioned"
    );
    assert!(
        run(
            "default-agent-outside-the-named-rule",
            "from-special",
            "special-only",
            None,
        ),
        "the named group's rule leaked onto the identity it does not name"
    );
    assert!(
        !run(
            "default-agent-on-the-wildcard-rule",
            "from-general",
            "general-only",
            None,
        ),
        "the compiled default did not fall back to the wildcard group"
    );
}

fn serve_a_site_naming_one_agent_in_robots() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the bound address").port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || answer_naming_one_agent_in_robots(stream));
        }
    });
    port
}

fn answer_naming_one_agent_in_robots(mut stream: TcpStream) -> std::io::Result<()> {
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
    let (media_type, body): (&str, &[u8]) = match path.as_str() {
        "/robots.txt" => ("text/plain", ROBOTS_TXT_NAMING_ONE_AGENT.as_bytes()),
        "/" => (
            "text/html; charset=utf-8",
            ROOT_PAGE_LINKING_BOTH.as_bytes(),
        ),
        "/from-special" => (
            "text/html; charset=utf-8",
            FROM_SPECIAL_SEED_PAGE.as_bytes(),
        ),
        "/from-general" => (
            "text/html; charset=utf-8",
            FROM_GENERAL_SEED_PAGE.as_bytes(),
        ),
        "/special-only" => (
            "text/html; charset=utf-8",
            b"<html><head><title>Special</title></head><body>the named group's page</body></html>",
        ),
        "/general-only" => (
            "text/html; charset=utf-8",
            b"<html><head><title>General</title></head><body>the wildcard's page</body></html>",
        ),
        _ => ("text/plain", b"not here"),
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {media_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// arch-ugh: one page linking two targets, under a `robots.txt` naming the identity the run
/// is using for one of them. RFC 9309 says the named group governs alone once it matches, so
/// this project's own `Disallow: /general-only` under `*` never applies here at all, which
/// means neither link is one the named group's own rule refuses; the vendored engine's own
/// frontier has nothing left to prefer over the other at a concurrency of one, and its own
/// well-known race, `captures_robots_group_named_for_the_configured_user_agent`'s own
/// two-seed split exists to dodge, drops one of the two on every single run rather than on
/// some of them. Measured before the fix: five failures in eight of exactly this shape.
///
/// Looping the same crawl rather than asserting once is the point: a version of this that
/// passed on its first try would have proven nothing about a race that struck roughly half
/// the time. `links_recovered`, read off `--json` and summed across every attempt, is what
/// tells the general-only half of the two assertions below apart from a run where the
/// ordinary frontier simply fetched the page itself and `recover_lost_links` was never
/// asked for anything: both leave the same archive behind, and only the count says which
/// happened.
#[test]
fn a_link_the_named_group_leaves_unmentioned_is_not_lost_to_the_frontier_race() {
    let dir = TempDir::new().expect("temp dir");
    let port = serve_a_site_naming_one_agent_in_robots();
    let general_only_url =
        CanonicalUrl::parse(&format!("http://127.0.0.1:{port}/general-only")).expect("valid url");
    let special_only_url =
        CanonicalUrl::parse(&format!("http://127.0.0.1:{port}/special-only")).expect("valid url");
    let mut links_recovered_total = 0u64;

    for attempt in 0..20 {
        let seed = format!("http://127.0.0.1:{port}/");
        let archive_path = dir.path().join(format!("attempt-{attempt}"));
        let output = archeion()
            .arg("capture")
            .arg("--json")
            .arg(&archive_path)
            .arg(&seed)
            .args([
                "--max-pages",
                "10",
                "--max-depth",
                "1",
                "--concurrency",
                "1",
                "--max-retries",
                "0",
                "--user-agent",
                "archive-bot/9.0",
                "--deadline",
                "20s",
                "--allow-private-addresses",
            ])
            .output()
            .expect("the binary runs");
        assert!(
            output.status.success(),
            "attempt {attempt}: {}",
            stderr_of(&output)
        );

        let archive = Archive::open_existing(&archive_path).expect("the archive exists");
        assert!(
            !archive
                .list_captures(&general_only_url)
                .expect("captures are listed")
                .is_empty(),
            "attempt {attempt}: the path the named group leaves unmentioned was not archived"
        );
        assert!(
            archive
                .list_captures(&special_only_url)
                .expect("captures are listed")
                .is_empty(),
            "attempt {attempt}: the named group's own Disallow rule was not applied"
        );

        let report: serde_json::Value =
            serde_json::from_str(&stdout_of(&output)).expect("one object and nothing else");
        links_recovered_total += report["links_recovered"].as_u64().expect("a count");
    }

    assert!(
        links_recovered_total > 0,
        "twenty attempts against a robots.txt naming the running identity never asked recover_lost_links for a single link, so this loop never exercised the race arch-ugh was filed against"
    );
}

/// The other way a discovered link can look lost without being lost: not a rule that
/// refused it, but a spelling the two sides of the comparison read differently. Two shapes
/// pages in the wild actually write: an entity inside an href rather than the character it
/// stands for, and a non-ASCII character percent-encoded rather than written literally.
/// Both still have to end up archived and unreported.
///
/// This does not assert what either linked page's canonical URL comes out as. `push_link`
/// still joins the entity-carrying href into a URL before anything decodes it; what
/// `arch-42q` added is a second resolution of the same href, decoded, that `hop_depth_guard`
/// records instead of the undecoded one and that `rewrite_escaped_href` hands the engine in
/// place of the address it resolved, so the two sides of the comparison this guard runs
/// agree on the corrected spelling rather than agreeing on the wrong one. What is asserted
/// here is the part that predates that fix and stays true regardless: both links are still
/// followed, still archived, and never reported as ones the crawl discovered and did not
/// fetch.
#[test]
fn a_link_whose_href_spells_its_query_string_with_an_entity_is_archived_and_not_reported_lost() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let site = Site::start();

    let output = archeion()
        .arg("capture")
        .arg(&archive_path)
        .arg(site.url("/entity-index.html"))
        .args([
            "--max-pages",
            "4",
            "--max-depth",
            "1",
            "--concurrency",
            "4",
            "--max-retries",
            "0",
        ])
        .args(["--deadline", "30s", "--allow-private-addresses"])
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(stderr_of(&output), "");
    assert!(
        stdout_of(&output).contains("archived 3 capture(s)"),
        "{}",
        stdout_of(&output)
    );
    assert!(
        stdout_of(&output).contains("links lost    0"),
        "{}",
        stdout_of(&output)
    );
}

/// A loopback site that remembers the path and query of every request it answered, so a
/// test can assert what the server actually received rather than what the archive ended up
/// holding once canonicalization has already folded a decoded and an undecoded spelling of
/// the same address into one item. `Site` above has no reason to do this: nothing else in
/// this file needs to tell one request from a second one on the same path.
struct RecordingSite {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
}

impl RecordingSite {
    /// `index_body` is served for `/index.html`; any other path answers with a page fixed
    /// enough to prove it was reached without saying anything about which address reached
    /// it, since the address itself is what the test is asking about.
    fn start(index_body: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let port = listener.local_addr().expect("the bound address").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_thread = requests.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let requests = requests_for_thread.clone();
                thread::spawn(move || answer_and_record(stream, index_body, requests));
            }
        });
        Self { port, requests }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Every request line seen so far whose path starts with `prefix`, in arrival order.
    fn requests_for(&self, prefix: &str) -> Vec<String> {
        self.requests
            .lock()
            .expect("the request log")
            .iter()
            .filter(|line| line.starts_with(prefix))
            .cloned()
            .collect()
    }
}

fn answer_and_record(
    mut stream: TcpStream,
    index_body: &str,
    requests: Arc<Mutex<Vec<String>>>,
) -> std::io::Result<()> {
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
    requests.lock().expect("the request log").push(path.clone());
    let (status, body): (&str, &[u8]) = if path == "/robots.txt" {
        ("404 Not Found", b"")
    } else if path == "/index.html" {
        ("200 OK", index_body.as_bytes())
    } else {
        (
            "200 OK",
            b"<html><head><title>Post</title></head><body>found</body></html>",
        )
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Drives a real crawl against a page whose only link spells the query separator with
/// `separator`, one of the three character references the HTML standard defines for `&`,
/// and asserts what the server actually saw: one request, carrying the query the page
/// meant rather than the escape or the fragment a URL parser cuts it into. `push_link`
/// resolves the href before anything decodes it, so before `arch-42q` the server received
/// either the escape verbatim or a query truncated at a `#` that was never part of the
/// page's own address; this is `hop_depth_guard`'s `corrected_resolution` and
/// `rewrite_escaped_href` proven at the socket rather than read back out of the archive.
fn a_character_reference_in_the_query_separator_reaches_the_server_decoded(separator: &str) {
    let index = format!(
        r#"<html><head><title>Index</title></head>
        <body><a href="/post?x=1{separator}y=2">the post</a></body></html>"#
    );
    let index: &'static str = Box::leak(index.into_boxed_str());
    let site = RecordingSite::start(index);
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");

    let output = archeion()
        .arg("capture")
        .arg(&archive_path)
        .arg(site.url("/index.html"))
        .args([
            "--max-pages",
            "4",
            "--max-depth",
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
    assert!(
        stdout_of(&output).contains("archived 2 capture(s)"),
        "{}",
        stdout_of(&output)
    );
    assert!(
        stdout_of(&output).contains("links lost    0"),
        "{}",
        stdout_of(&output)
    );

    assert_eq!(
        site.requests_for("/post"),
        vec!["/post?x=1&y=2".to_string()],
        "the request(s) the server received for the {separator:?} spelling"
    );

    let archive = Archive::open_existing(&archive_path).expect("the archive exists");
    let post = CanonicalUrl::parse(&site.url("/post?x=1&y=2")).expect("valid url");
    assert_eq!(
        archive
            .list_captures(&post)
            .expect("captures are listed")
            .len(),
        1,
        "the post should be filed as a single item regardless of how its link spelled `&`"
    );
}

#[test]
fn an_escaped_ampersand_in_the_query_separator_reaches_the_server_decoded() {
    a_character_reference_in_the_query_separator_reaches_the_server_decoded("&amp;");
}

#[test]
fn a_decimal_character_reference_in_the_query_separator_reaches_the_server_decoded() {
    a_character_reference_in_the_query_separator_reaches_the_server_decoded("&#38;");
}

#[test]
fn a_hex_character_reference_in_the_query_separator_reaches_the_server_decoded() {
    a_character_reference_in_the_query_separator_reaches_the_server_decoded("&#x26;");
}

/// The mirror of the case above and the one seen on real sites: an absolute self link
/// hardcoded in the other scheme from the one the seed was typed with. `push_link` in the
/// dependency rewrites a resolved, in-scope link's scheme to the seed's own before the link
/// ever reaches its frontier, so the fetch lands on the seed's scheme regardless of what the
/// page wrote; `depth_key` has to land on the same spelling or this reports a link archived
/// under a different scheme than the one it recorded.
#[test]
fn a_page_carrying_an_absolute_self_link_in_the_other_scheme_is_archived_without_a_reported_loss() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let site = Site::start();

    let output = archeion()
        .arg("capture")
        .arg(&archive_path)
        .arg(site.url("/other-scheme-index.html"))
        .args([
            "--max-pages",
            "4",
            "--max-depth",
            "1",
            "--concurrency",
            "4",
            "--max-retries",
            "0",
        ])
        .args(["--deadline", "30s", "--allow-private-addresses"])
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(stderr_of(&output), "");
    assert!(
        stdout_of(&output).contains("links lost    0"),
        "{}",
        stdout_of(&output)
    );

    let archive = Archive::open_existing(&archive_path).expect("the archive exists");
    let target = CanonicalUrl::parse(&site.url("/other-scheme-target")).expect("valid url");
    assert!(
        !archive
            .list_captures(&target)
            .expect("captures are listed")
            .is_empty(),
        "the link written in the other scheme was not archived"
    );
}

/// A page declaring an absolute `<base href>` resolves every relative link on it against
/// that value instead of against its own URL. This adapter has no way to read the same base
/// back out of `Page` without a second HTML pass of its own, so a page like this one is left
/// out of the depth map entirely: `docs/crawl-boundary.md` has the trade being made. What
/// this pins is the outward half of that decision, that the page underneath the rewritten
/// base is still archived and never reported as a link the crawl lost.
#[test]
fn a_page_declaring_an_absolute_base_href_is_archived_without_a_reported_loss() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let site = Site::start();

    let output = archeion()
        .arg("capture")
        .arg(&archive_path)
        .arg(site.url("/base-href-index.html"))
        .args([
            "--max-pages",
            "10",
            "--max-depth",
            "2",
            "--concurrency",
            "4",
            "--max-retries",
            "0",
        ])
        .args(["--deadline", "30s", "--allow-private-addresses"])
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(stderr_of(&output), "");
    assert!(
        stdout_of(&output).contains("links lost    0"),
        "{}",
        stdout_of(&output)
    );

    let archive = Archive::open_existing(&archive_path).expect("the archive exists");
    let intro = CanonicalUrl::parse(&site.url("/intro.html")).expect("valid url");
    assert!(
        !archive
            .list_captures(&intro)
            .expect("captures are listed")
            .is_empty(),
        "the page reached through the rewritten base was not archived"
    );
}

/// A same-host link in a scheme the engine will never dial. `validate_link` in the
/// dependency drops anything that is not `http` or `https` before it ever reaches the
/// frontier, so this project's own bookkeeping has to drop it on the same terms or it
/// reports a fetch the engine was never going to make.
#[test]
fn a_same_host_link_in_an_unfetchable_scheme_is_not_reported_as_a_lost_link() {
    let dir = TempDir::new().expect("temp dir");
    let archive_path = dir.path().join("collection");
    let site = Site::start();

    let output = archeion()
        .arg("capture")
        .arg(&archive_path)
        .arg(site.url("/ftp-index.html"))
        .args([
            "--max-pages",
            "4",
            "--max-depth",
            "1",
            "--concurrency",
            "4",
            "--max-retries",
            "0",
        ])
        .args(["--deadline", "30s", "--allow-private-addresses"])
        .output()
        .expect("the binary runs");

    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(stderr_of(&output), "");
    assert!(
        stdout_of(&output).contains("links lost    0"),
        "{}",
        stdout_of(&output)
    );

    let archive = Archive::open_existing(&archive_path).expect("the archive exists");
    let target = CanonicalUrl::parse(&site.url("/ftp-target")).expect("valid url");
    assert!(
        !archive
            .list_captures(&target)
            .expect("captures are listed")
            .is_empty(),
        "the ordinary link beside the FTP one was not archived"
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
                thread::spawn(move || answer(stream, port));
            }
        });
        Self { port }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

fn answer(mut stream: TcpStream, port: u16) -> std::io::Result<()> {
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
    let other_scheme_index_page = other_scheme_index(port);
    let base_href_guide = base_href_guide_page(port);
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
        "/entity-index.html" => (
            "200 OK",
            "text/html; charset=utf-8",
            ENTITY_INDEX.as_bytes(),
        ),
        "/other-scheme-index.html" => (
            "200 OK",
            "text/html; charset=utf-8",
            other_scheme_index_page.as_bytes(),
        ),
        "/other-scheme-target" => (
            "200 OK",
            "text/html; charset=utf-8",
            OTHER_SCHEME_TARGET_PAGE.as_bytes(),
        ),
        "/base-href-index.html" => (
            "200 OK",
            "text/html; charset=utf-8",
            BASE_HREF_INDEX.as_bytes(),
        ),
        "/docs/guide.html" => (
            "200 OK",
            "text/html; charset=utf-8",
            base_href_guide.as_bytes(),
        ),
        "/intro.html" => ("200 OK", "text/html; charset=utf-8", INTRO_PAGE.as_bytes()),
        "/ftp-index.html" => (
            "200 OK",
            "text/html; charset=utf-8",
            FTP_SCHEME_INDEX.as_bytes(),
        ),
        "/ftp-target" => (
            "200 OK",
            "text/html; charset=utf-8",
            FTP_TARGET_PAGE.as_bytes(),
        ),
        // Answered on the path alone, whatever the query string turns out to be spelled
        // as by the time it is requested: what this fixture is asking is whether the link
        // is followed and archived at all, not whether the entity in it was decoded first.
        path if path.starts_with("/entity-target") => (
            "200 OK",
            "text/html; charset=utf-8",
            ENTITY_TARGET_PAGE.as_bytes(),
        ),
        path if path.starts_with("/caf") => (
            "200 OK",
            "text/html; charset=utf-8",
            ENTITY_TARGET_PAGE.as_bytes(),
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
