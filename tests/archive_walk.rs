//! Reading an archive back without being told what is in it.
//!
//! Every other way into the store starts from a canonical URL the caller already has. The
//! walk is the one that starts from the tree, which is what a listing and an export need,
//! and it is also the one that meets whatever a directory happens to hold: an archive is
//! untrusted input for as long as it exists, not only while it is being written.

use archeion::CanonicalUrl;
use archeion::storage::{Archive, Header, ItemId, NewCapture, UnreadableItem};
use jiff::Timestamp;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

fn at(instant: &str) -> Timestamp {
    instant.parse().expect("test timestamp is valid")
}

fn archive_in(dir: &TempDir) -> Archive {
    Archive::open(dir.path()).expect("a fresh directory opens as an archive")
}

/// A capture with nothing in it but the fields the store requires, since what is being
/// asserted here is the tree and never the response.
fn capture_of(url: &CanonicalUrl, fetched_at: Timestamp) -> NewCapture {
    NewCapture {
        canonical_url: url.clone(),
        requested_url: url.as_str().to_owned(),
        final_url: url.as_str().to_owned(),
        status: 200,
        media_type: Some("text/html".to_owned()),
        response_headers: vec![Header {
            name: "content-type".to_owned(),
            value: "text/html".to_owned(),
        }],
        body: b"<html><title>A page</title></html>".to_vec(),
        body_truncated: false,
        fetched_at,
        assets: Vec::new(),
        assets_missed: Vec::new(),
        policy_departures: Vec::new(),
    }
}

fn archive_with(dir: &TempDir, urls: &[&str]) -> Archive {
    let archive = archive_in(dir);
    for (index, url) in urls.iter().enumerate() {
        let url = CanonicalUrl::parse(url).expect("valid url");
        archive
            .write_capture(capture_of(&url, at("2026-07-25T14:03:22Z")))
            .expect("the capture is written");
        // A second capture of one of them, so the walk is not accidentally counting captures.
        if index == 0 {
            archive
                .write_capture(capture_of(&url, at("2026-07-26T09:00:00Z")))
                .expect("the second capture is written");
        }
    }
    archive
}

fn item_dir(root: &TempDir, url: &str) -> PathBuf {
    let url = CanonicalUrl::parse(url).expect("valid url");
    root.path()
        .join("items")
        .join(url.host_dir())
        .join(ItemId::of(&url).as_str())
}

#[test]
fn a_walk_finds_every_item_the_archive_holds() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_with(
        &dir,
        &[
            "https://example.com/second",
            "https://blog.example.com/first",
        ],
    );

    let walk = archive.walk().expect("the archive walks");

    let found: Vec<&str> = walk
        .items
        .iter()
        .map(|item| item.canonical_url.as_str())
        .collect();
    assert_eq!(
        found,
        [
            "https://blog.example.com/first",
            "https://example.com/second"
        ]
    );
    assert!(walk.unreadable.is_empty());
}

#[test]
fn items_come_back_ordered_by_address_and_not_by_the_order_the_tree_holds_them_in() {
    let dir = TempDir::new().expect("temp dir");
    // Two items under one host, so what sorts the directory listing is the hash of each
    // address rather than the address. The fixture is only worth anything if those two
    // orders disagree, so it says so out loud instead of leaving it to chance: a pair that
    // happened to agree would let the sort be deleted with the suite still green.
    let first = CanonicalUrl::parse("https://example.com/alpha").expect("valid url");
    let second = CanonicalUrl::parse("https://example.com/beta").expect("valid url");
    assert!(first.as_str() < second.as_str());
    assert!(
        ItemId::of(&first).as_str() > ItemId::of(&second).as_str(),
        "the tree must hold these two in the opposite order to their addresses"
    );
    let archive = archive_with(&dir, &[first.as_str(), second.as_str()]);

    let walk = archive.walk().expect("the archive walks");

    let found: Vec<&str> = walk
        .items
        .iter()
        .map(|item| item.canonical_url.as_str())
        .collect();
    assert_eq!(found, [first.as_str(), second.as_str()]);
}

