use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use archeion::CanonicalUrl;
use archeion::metadata::{Attributed, MetadataSource, PageMetadata, PublicationDate};
use archeion::readability::{AdmissionCost, Article, ArticleRecord, ExtractionRules};
use archeion::storage::{Archive, Asset, ContentHash, Header, ItemId, NewAsset, NewCapture};
use jiff::Timestamp;
use tempfile::TempDir;

fn at(instant: &str) -> Timestamp {
    instant.parse().expect("test timestamp is valid")
}

fn archeion() -> Command {
    Command::new(env!("CARGO_BIN_EXE_archeion"))
}

fn capture_of(url: &CanonicalUrl, fetched_at: Timestamp, marker: &str) -> NewCapture {
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
        body: format!("<html><title>{marker}</title><p>{marker}</p></html>").into_bytes(),
        body_truncated: false,
        fetched_at,
        assets: Vec::new(),
        assets_missed: Vec::new(),
    }
}

fn article(markdown: &str, word_count: usize, excerpt: Option<&str>) -> Article {
    Article {
        markdown: markdown.to_owned(),
        record: ArticleRecord {
            extractor_version: archeion::readability::EXTRACTOR_VERSION,
            rules: ExtractionRules::Heuristic,
            word_count,
            excerpt: excerpt.map(str::to_owned),
            byline: None,
            truncated: Vec::new(),
            cost: AdmissionCost {
                document_bytes: 512,
                peak_open_elements: 5,
            },
        },
    }
}

fn metadata(title: Option<&str>) -> PageMetadata {
    PageMetadata {
        extractor_version: archeion::metadata::EXTRACTOR_VERSION,
        title: title.map(value),
        description: None,
        author: Some(value("J. Writer")),
        site_name: Some(value("Example Site")),
        language: Some(value("en")),
        published_at: Some(PublicationDate {
            raw: "2026-07-24".to_owned(),
            timestamp: Some(at("2026-07-24T00:00:00Z")),
            source: MetadataSource::Html,
        }),
        declared_canonical_url: None,
        meta: Vec::new(),
        json_ld: Vec::new(),
        links: Vec::new(),
        assets: Vec::new(),
        truncated: Vec::new(),
    }
}

fn value(value: &str) -> Attributed {
    Attributed {
        value: value.to_owned(),
        source: MetadataSource::Html,
    }
}

fn write_article_capture(
    archive: &Archive,
    address: &str,
    fetched_at: &str,
    title: Option<&str>,
    markdown: &str,
) -> CanonicalUrl {
    let url = CanonicalUrl::parse(address).expect("valid url");
    let capture = archive
        .write_capture(capture_of(
            &url,
            at(fetched_at),
            title.unwrap_or("untitled"),
        ))
        .expect("capture is written");
    archive
        .write_metadata(&url, &capture.id, &metadata(title))
        .expect("metadata is written");
    archive
        .write_article(
            &url,
            &capture.id,
            &article(markdown, 4, Some("A short excerpt.")),
        )
        .expect("article is written");
    url
}

fn export_fixture() -> TempDir {
    let dir = TempDir::new().expect("temp dir");
    let archive = Archive::open(dir.path()).expect("archive opens");

    let first = CanonicalUrl::parse("https://blog.example.com/first").expect("valid url");
    let old = archive
        .write_capture(capture_of(
            &first,
            at("2026-07-25T14:03:22Z"),
            "Older article",
        ))
        .expect("older capture is written");
    archive
        .write_metadata(&first, &old.id, &metadata(Some("Older article")))
        .expect("older metadata is written");
    archive
        .write_article(
            &first,
            &old.id,
            &article("# Older article\n\nOlder prose.", 4, Some("Older prose.")),
        )
        .expect("older article is written");
    let latest = archive
        .write_capture(capture_of(
            &first,
            at("2026-07-26T09:00:00Z"),
            "Latest article",
        ))
        .expect("latest capture is written");
    archive
        .write_metadata(&first, &latest.id, &metadata(Some("Latest article")))
        .expect("latest metadata is written");
    archive
        .write_article(
            &first,
            &latest.id,
            &article(
                "# Latest article\n\nLatest prose.",
                4,
                Some("Latest prose."),
            ),
        )
        .expect("latest article is written");

    write_article_capture(
        &archive,
        "https://example.com/empty-title",
        "2026-07-25T14:03:22Z",
        Some(""),
        "An article whose title record is empty and links to [first](https://blog.example.com/first).",
    );
    write_article_capture(
        &archive,
        "https://example.com/hostile-title",
        "2026-07-25T14:03:22Z",
        Some("\"quoted\"\n---\n../secret"),
        "A hostile title stays data.",
    );
    write_article_capture(
        &archive,
        "https://example.com/same-a",
        "2026-07-25T14:03:22Z",
        Some("Same Title"),
        "The first colliding title.",
    );
    write_article_capture(
        &archive,
        "https://example.com/same-b",
        "2026-07-25T14:03:22Z",
        Some("Same Title"),
        "The second colliding title.",
    );

    dir
}

