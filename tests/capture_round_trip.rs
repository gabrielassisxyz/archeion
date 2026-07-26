//! The archive is only worth anything if what went in comes back out, and if the shape it
//! takes on disk is the documented one: `docs/storage-model.md` is a promise to whoever
//! opens the directory in ten years, so the layout is asserted here and not just described.

use archeion::CanonicalUrl;
use archeion::storage::{Archive, ContentHash, Header, ItemId, NewAsset, NewCapture, StorageError};
use jiff::Timestamp;
use tempfile::TempDir;

const PAGE: &[u8] = b"<html><title>A page</title></html>";
const STYLESHEET: &[u8] = b"body { color: rebeccapurple }";

fn at(instant: &str) -> Timestamp {
    instant.parse().expect("test timestamp is valid")
}

fn page_capture(url: &CanonicalUrl, fetched_at: Timestamp, body: &[u8]) -> NewCapture {
    NewCapture {
        canonical_url: url.clone(),
        requested_url: "http://example.com/a-page".to_owned(),
        final_url: "https://example.com/a-page".to_owned(),
        status: 200,
        media_type: Some("text/html; charset=utf-8".to_owned()),
        response_headers: vec![Header {
            name: "content-type".to_owned(),
            value: "text/html; charset=utf-8".to_owned(),
        }],
        body: body.to_vec(),
        body_truncated: false,
        fetched_at,
        assets: vec![NewAsset {
            requested_url: "https://example.com/style.css".to_owned(),
            final_url: "https://example.com/style.css".to_owned(),
            status: 200,
            media_type: Some("text/css".to_owned()),
            body: STYLESHEET.to_vec(),
        }],
    }
}

fn archive_in(dir: &TempDir) -> Archive {
    Archive::open(dir.path()).expect("a fresh directory opens as an archive")
}

#[test]
fn a_capture_survives_a_write_and_a_read() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a-page").expect("valid url");

    let written = archive
        .write_capture(page_capture(&url, at("2026-07-25T14:03:22Z"), PAGE))
        .expect("capture is stored");
    let read_back = archive
        .read_capture(&url, &written.id)
        .expect("capture is found");

    assert_eq!(read_back, written);
    assert_eq!(read_back.body.sha256, ContentHash::of(PAGE));
    assert_eq!(read_back.body.byte_len, PAGE.len() as u64);
    assert_eq!(
        archive.read_body(&read_back.body.sha256).expect("body"),
        PAGE
    );
    assert_eq!(
        archive
            .read_body(&read_back.assets[0].body.sha256)
            .expect("asset body"),
        STYLESHEET
    );
}

#[test]
fn the_item_record_names_the_url_its_directory_only_hashes() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a-page").expect("valid url");

    archive
        .write_capture(page_capture(&url, at("2026-07-25T14:03:22Z"), PAGE))
        .expect("capture is stored");
    let item = archive.read_item(&url).expect("item is found");

    assert_eq!(item.canonical_url, url);
    assert_eq!(item.id, ItemId::of(&url));
    assert_eq!(item.first_captured_at, at("2026-07-25T14:03:22Z"));
    assert_eq!(item.last_captured_at, at("2026-07-25T14:03:22Z"));
}

#[test]
fn the_layout_on_disk_is_the_documented_one() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a-page").expect("valid url");

    let capture = archive
        .write_capture(page_capture(&url, at("2026-07-25T14:03:22Z"), PAGE))
        .expect("capture is stored");

    let item_dir = dir
        .path()
        .join("items")
        .join("example.com")
        .join(ItemId::of(&url).as_str());
    let hash = ContentHash::of(PAGE);
    let blob = dir
        .path()
        .join("blobs")
        .join("sha256")
        .join(&hash.as_str()[0..2])
        .join(&hash.as_str()[2..4])
        .join(hash.as_str());

    assert!(dir.path().join("archeion.json").is_file());
    assert!(item_dir.join("item.json").is_file());
    assert!(
        item_dir
            .join("captures")
            .join(format!("{}.json", capture.id))
            .is_file()
    );
    assert!(blob.is_file());
}