#[test]
fn an_empty_archive_walks_to_nothing_rather_than_failing() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);

    let walk = archive
        .walk()
        .expect("an archive with no items still walks");

    assert!(walk.items.is_empty());
    assert!(walk.unreadable.is_empty());
}

/// The walk is not supposed to learn how the archive came to exist. A directory adopted
/// because it held only the rules file has to walk exactly like one adopted empty: no item,
/// no unreadable entry, and the rules file itself is not mistaken for either.
#[test]
fn a_directory_adopted_from_only_a_rules_file_walks_to_nothing_too() {
    let dir = TempDir::new().expect("temp dir");
    fs::write(dir.path().join("extraction-rules.json"), b"{}").expect("the rules file first");
    let archive =
        Archive::open(dir.path()).expect("a directory holding only the rules file is adopted");

    let walk = archive
        .walk()
        .expect("an archive with no items still walks");

    assert!(walk.items.is_empty());
    assert!(walk.unreadable.is_empty());
}

#[test]
fn an_item_directory_with_no_record_is_reported_and_not_passed_over() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_with(&dir, &["https://example.com/a-page"]);
    let orphaned = item_dir(&dir, "https://example.com/a-page");
    fs::remove_file(orphaned.join("item.json")).expect("the item record is removed");

    let walk = archive.walk().expect("the archive walks");

    assert!(walk.items.is_empty());
    // Captures with no item record beside them cannot be read back to an address, which is
    // exactly the loss the write order exists to prevent. Counting them as absent would
    // report the archive as empty when it is damaged.
    assert!(matches!(
        walk.unreadable.as_slice(),
        [UnreadableItem::NoRecord { path }] if path == &orphaned
    ));
}

#[test]
fn an_item_record_that_does_not_parse_is_reported() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_with(&dir, &["https://example.com/a-page"]);
    let damaged = item_dir(&dir, "https://example.com/a-page");
    fs::write(damaged.join("item.json"), b"{ this is not a record }")
        .expect("the record is overwritten");

    let walk = archive.walk().expect("the archive walks");

    assert!(walk.items.is_empty());
    // The item directory and not the record inside it, which is what every other variant
    // carries: a caller reporting these should not have to know which one it is holding.
    assert!(matches!(
        walk.unreadable.as_slice(),
        [UnreadableItem::Malformed { path, .. }] if path == &damaged
    ));
}

#[test]
fn an_item_record_is_refused_when_the_tree_disagrees_with_the_address_in_it() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_with(&dir, &["https://example.com/a-page"]);
    let record = fs::read(item_dir(&dir, "https://example.com/a-page").join("item.json"))
        .expect("the item record is read");

    // The same record filed under a host it does not belong to, and under an id its address
    // does not hash to. Either one means a lookup by URL lands somewhere other than where
    // the walk found the record, so the tree cannot be trusted to answer both ways.
    let wrong_host = dir.path().join("items").join("elsewhere.example.com").join(
        ItemId::of(&CanonicalUrl::parse("https://example.com/a-page").expect("valid url")).as_str(),
    );
    let wrong_id = dir
        .path()
        .join("items")
        .join("example.com")
        .join("0".repeat(64));
    for misfiled in [&wrong_host, &wrong_id] {
        fs::create_dir_all(misfiled).expect("the directory is created");
        fs::write(misfiled.join("item.json"), &record).expect("the record is copied");
    }

    let walk = archive.walk().expect("the archive walks");

    assert_eq!(
        walk.items.len(),
        1,
        "the correctly filed item is still read"
    );
    let mut reported: Vec<&PathBuf> = walk
        .unreadable
        .iter()
        .map(|entry| match entry {
            UnreadableItem::Misfiled { path, .. } => path,
            other => panic!("expected a misfiled item, got {other:?}"),
        })
        .collect();
    reported.sort();
    let mut expected = vec![&wrong_host, &wrong_id];
    expected.sort();
    assert_eq!(reported, expected);
}