fn add_unreadable_item_directory(archive: &Path) {
    let broken = archive
        .join("items")
        .join("broken.example.com")
        .join("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
    std::fs::create_dir_all(broken).expect("broken item directory exists");
}

fn store_asset(
    archive: &Archive,
    requested_url: &str,
    final_url: &str,
    media_type: &str,
    body: &[u8],
) -> Asset {
    archive
        .store_asset(&NewAsset {
            requested_url: requested_url.to_owned(),
            final_url: final_url.to_owned(),
            status: 200,
            media_type: Some(media_type.to_owned()),
            body: body.to_vec(),
        })
        .expect("asset is stored")
}

fn stored_body_path(archive: &Path, hash: &ContentHash) -> std::path::PathBuf {
    archive
        .join("blobs")
        .join("sha256")
        .join(&hash.as_str()[0..2])
        .join(&hash.as_str()[2..4])
        .join(hash.as_str())
}

fn exported_tree(root: &Path) -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    collect_files(root, root, &mut files);
    files
}

fn collect_files(root: &Path, path: &Path, files: &mut BTreeMap<String, String>) {
    let mut entries: Vec<_> = std::fs::read_dir(path)
        .expect("directory reads")
        .map(|entry| entry.expect("entry reads").path())
        .collect();
    entries.sort();
    for entry in entries {
        if entry.is_dir() {
            collect_files(root, &entry, files);
        } else {
            let relative = entry
                .strip_prefix(root)
                .expect("entry is under root")
                .to_string_lossy()
                .replace('\\', "/");
            files.insert(
                relative,
                std::fs::read_to_string(&entry).expect("exported note reads"),
            );
        }
    }
}