#[test]
fn recapturing_unchanged_bytes_adds_a_capture_and_not_a_copy() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a-page").expect("valid url");

    archive
        .write_capture(page_capture(&url, at("2026-07-25T14:03:22Z"), PAGE))
        .expect("first capture");
    archive
        .write_capture(page_capture(&url, at("2026-08-01T09:00:00Z"), PAGE))
        .expect("second capture");

    let captures = archive.list_captures(&url).expect("captures are listed");
    assert_eq!(captures.len(), 2);
    assert!(captures[0].as_str() < captures[1].as_str());
    // The page and the stylesheet, one file each, however many captures reference them.
    assert_eq!(stored_body_count(dir.path()), 2);
}

/// The other half of dedupe, and the reason canonicalization exists: the spellings a page
/// is linked by around the web are one address here, so they share one item directory and
/// one history rather than archiving the same site once per spelling.
#[test]
fn every_spelling_of_a_page_lands_on_one_item() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let spellings = [
        ("https://example.com/a-page", "2026-07-25T14:03:22Z"),
        ("https://WWW.Example.com/a-page", "2026-07-26T14:03:22Z"),
        ("https://example.com.:443/a-page", "2026-07-27T14:03:22Z"),
        (
            "https://example.com/a-page#section-2",
            "2026-07-28T14:03:22Z",
        ),
        (
            "https://example.com/a-page?utm_source=newsletter",
            "2026-07-29T14:03:22Z",
        ),
    ];

    for (spelling, fetched_at) in spellings {
        let url = CanonicalUrl::parse(spelling).expect("valid url");
        archive
            .write_capture(page_capture(&url, at(fetched_at), PAGE))
            .expect("capture is stored");
    }

    let url = CanonicalUrl::parse("https://example.com/a-page").expect("valid url");
    assert_eq!(item_record_count(dir.path()), 1);
    assert_eq!(
        archive
            .list_captures(&url)
            .expect("captures are listed")
            .len(),
        spellings.len()
    );
    assert_eq!(stored_body_count(dir.path()), 2);
    let item = archive.read_item(&url).expect("item is found");
    assert_eq!(item.first_captured_at, at("2026-07-25T14:03:22Z"));
    assert_eq!(item.last_captured_at, at("2026-07-29T14:03:22Z"));
}

#[test]
fn a_backfilled_older_capture_widens_the_item_window() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a-page").expect("valid url");

    archive
        .write_capture(page_capture(&url, at("2026-07-25T14:03:22Z"), PAGE))
        .expect("first capture");
    archive
        .write_capture(page_capture(
            &url,
            at("2026-01-04T08:00:00Z"),
            b"<html>older</html>",
        ))
        .expect("older capture written later");

    let item = archive.read_item(&url).expect("item is found");
    assert_eq!(item.first_captured_at, at("2026-01-04T08:00:00Z"));
    assert_eq!(item.last_captured_at, at("2026-07-25T14:03:22Z"));
}

#[test]
fn asking_for_what_was_never_archived_says_so() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a-page").expect("valid url");
    let other = CanonicalUrl::parse("https://example.com/never-fetched").expect("valid url");

    let capture = archive
        .write_capture(page_capture(&url, at("2026-07-25T14:03:22Z"), PAGE))
        .expect("capture is stored");

    assert!(matches!(
        archive.read_item(&other),
        Err(StorageError::NoSuchItem { .. })
    ));
    assert!(matches!(
        archive.read_capture(&other, &capture.id),
        Err(StorageError::NoSuchCapture { .. })
    ));
    assert_eq!(
        archive.list_captures(&other).expect("empty listing"),
        Vec::new()
    );
}

