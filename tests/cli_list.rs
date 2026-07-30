use std::process::Command;

use archeion::CanonicalUrl;
use archeion::readability::{AdmissionCost, Article, ArticleRecord, ExtractionRules, ProseShare};
use archeion::storage::{Archive, CaptureId, Header, ItemId, NewCapture};
use jiff::Timestamp;
use tempfile::TempDir;

fn at(instant: &str) -> Timestamp {
    instant.parse().expect("test timestamp is valid")
}

fn capture_of(url: &CanonicalUrl, fetched_at: Timestamp, title: &str) -> NewCapture {
    NewCapture {
        canonical_url: url.clone(),
        requested_url: url.as_str().to_owned(),
        final_url: url.as_str().to_owned(),
        status: 200,
        media_type: Some("text/html; charset=utf-8".to_owned()),
        response_headers: vec![Header {
            name: "content-type".to_owned(),
            value: "text/html; charset=utf-8".to_owned(),
        }],
        body: format!("<html><title>{title}</title></html>").into_bytes(),
        body_truncated: false,
        fetched_at,
        assets: Vec::new(),
        assets_missed: Vec::new(),
        policy_departures: Vec::new(),
    }
}

fn article() -> Article {
    Article {
        markdown: "# Latest article\n\nThis capture produced prose.".to_owned(),
        record: ArticleRecord {
            extractor_version: archeion::readability::EXTRACTOR_VERSION,
            rules: ExtractionRules::Heuristic,
            word_count: 5,
            share: Some(ProseShare {
                article_chars: 240,
                page_chars: 300,
            }),
            excerpt: Some("This capture produced prose.".to_owned()),
            byline: None,
            accessible_for_free: None,
            truncated: Vec::new(),
            cost: AdmissionCost {
                document_bytes: 256,
                peak_open_elements: 4,
            },
        },
    }
}

fn archive_fixture() -> TempDir {
    let dir = TempDir::new().expect("temp dir");
    let archive = Archive::open(dir.path()).expect("archive opens");
    let first = CanonicalUrl::parse("https://blog.example.com/first").expect("valid url");
    let second = CanonicalUrl::parse("https://example.com/second").expect("valid url");

    archive
        .write_capture(capture_of(&first, at("2026-07-25T14:03:22Z"), "First"))
        .expect("first capture is written");
    let latest = archive
        .write_capture(capture_of(
            &first,
            at("2026-07-26T09:00:00Z"),
            "First updated",
        ))
        .expect("latest capture is written");
    archive
        .write_article(&first, &latest.id, &article())
        .expect("article is written");
    archive
        .write_capture(capture_of(&second, at("2026-07-25T14:03:22Z"), "Second"))
        .expect("second item is written");

    dir
}

fn archeion() -> Command {
    Command::new(env!("CARGO_BIN_EXE_archeion"))
}

fn capture_path(dir: &TempDir, url: &CanonicalUrl, capture: &CaptureId) -> std::path::PathBuf {
    dir.path()
        .join("items")
        .join(url.host_dir())
        .join(ItemId::of(url).as_str())
        .join("captures")
        .join(format!("{capture}.json"))
}

#[test]
fn list_reports_the_archive_as_a_table() {
    let dir = archive_fixture();

    let output = archeion()
        .arg("list")
        .arg(dir.path())
        .output()
        .expect("the binary runs");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "URL                             CAPTURES  LAST_CAPTURED_AT      ARTICLE\n\
         https://blog.example.com/first         2  2026-07-26T09:00:00Z  yes\n\
         https://example.com/second             1  2026-07-25T14:03:22Z  no\n"
    );
}

#[test]
fn list_reports_the_archive_as_json_lines() {
    let dir = archive_fixture();

    let output = archeion()
        .arg("list")
        .arg("--json")
        .arg(dir.path())
        .output()
        .expect("the binary runs");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "{\"canonical_url\":\"https://blog.example.com/first\",\"captures\":2,\"last_captured_at\":\"2026-07-26T09:00:00Z\",\"has_article\":true}\n\
         {\"canonical_url\":\"https://example.com/second\",\"captures\":1,\"last_captured_at\":\"2026-07-25T14:03:22Z\",\"has_article\":false}\n"
    );
}

#[test]
fn list_does_not_need_to_parse_a_capture_record_to_count_it() {
    let dir = archive_fixture();
    let archive = Archive::open_existing(dir.path()).expect("archive opens");
    let url = CanonicalUrl::parse("https://example.com/second").expect("valid url");
    let capture = archive
        .list_captures(&url)
        .expect("captures are listed")
        .pop()
        .expect("a capture exists");
    std::fs::write(capture_path(&dir, &url, &capture), b"{ not a capture")
        .expect("capture record is damaged");

    let output = archeion()
        .arg("list")
        .arg(dir.path())
        .output()
        .expect("the binary runs");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "URL                             CAPTURES  LAST_CAPTURED_AT      ARTICLE\n\
         https://blog.example.com/first         2  2026-07-26T09:00:00Z  yes\n\
         https://example.com/second             1  2026-07-25T14:03:22Z  no\n"
    );
}

#[test]
fn list_of_a_missing_path_does_not_create_an_empty_archive() {
    let dir = TempDir::new().expect("temp dir");
    let missing = dir.path().join("missing");

    let output = archeion()
        .arg("list")
        .arg(&missing)
        .output()
        .expect("the binary runs");

    assert!(!output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        format!("{} does not exist\n", missing.display())
    );
    assert!(!missing.exists());
}