#[test]
fn export_writes_a_markdown_vault_for_the_latest_article_capture_per_item() {
    let archive = export_fixture();
    let destination = TempDir::new().expect("temp dir");
    let destination_path = destination.path().join("vault");
    let colliding = CanonicalUrl::parse("https://example.com/same-b").expect("valid url");
    let colliding_id = ItemId::of(&colliding);
    let collision_suffix = &colliding_id.as_str()[..12];

    let output = archeion()
        .arg("export")
        .arg(archive.path())
        .arg(&destination_path)
        .output()
        .expect("the binary runs");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "exported 7 notes\n"
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    assert_eq!(
        exported_tree(&destination_path),
        BTreeMap::from([
            (
                "blog.example.com/2026-07-26-latest-article.md".to_owned(),
                "---\n\
                 title: \"Latest article\"\n\
                 canonical_url: \"https://blog.example.com/first\"\n\
                 captured_at: \"2026-07-26T09:00:00Z\"\n\
                 published_at: \"2026-07-24T00:00:00Z\"\n\
                 author: \"J. Writer\"\n\
                 site_name: \"Example Site\"\n\
                 language: \"en\"\n\
                 word_count: 4\n\
                 excerpt: \"Latest prose.\"\n\
                 ---\n\n\
                 # Latest article\n\n\
                 Latest prose."
                    .to_owned(),
            ),
            (
                "blog.example.com/index.md".to_owned(),
                "# blog.example.com\n\n\
                 - [2026-07-26-latest-article.md](2026-07-26-latest-article.md)\n"
                    .to_owned(),
            ),
            (
                "example.com/2026-07-25-empty-title.md".to_owned(),
                "---\n\
                 title: \"\"\n\
                 canonical_url: \"https://example.com/empty-title\"\n\
                 captured_at: \"2026-07-25T14:03:22Z\"\n\
                 published_at: \"2026-07-24T00:00:00Z\"\n\
                 author: \"J. Writer\"\n\
                 site_name: \"Example Site\"\n\
                 language: \"en\"\n\
                 word_count: 4\n\
                 excerpt: \"A short excerpt.\"\n\
                 ---\n\n\
                 An article whose title record is empty and links to [first](../blog.example.com/2026-07-26-latest-article.md)."
                    .to_owned(),
            ),
            (
                "example.com/2026-07-25-quoted-secret.md".to_owned(),
                "---\n\
                 title: \"\\\"quoted\\\"\\n---\\n../secret\"\n\
                 canonical_url: \"https://example.com/hostile-title\"\n\
                 captured_at: \"2026-07-25T14:03:22Z\"\n\
                 published_at: \"2026-07-24T00:00:00Z\"\n\
                 author: \"J. Writer\"\n\
                 site_name: \"Example Site\"\n\
                 language: \"en\"\n\
                 word_count: 4\n\
                 excerpt: \"A short excerpt.\"\n\
                 ---\n\n\
                 A hostile title stays data."
                    .to_owned(),
            ),
            (
                "example.com/2026-07-25-same-title.md".to_owned(),
                "---\n\
                 title: \"Same Title\"\n\
                 canonical_url: \"https://example.com/same-a\"\n\
                 captured_at: \"2026-07-25T14:03:22Z\"\n\
                 published_at: \"2026-07-24T00:00:00Z\"\n\
                 author: \"J. Writer\"\n\
                 site_name: \"Example Site\"\n\
                 language: \"en\"\n\
                 word_count: 4\n\
                 excerpt: \"A short excerpt.\"\n\
                 ---\n\n\
                 The first colliding title."
                    .to_owned(),
            ),
            (
                format!("example.com/2026-07-25-same-title-{collision_suffix}.md"),
                "---\n\
                 title: \"Same Title\"\n\
                 canonical_url: \"https://example.com/same-b\"\n\
                 captured_at: \"2026-07-25T14:03:22Z\"\n\
                 published_at: \"2026-07-24T00:00:00Z\"\n\
                 author: \"J. Writer\"\n\
                 site_name: \"Example Site\"\n\
                 language: \"en\"\n\
                 word_count: 4\n\
                 excerpt: \"A short excerpt.\"\n\
                 ---\n\n\
                 The second colliding title."
                    .to_owned(),
            ),
            (
                "example.com/index.md".to_owned(),
                format!(
                    "# example.com\n\n\
                     - [2026-07-25-empty-title.md](2026-07-25-empty-title.md)\n\
                     - [2026-07-25-quoted-secret.md](2026-07-25-quoted-secret.md)\n\
                     - [2026-07-25-same-title.md](2026-07-25-same-title.md)\n\
                     - [2026-07-25-same-title-{collision_suffix}.md](2026-07-25-same-title-{collision_suffix}.md)\n"
                ),
            ),
        ])
    );
}