#[test]
fn a_directory_holding_something_else_is_not_adopted_as_an_archive() {
    let dir = TempDir::new().expect("temp dir");
    std::fs::write(dir.path().join("tax-returns.pdf"), b"not an archive").expect("write");

    assert!(matches!(
        Archive::open(dir.path()),
        Err(StorageError::NotAnArchive { .. })
    ));
}

#[test]
fn reopening_an_archive_keeps_reading_what_is_in_it() {
    let dir = TempDir::new().expect("temp dir");
    let url = CanonicalUrl::parse("https://example.com/a-page").expect("valid url");
    let capture = archive_in(&dir)
        .write_capture(page_capture(&url, at("2026-07-25T14:03:22Z"), PAGE))
        .expect("capture is stored");

    let reopened = Archive::open(dir.path()).expect("an existing archive opens again");
    assert_eq!(
        reopened.read_capture(&url, &capture.id).expect("capture"),
        capture
    );
}

fn stored_body_count(root: &std::path::Path) -> usize {
    fn walk(path: &std::path::Path, files: &mut usize) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                walk(&entry.path(), files);
            } else {
                *files += 1;
            }
        }
    }
    let mut files = 0;
    walk(&root.join("blobs"), &mut files);
    files
}

fn item_record_count(root: &std::path::Path) -> usize {
    fn walk(path: &std::path::Path, records: &mut usize) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                walk(&entry.path(), records);
            } else if entry.file_name() == "item.json" {
                *records += 1;
            }
        }
    }
    let mut records = 0;
    walk(&root.join("items"), &mut records);
    records
}

#[test]
fn two_fetches_in_the_same_second_with_the_same_bytes_stay_two_captures() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a-page").expect("valid url");
    let moment = at("2026-07-25T14:03:22Z");

    // A retry inside the same second can return identical bytes under a different status,
    // and the archive has to keep both: the pair is the evidence of what happened.
    let served = archive
        .write_capture(page_capture(&url, moment, PAGE))
        .expect("first capture");
    let mut failed = page_capture(&url, moment, PAGE);
    failed.status = 503;
    let failed = archive.write_capture(failed).expect("second capture");

    assert_ne!(served.id, failed.id);
    assert_eq!(archive.list_captures(&url).expect("listing").len(), 2);
    assert_eq!(
        archive
            .read_capture(&url, &served.id)
            .expect("first")
            .status,
        200
    );
}

/// The same bytes can be the whole page once and all that survived of it the next time,
/// and the two are different captures. Naming them alike would file the short one over the
/// complete one and leave the archive claiming a page it no longer holds.
#[test]
fn a_body_that_arrived_short_is_not_filed_over_the_complete_one() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a-page").expect("valid url");
    let moment = at("2026-07-25T14:03:22Z");

    let complete = archive
        .write_capture(page_capture(&url, moment, PAGE))
        .expect("complete capture");
    let mut short = page_capture(&url, moment, PAGE);
    short.body_truncated = true;
    let short = archive.write_capture(short).expect("short capture");

    assert_ne!(complete.id, short.id);
    assert_eq!(archive.list_captures(&url).expect("listing").len(), 2);
    assert!(
        archive
            .read_capture(&url, &short.id)
            .expect("short capture reads back")
            .body_truncated
    );
}

/// A capture written before the archive tracked shortfalls says nothing about one, and
/// nothing has to keep reading as the whole page rather than as an unreadable record.
#[test]
fn a_record_written_before_the_field_existed_still_reads() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a-page").expect("valid url");
    let capture = archive
        .write_capture(page_capture(&url, at("2026-07-25T14:03:22Z"), PAGE))
        .expect("capture is stored");

    let path = capture_file(dir.path(), &url, &capture.id);
    let record = std::fs::read_to_string(&path).expect("read record");
    let older: String = record
        .lines()
        .filter(|line| !line.contains("\"body_truncated\""))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !older.contains("body_truncated"),
        "the field was not removed"
    );
    std::fs::write(&path, older).expect("write record");

    let read_back = archive
        .read_capture(&url, &capture.id)
        .expect("an older record still reads");
    assert!(!read_back.body_truncated);
}

