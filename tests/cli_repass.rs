use std::process::Command;

use archeion::CanonicalUrl;
use archeion::readability::{AdmissionCost, Article, ArticleRecord, ExtractionRules, ProseShare};
use archeion::storage::{Archive, Header, NewCapture};
use jiff::Timestamp;
use serde_json::Value;
use tempfile::TempDir;

fn archeion() -> Command {
    Command::new(env!("CARGO_BIN_EXE_archeion"))
}

fn at(instant: &str) -> Timestamp {
    instant.parse().expect("test timestamp is valid")
}

fn html_capture(url: &CanonicalUrl, body: &str) -> NewCapture {
    NewCapture {
        canonical_url: url.clone(),
        requested_url: url.as_str().to_owned(),
        final_url: url.as_str().to_owned(),
        status: 200,
        media_type: Some("text/html".to_owned()),
        response_headers: vec![Header {
            name: "content-type".to_owned(),
            value: "text/html; charset=utf-8".to_owned(),
        }],
        body: body.as_bytes().to_vec(),
        body_truncated: false,
        fetched_at: at("2026-07-25T14:03:22Z"),
        assets: Vec::new(),
        assets_missed: Vec::new(),
        policy_departures: Vec::new(),
    }
}

fn stale_article() -> Article {
    Article {
        markdown: "# Old\n\nOld prose.".to_owned(),
        record: ArticleRecord {
            extractor_version: 1,
            rules: ExtractionRules::Heuristic,
            word_count: 3,
            share: Some(ProseShare {
                article_chars: 300,
                page_chars: 300,
            }),
            excerpt: Some("Old prose".to_owned()),
            byline: None,
            accessible_for_free: None,
            truncated: Vec::new(),
            cost: AdmissionCost {
                document_bytes: 100,
                peak_open_elements: 4,
            },
        },
    }
}

#[test]
fn repass_refreshes_derived_records_from_the_command_line() {
    let dir = TempDir::new().expect("temp dir");
    let archive = Archive::open(dir.path()).expect("archive opens");
    let url = CanonicalUrl::parse("https://example.com/listing").expect("valid URL");
    let capture = archive
        .write_capture(html_capture(
            &url,
            "<html><head><title>Recipes</title></head><body><h1>Recipes</h1><ul>\
             <li><a href=\"/a\">Ten pasta shapes</a></li><li><a href=\"/b\">Bread</a></li>\
             <li><a href=\"/c\">Soup</a></li><li><a href=\"/d\">Cake</a></li></ul></body></html>",
        ))
        .expect("capture is written");
    archive
        .write_article(&url, &capture.id, &stale_article())
        .expect("old article is written");

    let output = archeion()
        .arg("--json")
        .arg("repass")
        .arg(dir.path())
        .output()
        .expect("the binary runs");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    let report: Value = serde_json::from_slice(&output.stdout).expect("stdout is one JSON report");
    assert_eq!(report["captures_seen"], 1);
    assert_eq!(report["non_articles_marked"], 1);
    assert_eq!(report["asset_fetches"], 0);
    assert_eq!(report["unreadable_pages"], Value::Array(Vec::new()));

    assert!(
        archive
            .read_article(&url, &capture.id)
            .expect("article read succeeds")
            .is_none()
    );
    assert!(
        archive
            .read_non_article(&url, &capture.id)
            .expect("non-article marker reads")
            .is_some()
    );
}

/// The same guarantee `capture` makes, over the verb whose silence is the same problem: the
/// flag changes stderr and stdout not at all, in either mode, at every level.
#[test]
fn no_progress_level_changes_a_single_byte_of_a_repass_report() {
    let at_level = |level: Option<&str>, json: bool| {
        let dir = TempDir::new().expect("temp dir");
        let archive = Archive::open(dir.path()).expect("archive opens");
        let url = CanonicalUrl::parse("https://example.com/listing").expect("valid URL");
        let capture = archive
            .write_capture(html_capture(
                &url,
                "<html><head><title>Recipes</title></head><body><h1>Recipes</h1><ul>\
                 <li><a href=\"/a\">Ten pasta shapes</a></li><li><a href=\"/b\">Bread</a></li>\
                 <li><a href=\"/c\">Soup</a></li><li><a href=\"/d\">Cake</a></li></ul></body></html>",
            ))
            .expect("capture is written");
        archive
            .write_article(&url, &capture.id, &stale_article())
            .expect("old article is written");

        let mut command = archeion();
        if json {
            command.arg("--json");
        }
        command.arg("repass");
        if let Some(level) = level {
            command.arg(level);
        }
        let output = command.arg(dir.path()).output().expect("the binary runs");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout)
            .replace(&dir.path().display().to_string(), "<archive>");
        (stdout, String::from_utf8_lossy(&output.stderr).into_owned())
    };

    let (human, human_stderr) = at_level(None, false);
    assert_eq!(
        human,
        "repassed 1 capture(s) in <archive>\n  \
         metadata      1 written\n  \
         articles      0 written, 0 refused, 1 not article\n  \
         assets        0 recovered, 0 still missing, 0 not retried, 0 request(s)\n  \
         unchanged     0 derived record(s)\n"
    );
    assert_eq!(
        human_stderr, "",
        "a repass with no --progress said something"
    );
    let (machine, machine_stderr) = at_level(None, true);
    assert_eq!(machine_stderr, "");

    for level in ["--progress", "--progress=lines", "--progress=bar"] {
        let (stdout, stderr) = at_level(Some(level), false);
        assert_eq!(stdout, human, "{level} reached stdout");
        assert!(!stderr.is_empty(), "{level} said nothing at all");
        assert!(!stderr.contains('\r'), "{level} redrew into a pipe");
        assert!(
            !stderr.contains('\x1b'),
            "{level} wrote an escape into a pipe"
        );
        assert_eq!(
            at_level(Some(level), true).0,
            machine,
            "{level} reached stdout"
        );
    }

    // What each level says, since writing no control characters and also saying nothing would
    // satisfy every assertion above.
    assert!(
        at_level(Some("--progress=lines"), false)
            .1
            .contains("https://example.com/listing: not article"),
        "the lines level named no capture and what it became"
    );
    assert!(
        at_level(Some("--progress=bar"), false)
            .1
            .contains("1/1 items"),
        "the bar stated no progress against the walk's own total"
    );
}