#[test]
fn export_rewrites_links_to_notes_and_writes_a_host_index() {
    let dir = TempDir::new().expect("temp dir");
    let archive = Archive::open(dir.path()).expect("archive opens");

    write_article_capture(
        &archive,
        "https://example.com/alpha",
        "2026-07-26T23:00:00Z",
        Some("Alpha"),
        "Alpha cites [Beta](https://www.example.com/beta?utm_source=news#section), [Gamma](https://blog.example.com/gamma), [outside](https://outside.example.com/page) and [relative](/beta).",
    );
    write_article_capture(
        &archive,
        "https://example.com/beta",
        "2026-07-26T01:00:00Z",
        Some("Beta"),
        "Beta cites [Alpha](https://example.com/alpha#top).",
    );
    write_article_capture(
        &archive,
        "https://blog.example.com/gamma",
        "2026-07-27T10:00:00Z",
        Some("Gamma"),
        "Gamma cites [Alpha](https://example.com/alpha).",
    );

    let destination = TempDir::new().expect("temp dir");
    let destination_path = destination.path().join("vault");
    let output = archeion()
        .arg("export")
        .arg(dir.path())
        .arg(&destination_path)
        .output()
        .expect("the binary runs");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "exported 5 notes\n"
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");

    let alpha = std::fs::read_to_string(destination_path.join("example.com/2026-07-26-alpha.md"))
        .expect("alpha note reads");
    assert!(
        alpha.contains(
            "Alpha cites [Beta](2026-07-26-beta.md), [Gamma](../blog.example.com/2026-07-27-gamma.md), [outside](https://outside.example.com/page) and [relative](/beta)."
        ),
        "{alpha}"
    );

    let beta = std::fs::read_to_string(destination_path.join("example.com/2026-07-26-beta.md"))
        .expect("beta note reads");
    assert!(
        beta.contains("Beta cites [Alpha](2026-07-26-alpha.md)."),
        "{beta}"
    );

    let gamma =
        std::fs::read_to_string(destination_path.join("blog.example.com/2026-07-27-gamma.md"))
            .expect("gamma note reads");
    assert!(
        gamma.contains("Gamma cites [Alpha](../example.com/2026-07-26-alpha.md)."),
        "{gamma}"
    );

    let index = std::fs::read_to_string(destination_path.join("example.com/index.md"))
        .expect("index reads");
    assert_eq!(
        index,
        "# example.com\n\n\
         - [2026-07-26-alpha.md](2026-07-26-alpha.md)\n\
         - [2026-07-26-beta.md](2026-07-26-beta.md)\n"
    );
}