#[test]
fn something_that_is_not_an_item_directory_is_not_an_unreadable_item() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_with(&dir, &["https://example.com/a-page"]);
    // A stray file is not a damaged record, and reporting every one as unreadable would
    // bury the entries that are.
    fs::write(dir.path().join("items").join(".DS_Store"), b"junk").expect("the file is written");
    fs::write(
        dir.path()
            .join("items")
            .join("example.com")
            .join("notes.txt"),
        b"junk",
    )
    .expect("the file is written");

    let walk = archive.walk().expect("the archive walks");

    assert_eq!(walk.items.len(), 1);
    assert!(walk.unreadable.is_empty());
}

#[test]
fn a_record_whose_own_id_is_not_its_address_is_refused_where_it_sits() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_with(&dir, &["https://example.com/a-page"]);
    let home = item_dir(&dir, "https://example.com/a-page");
    // The record is in the right directory and names an id that its own address does not
    // hash to. The directory alone would pass, so this is the case that says the `id` field
    // is read rather than taken on trust.
    let record = fs::read_to_string(home.join("item.json")).expect("the record is read");
    let tampered = record.replace(
        ItemId::of(&CanonicalUrl::parse("https://example.com/a-page").expect("valid url")).as_str(),
        &"0".repeat(64),
    );
    fs::write(home.join("item.json"), tampered).expect("the record is overwritten");

    let walk = archive.walk().expect("the archive walks");

    assert!(walk.items.is_empty());
    assert!(matches!(
        walk.unreadable.as_slice(),
        [UnreadableItem::Misfiled { path, .. }] if path == &home
    ));
}

#[test]
fn a_symbolic_link_where_an_item_belongs_is_reported_rather_than_followed() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_with(&dir, &["https://example.com/a-page"]);
    // Passing over this in silence would report the archive as whole while an item is
    // missing from it, and following it would let an edited tree aim the walk anywhere.
    let link = dir
        .path()
        .join("items")
        .join("example.com")
        .join("1".repeat(64));
    std::os::unix::fs::symlink(item_dir(&dir, "https://example.com/a-page"), &link)
        .expect("the link is created");

    let walk = archive.walk().expect("the archive walks");

    assert_eq!(walk.items.len(), 1);
    assert!(matches!(
        walk.unreadable.as_slice(),
        [UnreadableItem::Refused { path, .. }] if path == &link
    ));
}

#[test]
fn one_item_that_cannot_be_read_costs_that_item_and_not_the_listing() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_with(
        &dir,
        &["https://example.com/alpha", "https://example.com/beta"],
    );
    // A directory where the record belongs. Reading it fails as an IO error rather than as a
    // parse error, which is the whole class that must not be allowed to deny the listing:
    // an archive with a damaged corner is not an unreadable archive.
    let broken = item_dir(&dir, "https://example.com/alpha").join("item.json");
    fs::remove_file(&broken).expect("the record is removed");
    fs::create_dir(&broken).expect("a directory takes its place");

    let walk = archive.walk().expect("the archive still walks");

    assert_eq!(
        walk.items
            .iter()
            .map(|item| item.canonical_url.as_str())
            .collect::<Vec<_>>(),
        ["https://example.com/beta"]
    );
    assert!(matches!(
        walk.unreadable.as_slice(),
        [UnreadableItem::Refused { path, .. }]
            if path == &item_dir(&dir, "https://example.com/alpha")
    ));
}

#[test]
fn a_record_far_larger_than_a_record_can_be_is_refused_before_it_is_read() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_with(&dir, &["https://example.com/a-page"]);
    let home = item_dir(&dir, "https://example.com/a-page");
    // An item record is an id, an address and two instants. Anything at this size is a file
    // that is not one, and the walk is the first read that reaches a path nobody asked for.
    fs::write(home.join("item.json"), vec![b' '; 128 * 1024]).expect("the record is overwritten");

    let walk = archive.walk().expect("the archive walks");

    assert!(walk.items.is_empty());
    assert!(matches!(
        walk.unreadable.as_slice(),
        [UnreadableItem::Refused { path, .. }] if path == &home
    ));
}
