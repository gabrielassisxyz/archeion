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