#[test]
fn export_carries_referenced_article_images_as_content_hashed_assets() {
    let dir = TempDir::new().expect("temp dir");
    let archive = Archive::open(dir.path()).expect("archive opens");
    let url = CanonicalUrl::parse("https://example.com/images").expect("valid url");
    let png = b"png bytes";
    let jpeg = b"jpeg bytes";
    let webp = b"webp bytes";
    let accented = b"accented image";
    let css = b"body { color: red; }";
    let unused = b"unused image";
    let pdf = b"pdf bytes";

    let mut capture = capture_of(&url, at("2026-07-27T10:00:00Z"), "Images");
    capture.assets = vec![
        store_asset(
            &archive,
            "https://example.com/a",
            "https://cdn.example.com/a.png",
            "image/png",
            png,
        ),
        store_asset(
            &archive,
            "https://en.wikipedia.org/wiki/Foo_(bar)",
            "https://en.wikipedia.org/wiki/Foo_(bar)",
            "image/jpeg",
            jpeg,
        ),
        store_asset(
            &archive,
            "https://example.com/a b",
            "https://example.com/a b",
            "image/webp",
            webp,
        ),
        store_asset(
            &archive,
            "https://example.com/img/caf%C3%A9.png",
            "https://example.com/img/caf%C3%A9.png",
            "image/png",
            accented,
        ),
        store_asset(
            &archive,
            "https://example.com/style.css",
            "https://example.com/style.css",
            "text/css",
            css,
        ),
        store_asset(
            &archive,
            "https://example.com/unused.png",
            "https://example.com/unused.png",
            "image/png",
            unused,
        ),
        store_asset(
            &archive,
            "https://example.com/file.pdf",
            "https://example.com/file.pdf",
            "application/pdf",
            pdf,
        ),
    ];
    let capture = archive.write_capture(capture).expect("capture is written");
    archive
        .write_metadata(&url, &capture.id, &metadata(Some("Images")))
        .expect("metadata is written");
    archive
        .write_article(
            &url,
            &capture.id,
            &article(
                "`literal [text](https://example.com/x) here`\n\n\
                 ```\n\
                 see [text](https://example.com/x) in code\n\
                 ```\n\n\
                 ![this](https://example.com/a \"the title\")\n\n\
                 ![wiki](https://en.wikipedia.org/wiki/Foo_\\(bar\\))\n\n\
                 ![spaced](<https://example.com/a b>)\n\n\
                 ![a \\[b\\] c](https://example.com/a)\n\n\
                 ![accented](https://example.com/img/café.png)\n\n\
                 ![pdf](https://example.com/file.pdf)\n\n\
                 ![missing](https://example.com/missing.png)",
                20,
                Some("Images."),
            ),
        )
        .expect("article is written");

    let destination = TempDir::new().expect("temp dir");
    let destination_path = destination.path().join("vault");
    let output = archeion()
        .arg("export")
        .arg(dir.path())
        .arg(&destination_path)
        .output()
        .expect("the binary runs");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "exported 2 notes\n"
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");

    let png_name = format!("{}.png", ContentHash::of(png).as_str());
    let jpeg_name = format!("{}.jpg", ContentHash::of(jpeg).as_str());
    let webp_name = format!("{}.webp", ContentHash::of(webp).as_str());
    let accented_name = format!("{}.png", ContentHash::of(accented).as_str());
    let mut asset_names: Vec<_> = std::fs::read_dir(destination_path.join("assets"))
        .expect("asset dir reads")
        .map(|entry| {
            entry
                .expect("asset entry reads")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    asset_names.sort();
    assert_eq!(asset_names, {
        let mut names = vec![
            accented_name.clone(),
            jpeg_name.clone(),
            png_name.clone(),
            webp_name.clone(),
        ];
        names.sort();
        names
    });
    assert_eq!(
        std::fs::read(destination_path.join("assets").join(&png_name)).expect("png reads"),
        png
    );
    assert_eq!(
        std::fs::read(destination_path.join("assets").join(&jpeg_name)).expect("jpeg reads"),
        jpeg
    );
    assert_eq!(
        std::fs::read(destination_path.join("assets").join(&webp_name)).expect("webp reads"),
        webp
    );
    assert_eq!(
        std::fs::read(destination_path.join("assets").join(&accented_name))
            .expect("accented image reads"),
        accented
    );

    let note = std::fs::read_to_string(destination_path.join("example.com/2026-07-27-images.md"))
        .expect("note reads");
    assert_eq!(
        note,
        format!(
            "---\n\
             title: \"Images\"\n\
             canonical_url: \"https://example.com/images\"\n\
             captured_at: \"2026-07-27T10:00:00Z\"\n\
             published_at: \"2026-07-24T00:00:00Z\"\n\
             author: \"J. Writer\"\n\
             site_name: \"Example Site\"\n\
             language: \"en\"\n\
             word_count: 20\n\
             excerpt: \"Images.\"\n\
             ---\n\n\
             `literal [text](https://example.com/x) here`\n\n\
             ```\n\
             see [text](https://example.com/x) in code\n\
             ```\n\n\
             ![this](../assets/{png_name} \"the title\")\n\n\
             ![wiki](../assets/{jpeg_name})\n\n\
             ![spaced](<../assets/{webp_name}>)\n\n\
             ![a \\[b\\] c](../assets/{png_name})\n\n\
             ![accented](../assets/{accented_name})\n\n\
             ![pdf](https://example.com/file.pdf)\n\n\
             ![missing](https://example.com/missing.png)"
        )
    );
}

#[test]
fn export_keeps_the_note_when_a_referenced_image_body_is_missing() {
    let dir = TempDir::new().expect("temp dir");
    let archive = Archive::open(dir.path()).expect("archive opens");
    let url = CanonicalUrl::parse("https://example.com/missing-asset").expect("valid url");
    let body = b"image bytes";
    let asset = store_asset(
        &archive,
        "https://example.com/image.png",
        "https://example.com/image.png",
        "image/png",
        body,
    );

    let mut capture = capture_of(&url, at("2026-07-27T11:00:00Z"), "Missing asset");
    capture.assets = vec![asset.clone()];
    let capture = archive.write_capture(capture).expect("capture is written");
    archive
        .write_metadata(&url, &capture.id, &metadata(Some("Missing asset")))
        .expect("metadata is written");
    archive
        .write_article(
            &url,
            &capture.id,
            &article(
                "The prose survives.\n\n![image](https://example.com/image.png)",
                5,
                Some("The prose survives."),
            ),
        )
        .expect("article is written");
    std::fs::remove_file(stored_body_path(dir.path(), &asset.body.sha256))
        .expect("asset body is removed");

    let destination = TempDir::new().expect("temp dir");
    let destination_path = destination.path().join("vault");
    let output = archeion()
        .arg("export")
        .arg(dir.path())
        .arg(&destination_path)
        .output()
        .expect("the binary runs");

    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "exported 2 notes\n"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("warning: capture "), "{stderr}");
    assert!(stderr.contains(asset.body.sha256.as_str()), "{stderr}");
    assert!(
        stderr.contains("archive has 1 unreadable item(s)"),
        "{stderr}"
    );
    assert!(
        !destination_path.join("assets").exists(),
        "no asset was copied"
    );
    let note =
        std::fs::read_to_string(destination_path.join("example.com/2026-07-27-missing-asset.md"))
            .expect("note reads");
    assert!(note.contains("The prose survives."));
    assert!(note.contains("![image](https://example.com/image.png)"));
}

#[test]
fn export_all_captures_includes_the_article_history() {
    let archive = export_fixture();
    let destination = TempDir::new().expect("temp dir");
    let destination_path = destination.path().join("vault");

    let output = archeion()
        .arg("export")
        .arg("--all-captures")
        .arg(archive.path())
        .arg(&destination_path)
        .output()
        .expect("the binary runs");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "exported 8 notes\n"
    );
    let tree = exported_tree(&destination_path);
    assert!(tree.contains_key("blog.example.com/2026-07-25-older-article.md"));
    assert!(tree.contains_key("blog.example.com/2026-07-26-latest-article.md"));
    assert!(
        tree["example.com/2026-07-25-empty-title.md"]
            .contains("[first](../blog.example.com/2026-07-26-latest-article.md)")
    );
}