#[test]
fn writing_the_very_same_capture_twice_is_idempotent() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a-page").expect("valid url");
    let moment = at("2026-07-25T14:03:22Z");

    let first = archive
        .write_capture(page_capture(&url, moment, PAGE))
        .expect("first capture");
    let again = archive
        .write_capture(page_capture(&url, moment, PAGE))
        .expect("same capture again");

    assert_eq!(first, again);
    assert_eq!(archive.list_captures(&url).expect("listing").len(), 1);
}

#[test]
fn a_record_edited_to_point_outside_the_archive_is_refused() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a-page").expect("valid url");
    let capture = archive
        .write_capture(page_capture(&url, at("2026-07-25T14:03:22Z"), PAGE))
        .expect("capture is stored");

    // An archive is hostile input forever, not only while it is being written: the file
    // below is the one thing standing between a stored record and an arbitrary read.
    let path = capture_file(dir.path(), &url, &capture.id);
    let record = std::fs::read_to_string(&path).expect("read record");
    std::fs::write(
        &path,
        record.replace(capture.body.sha256.as_str(), "../../../../../../etc/passwd"),
    )
    .expect("write record");

    assert!(matches!(
        archive.read_capture(&url, &capture.id),
        Err(StorageError::MalformedRecord { .. })
    ));
}

#[test]
fn a_body_that_no_longer_matches_its_name_is_reported_and_not_returned() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a-page").expect("valid url");
    let capture = archive
        .write_capture(page_capture(&url, at("2026-07-25T14:03:22Z"), PAGE))
        .expect("capture is stored");

    let hash = capture.body.sha256.clone();
    std::fs::write(blob_file(dir.path(), &hash), b"corrupted").expect("overwrite blob");

    assert!(matches!(
        archive.read_body(&hash),
        Err(StorageError::CorruptBody { .. })
    ));
}

#[test]
fn a_body_a_record_references_but_the_archive_lacks_is_reported() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);

    assert!(matches!(
        archive.read_body(&ContentHash::of(b"never stored")),
        Err(StorageError::MissingBody { .. })
    ));
}

#[test]
fn an_archive_written_by_a_newer_format_is_not_guessed_at() {
    let dir = TempDir::new().expect("temp dir");
    std::fs::write(
        dir.path().join("archeion.json"),
        br#"{"format":"archeion-archive","version":99}"#,
    )
    .expect("write marker");

    assert!(matches!(
        Archive::open(dir.path()),
        Err(StorageError::UnreadableFormat { .. })
    ));
}

#[test]
fn a_temporary_file_left_by_a_crash_does_not_block_the_directory_forever() {
    let dir = TempDir::new().expect("temp dir");
    std::fs::write(dir.path().join(".4242.0.tmp"), b"half a marker").expect("write temp");

    assert!(Archive::open(dir.path()).is_ok());
}

#[test]
fn an_address_host_lands_in_a_directory_a_filesystem_accepts() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("http://[2001:db8::1]/a-page").expect("valid url");

    archive
        .write_capture(page_capture(&url, at("2026-07-25T14:03:22Z"), PAGE))
        .expect("capture is stored");

    assert!(
        dir.path()
            .join("items")
            .join("2001-db8--1")
            .join(ItemId::of(&url).as_str())
            .join("item.json")
            .is_file()
    );
}

fn capture_file(
    root: &std::path::Path,
    url: &CanonicalUrl,
    capture: &archeion::storage::CaptureId,
) -> std::path::PathBuf {
    root.join("items")
        .join(url.host_dir())
        .join(ItemId::of(url).as_str())
        .join("captures")
        .join(format!("{capture}.json"))
}

fn blob_file(root: &std::path::Path, hash: &ContentHash) -> std::path::PathBuf {
    root.join("blobs")
        .join("sha256")
        .join(&hash.as_str()[0..2])
        .join(&hash.as_str()[2..4])
        .join(hash.as_str())
}