#[test]
fn export_writes_intact_items_and_reports_unreadable_item_directories() {
    let archive = export_fixture();
    add_unreadable_item_directory(archive.path());
    let destination = TempDir::new().expect("temp dir");
    let destination_path = destination.path().join("vault");

    let output = archeion()
        .arg("export")
        .arg(archive.path())
        .arg(&destination_path)
        .output()
        .expect("the binary runs");

    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "exported 7 notes\n"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("warning: "), "{stderr}");
    assert!(stderr.contains("holds no item record"), "{stderr}");
    assert!(
        stderr.contains("archive has 1 unreadable item(s)"),
        "{stderr}"
    );
    assert!(
        exported_tree(&destination_path)
            .contains_key("blog.example.com/2026-07-26-latest-article.md")
    );
}

#[cfg(unix)]
#[test]
fn export_accepts_a_symlink_to_an_empty_destination_directory() {
    let archive = export_fixture();
    let destination = TempDir::new().expect("temp dir");
    let real = destination.path().join("real-vault");
    let linked = destination.path().join("linked-vault");
    std::fs::create_dir(&real).expect("real destination exists");
    std::os::unix::fs::symlink(&real, &linked).expect("destination symlink exists");

    let output = archeion()
        .arg("export")
        .arg(archive.path())
        .arg(&linked)
        .output()
        .expect("the binary runs");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "exported 7 notes\n"
    );
    assert!(exported_tree(&real).contains_key("blog.example.com/2026-07-26-latest-article.md"));
}

#[test]
fn export_refuses_to_write_into_a_non_empty_destination() {
    let archive = export_fixture();
    let destination = TempDir::new().expect("temp dir");
    let existing = destination.path().join("vault");
    std::fs::create_dir(&existing).expect("destination directory exists");
    std::fs::write(existing.join("note.md"), "kept").expect("existing file is written");

    let output = archeion()
        .arg("export")
        .arg(archive.path())
        .arg(&existing)
        .output()
        .expect("the binary runs");

    assert!(!output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        format!(
            "{} exists and is not an empty directory\n",
            existing.display()
        )
    );
    assert_eq!(
        std::fs::read_to_string(existing.join("note.md")).expect("existing file reads"),
        "kept"
    );
}
