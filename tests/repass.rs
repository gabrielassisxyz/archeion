use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::ControlFlow;

use archeion::CanonicalUrl;
use archeion::crawl::{
    CrawlEngine, CrawlError, CrawlOutcome, FetchFailure, PageEvent, PageResponse, Seed,
};
use archeion::metadata::{self, PageSource};
use archeion::readability::{
    self, AdmissionCost, Article, ArticleBound, ArticleRecord, ExtractionRules, ProseShare,
    RefusedExtraction, SiteRules,
};
use archeion::repass::{RepassOptions, repass_archive};
use archeion::storage::{Archive, AssetMiss, Header, MissedAsset, NewCapture};
use jiff::Timestamp;
use tempfile::TempDir;

struct ScriptedEngine {
    subresources: HashMap<String, PageEvent>,
    fetched: RefCell<Vec<String>>,
}

impl ScriptedEngine {
    fn new(subresources: Vec<PageEvent>) -> Self {
        let mut by_url = HashMap::new();
        for event in subresources {
            let url = match &event {
                PageEvent::Response(response) => response.requested_url.clone(),
                PageEvent::NoResponse(failure) => failure.url.clone(),
            };
            by_url.insert(url, event);
        }
        Self {
            subresources: by_url,
            fetched: RefCell::new(Vec::new()),
        }
    }

    fn fetched(&self) -> Vec<String> {
        self.fetched.borrow().clone()
    }
}

impl CrawlEngine for ScriptedEngine {
    fn check_seed(&self, _seed: &Seed) -> Result<(), CrawlError> {
        Ok(())
    }

    fn crawl(
        &self,
        _seed: &Seed,
        _on_page: &mut dyn FnMut(PageEvent) -> ControlFlow<()>,
    ) -> Result<CrawlOutcome, CrawlError> {
        Ok(CrawlOutcome::default())
    }

    fn fetch(&self, url: &str, _seed: &Seed) -> PageEvent {
        self.fetched.borrow_mut().push(url.to_owned());
        self.subresources.get(url).cloned().unwrap_or_else(|| {
            PageEvent::NoResponse(FetchFailure {
                url: url.to_owned(),
                reason: "this fake has no response for the URL".to_owned(),
            })
        })
    }
}

fn archive_in(dir: &TempDir) -> Archive {
    Archive::open(dir.path()).expect("archive opens")
}

fn at(text: &str) -> Timestamp {
    text.parse().expect("valid timestamp")
}

fn html_capture(url: &CanonicalUrl, body: &str) -> NewCapture {
    NewCapture {
        canonical_url: url.clone(),
        requested_url: url.to_string(),
        final_url: url.to_string(),
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

fn markdown_capture(url: &CanonicalUrl, body: &str) -> NewCapture {
    NewCapture {
        media_type: Some("text/markdown".to_owned()),
        response_headers: vec![Header {
            name: "content-type".to_owned(),
            value: "text/markdown; charset=utf-8".to_owned(),
        }],
        ..html_capture(url, body)
    }
}

fn article_record(version: u32, rules: ExtractionRules) -> ArticleRecord {
    ArticleRecord {
        extractor_version: version,
        rules,
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
    }
}

fn old_article() -> Article {
    Article {
        markdown: "# Old\n\nOld prose.".to_owned(),
        record: article_record(1, ExtractionRules::Heuristic),
    }
}

fn article_page(extra: &str) -> String {
    format!(
        "<html><head><title>A page</title></head><body><article>{extra}{}</article></body></html>",
        "<p>Bread is mostly patience, and the dough will tell you when it is ready.</p>".repeat(8)
    )
}

/// What makes the served document retroactive. A capture stored as Markdown before the
/// extractor could read one has no article beside it, and that absence is what a repass has to
/// recognise as an answer nobody has given yet rather than as a response with nothing in it.
///
/// It is also the whole retroactive path: nothing is fetched, the stored response is untouched,
/// and its hash is the same afterwards.
#[test]
fn a_response_stored_as_markdown_before_it_could_be_read_becomes_an_article() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/posts/one.md").expect("valid URL");
    let capture = archive
        .write_capture(markdown_capture(
            &url,
            "# The oven is fixed\n\nThe element went in this morning.\n",
        ))
        .expect("capture is written");
    let body_hash = capture.body.sha256.clone();
    let engine = ScriptedEngine::new(Vec::new());

    let run = repass_archive(
        &engine,
        &archive,
        &SiteRules::default(),
        RepassOptions::default(),
    )
    .expect("repass succeeds");

    assert_eq!(run.articles_written, 1);
    assert_eq!(engine.fetched(), Vec::<String>::new());
    let article = archive
        .read_article(&url, &capture.id)
        .expect("article read succeeds")
        .expect("an article was written");
    assert_eq!(article.record.rules, ExtractionRules::Served);
    assert!(
        article
            .markdown
            .contains("The element went in this morning")
    );
    assert_eq!(
        archive
            .read_capture(&url, &capture.id)
            .expect("capture reads")
            .body
            .sha256,
        body_hash
    );

    // The second pass has nothing left to do, which is what says the answer was recorded rather
    // than merely produced.
    let again = repass_archive(
        &engine,
        &archive,
        &SiteRules::default(),
        RepassOptions::default(),
    )
    .expect("repass succeeds");
    assert_eq!(again.articles_written, 0);
    assert_eq!(again.derived_unchanged, 1);
}

/// The metadata extractor reads tags, and a Markdown document has none. Sending it after one
/// anyway would report the same unreadable page on every pass forever, so the two questions
/// stay two predicates.
#[test]
fn a_markdown_capture_is_never_sent_to_the_metadata_extractor() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/posts/one.md").expect("valid URL");
    let capture = archive
        .write_capture(markdown_capture(
            &url,
            "# A post\n\nProse enough to keep.\n",
        ))
        .expect("capture is written");

    let run = repass_archive(
        &ScriptedEngine::new(Vec::new()),
        &archive,
        &SiteRules::default(),
        RepassOptions::default(),
    )
    .expect("repass succeeds");

    assert_eq!(run.metadata_written, 0);
    assert_eq!(run.unreadable_pages, Vec::new());
    assert!(
        archive
            .read_metadata(&url, &capture.id)
            .expect("metadata read succeeds")
            .is_none()
    );
}

#[test]
fn a_stale_article_that_is_now_a_listing_stops_reading_as_one() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
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
        .write_article(&url, &capture.id, &old_article())
        .expect("old article is written");
    let body_hash = capture.body.sha256.clone();

    let run = repass_archive(
        &ScriptedEngine::new(Vec::new()),
        &archive,
        &SiteRules::default(),
        RepassOptions::default(),
    )
    .expect("repass succeeds");

    assert_eq!(run.non_articles_marked, 1);
    assert_eq!(
        archive
            .read_capture(&url, &capture.id)
            .expect("capture reads")
            .body
            .sha256,
        body_hash
    );
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

#[test]
fn an_oversized_article_state_record_is_rebuilt_by_repass() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a-page").expect("valid URL");
    let byline = "A".repeat(70 * 1024);
    let body = format!(
        "<html><head><title>A page</title><meta name=\"author\" content=\"{byline}\"></head>\
         <body><article>{}</article></body></html>",
        "<p>Bread is mostly patience, and the dough will tell you when it is ready.</p>".repeat(8)
    );
    let capture = archive
        .write_capture(html_capture(&url, &body))
        .expect("capture is written");
    archive
        .write_refused_extraction(
            &url,
            &capture.id,
            &RefusedExtraction {
                extractor_version: readability::EXTRACTOR_VERSION,
                rules: ExtractionRules::Heuristic,
                share: ProseShare {
                    article_chars: 137,
                    page_chars: 1188,
                },
                excerpt: Some(byline),
                truncated: Vec::new(),
            },
        )
        .expect("old oversized refusal is written");

    let run = repass_archive(
        &ScriptedEngine::new(Vec::new()),
        &archive,
        &SiteRules::default(),
        RepassOptions::default(),
    )
    .expect("repass succeeds");

    assert_eq!(run.articles_written, 1);
    let article = archive
        .read_article(&url, &capture.id)
        .expect("rebuilt article reads")
        .expect("article was rebuilt");
    assert_eq!(article.record.byline, None);
    assert_eq!(article.record.truncated, [ArticleBound::Byline]);
    assert!(
        archive
            .read_refused_extraction(&url, &capture.id)
            .expect("old refusal does not remain readable")
            .is_none()
    );
}

#[test]
fn a_host_rule_applies_to_a_capture_that_was_already_stored() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a").expect("valid URL");
    let capture = archive
        .write_capture(html_capture(
            &url,
            &article_page(
                "<aside class=\"promo\"><p>This subscription pitch is not part of the article.</p></aside>",
            ),
        ))
        .expect("capture is written");
    let metadata = metadata::extract(metadata::PageSource {
        body: &archive
            .read_body(&capture.body.sha256)
            .expect("body is stored"),
        content_type: Some("text/html; charset=utf-8"),
        final_url: url.as_str(),
    })
    .expect("metadata reads")
    .expect("HTML has metadata");
    archive
        .write_metadata(&url, &capture.id, &metadata)
        .expect("metadata is written");
    archive
        .write_article(
            &url,
            &capture.id,
            &Article {
                markdown: "# A page\n\nThis subscription pitch is not part of the article.\n\nBread is mostly patience.".to_owned(),
                record: article_record(readability::EXTRACTOR_VERSION, ExtractionRules::Heuristic),
            },
        )
        .expect("heuristic article is written");
    let (rules, unused) = SiteRules::parse(
        r#"{"hosts": {"example.com": {"strip": ["aside.promo"]}}}"#,
        "test",
    );
    assert!(unused.is_empty(), "{unused:?}");

    let run = repass_archive(
        &ScriptedEngine::new(Vec::new()),
        &archive,
        &rules,
        RepassOptions::default(),
    )
    .expect("repass succeeds");

    assert_eq!(run.articles_written, 1);
    let article = archive
        .read_article(&url, &capture.id)
        .expect("article reads")
        .expect("article exists");
    assert_eq!(
        article.record.rules,
        ExtractionRules::Site("example.com".to_owned())
    );
    assert!(!article.markdown.contains("subscription pitch"));
}

/// The captures that motivated the declaration are the ones already on disk, taken before
/// anything read it. Their responses carry the answer, so a pass has to find them: an article
/// at the current version whose record says nothing, over a page that did say something, is
/// stale for that reason alone.
///
/// This is the absence being answered by the pass rather than by the extractor version, which
/// would have rewritten every article in the archive to reach the handful this applies to.
#[test]
fn an_article_stored_before_the_declaration_was_read_is_re_read_from_the_page_that_declared_it() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a").expect("valid URL");
    let declaration = r#"<script type="application/ld+json">
        {"@type": "Article", "isAccessibleForFree": false}
        </script>"#;
    let capture = archive
        .write_capture(html_capture(&url, &article_page(declaration)))
        .expect("capture is written");
    archive
        .write_metadata(
            &url,
            &capture.id,
            &metadata::extract(PageSource {
                body: article_page(declaration).as_bytes(),
                content_type: Some("text/html; charset=utf-8"),
                final_url: url.as_str(),
            })
            .expect("the page reads")
            .expect("the page is html"),
        )
        .expect("metadata is written");
    archive
        .write_article(
            &url,
            &capture.id,
            &Article {
                markdown: "# A page\n\nBread is mostly patience.".to_owned(),
                record: article_record(readability::EXTRACTOR_VERSION, ExtractionRules::Heuristic),
            },
        )
        .expect("an article with no declaration on it is written");

    let run = repass_archive(
        &ScriptedEngine::new(Vec::new()),
        &archive,
        &SiteRules::default(),
        RepassOptions::default(),
    )
    .expect("repass succeeds");

    assert_eq!(run.articles_written, 1);
    let article = archive
        .read_article(&url, &capture.id)
        .expect("article reads")
        .expect("article exists");
    assert_eq!(article.record.accessible_for_free, Some(false));
}

#[test]
fn deleting_a_host_rule_makes_the_site_derived_article_stale() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a").expect("valid URL");
    let capture = archive
        .write_capture(html_capture(&url, &article_page("")))
        .expect("capture is written");
    archive
        .write_article(
            &url,
            &capture.id,
            &Article {
                markdown: "# A page\n\nBread is mostly patience.".to_owned(),
                record: article_record(
                    readability::EXTRACTOR_VERSION,
                    ExtractionRules::Site("example.com".to_owned()),
                ),
            },
        )
        .expect("site-derived article is written");

    let run = repass_archive(
        &ScriptedEngine::new(Vec::new()),
        &archive,
        &SiteRules::default(),
        RepassOptions::default(),
    )
    .expect("repass succeeds");

    assert_eq!(run.articles_written, 1);
    let article = archive
        .read_article(&url, &capture.id)
        .expect("article reads")
        .expect("article exists");
    assert_eq!(article.record.rules, ExtractionRules::Heuristic);
}

#[test]
fn a_refusal_replaces_an_existing_article_pair() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a").expect("valid URL");
    let capture = archive
        .write_capture(html_capture(&url, &article_page("")))
        .expect("capture is written");
    archive
        .write_article(&url, &capture.id, &old_article())
        .expect("old article is written");

    archive
        .write_refused_extraction(
            &url,
            &capture.id,
            &RefusedExtraction {
                extractor_version: readability::EXTRACTOR_VERSION,
                rules: ExtractionRules::Heuristic,
                share: ProseShare {
                    article_chars: 100,
                    page_chars: 1000,
                },
                excerpt: Some("Too small".to_owned()),
                truncated: Vec::new(),
            },
        )
        .expect("refusal is written");

    assert!(
        archive
            .read_article(&url, &capture.id)
            .expect("article read succeeds")
            .is_none()
    );
    assert!(
        archive
            .read_refused_extraction(&url, &capture.id)
            .expect("refusal reads")
            .is_some()
    );
}

#[test]
fn an_asset_missed_by_archive_policy_is_recovered_without_rewriting_the_capture_record() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a").expect("valid URL");
    let mut new = html_capture(&url, &article_page(""));
    new.assets_missed = vec![
        MissedAsset {
            url: "https://example.com/style.css".to_owned(),
            reason: AssetMiss::CountCeilingReached,
        },
        MissedAsset {
            url: "https://example.com/gone.css".to_owned(),
            reason: AssetMiss::NoResponse {
                detail: "dns error".to_owned(),
            },
        },
    ];
    let capture = archive.write_capture(new).expect("capture is written");
    let capture_path = dir
        .path()
        .join("items")
        .join("example.com")
        .join(archeion::storage::ItemId::of(&url).as_str())
        .join("captures")
        .join(format!("{}.json", capture.id));
    let original_record = std::fs::read_to_string(&capture_path).expect("capture record reads");
    let engine = ScriptedEngine::new(vec![PageEvent::Response(PageResponse {
        requested_url: "https://example.com/style.css".to_owned(),
        final_url: "https://example.com/style.css".to_owned(),
        status: 200,
        headers: vec![Header {
            name: "content-type".to_owned(),
            value: "text/css".to_owned(),
        }],
        body: b"body { color: black }".to_vec(),
        body_truncated: false,
        fetched_at: at("2026-07-25T14:03:23Z"),
    })]);

    let run = repass_archive(
        &engine,
        &archive,
        &SiteRules::default(),
        RepassOptions::default(),
    )
    .expect("repass succeeds");

    assert_eq!(engine.fetched(), ["https://example.com/style.css"]);
    assert_eq!(run.assets_recovered, 1);
    assert_eq!(run.assets_not_retried, 1);
    let read_back = archive
        .read_capture(&url, &capture.id)
        .expect("capture reads");
    assert_eq!(read_back.assets.len(), 1);
    assert_eq!(
        read_back.assets[0].requested_url,
        "https://example.com/style.css"
    );
    assert_eq!(
        read_back
            .assets_missed
            .iter()
            .map(|miss| &miss.url)
            .collect::<Vec<_>>(),
        [&"https://example.com/gone.css".to_owned()]
    );
    assert_eq!(
        std::fs::read_to_string(&capture_path).expect("capture record reads"),
        original_record
    );
}

/// The order the asset pass buys references in did not reach a repass at all. Every retryable
/// miss was handed over as an image, so the priority was uniform, the stable sort left the
/// record's own order in place, and a page with more retryable misses than one pass deals with
/// bought whatever it happened to list first. The role is in the metadata record beside the
/// capture, which is where this reads it back from.
#[test]
fn a_repass_asks_for_the_pictures_of_a_page_before_its_scripts() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a").expect("valid URL");
    let page = article_page(r#"<script src="/bundle.js"></script><img src="/photo.jpeg">"#);
    let mut new = html_capture(&url, &page);
    // The page's own order, which is the order a repass used to buy them back in.
    new.assets_missed = vec![
        MissedAsset {
            url: "https://example.com/bundle.js".to_owned(),
            reason: AssetMiss::CountCeilingReached,
        },
        MissedAsset {
            url: "https://example.com/photo.jpeg".to_owned(),
            reason: AssetMiss::CountCeilingReached,
        },
    ];
    let capture = archive.write_capture(new).expect("capture is written");
    let metadata = metadata::extract(PageSource {
        body: page.as_bytes(),
        content_type: Some("text/html; charset=utf-8"),
        final_url: url.as_str(),
    })
    .expect("the page reads")
    .expect("the page is html");
    archive
        .write_metadata(&url, &capture.id, &metadata)
        .expect("metadata is written");
    let engine = ScriptedEngine::new(Vec::new());

    repass_archive(
        &engine,
        &archive,
        &SiteRules::default(),
        RepassOptions::default(),
    )
    .expect("repass succeeds");

    assert_eq!(
        engine.fetched(),
        [
            "https://example.com/photo.jpeg",
            "https://example.com/bundle.js",
        ]
    );
}

/// A capture record is an observation and never changes; the metadata beside it is re-derived
/// under whichever extractor runs last. The two therefore stop naming the same set as soon as a
/// rule about which references are recorded changes, and a `srcset`'s narrower renditions are
/// exactly that case. An address the record no longer names is still retried, and still retried
/// as content: a lookup that came up empty says nothing about what the page was.
#[test]
fn a_missed_address_the_metadata_no_longer_names_is_still_retried() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a").expect("valid URL");
    let page = article_page(r#"<img srcset="/small.jpeg 424w, /large.jpeg 1456w">"#);
    let mut new = html_capture(&url, &page);
    new.assets_missed = vec![MissedAsset {
        url: "https://example.com/small.jpeg".to_owned(),
        reason: AssetMiss::CountCeilingReached,
    }];
    let capture = archive.write_capture(new).expect("capture is written");
    let metadata = metadata::extract(PageSource {
        body: page.as_bytes(),
        content_type: Some("text/html; charset=utf-8"),
        final_url: url.as_str(),
    })
    .expect("the page reads")
    .expect("the page is html");
    assert!(
        !metadata
            .assets
            .iter()
            .any(|asset| asset.url == "https://example.com/small.jpeg"),
        "the narrower rendition is the one the record stops naming"
    );
    archive
        .write_metadata(&url, &capture.id, &metadata)
        .expect("metadata is written");
    let engine = ScriptedEngine::new(Vec::new());

    repass_archive(
        &engine,
        &archive,
        &SiteRules::default(),
        RepassOptions::default(),
    )
    .expect("repass succeeds");

    assert_eq!(engine.fetched(), ["https://example.com/small.jpeg"]);
}

/// What the fallback above is worth once there is something to rank it against. The record
/// names the script and not the rendition, so one retry has a role and the other does not, and
/// the one without still goes first: a lookup that came up empty is not evidence that a page's
/// picture belongs behind its build output.
#[test]
fn a_missed_address_with_no_role_in_the_record_is_still_asked_for_before_a_script() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a").expect("valid URL");
    let page = article_page(
        r#"<script src="/bundle.js"></script><img srcset="/small.jpeg 424w, /large.jpeg 1456w">"#,
    );
    let mut new = html_capture(&url, &page);
    new.assets_missed = vec![
        MissedAsset {
            url: "https://example.com/bundle.js".to_owned(),
            reason: AssetMiss::CountCeilingReached,
        },
        MissedAsset {
            url: "https://example.com/small.jpeg".to_owned(),
            reason: AssetMiss::CountCeilingReached,
        },
    ];
    let capture = archive.write_capture(new).expect("capture is written");
    let metadata = metadata::extract(PageSource {
        body: page.as_bytes(),
        content_type: Some("text/html; charset=utf-8"),
        final_url: url.as_str(),
    })
    .expect("the page reads")
    .expect("the page is html");
    archive
        .write_metadata(&url, &capture.id, &metadata)
        .expect("metadata is written");
    let engine = ScriptedEngine::new(Vec::new());

    repass_archive(
        &engine,
        &archive,
        &SiteRules::default(),
        RepassOptions::default(),
    )
    .expect("repass succeeds");

    assert_eq!(
        engine.fetched(),
        [
            "https://example.com/small.jpeg",
            "https://example.com/bundle.js",
        ]
    );
}

#[test]
fn a_failed_asset_recovery_is_not_retried_on_the_next_pass() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a").expect("valid URL");
    let mut new = html_capture(&url, &article_page(""));
    new.assets_missed = vec![MissedAsset {
        url: "https://example.com/missing.css".to_owned(),
        reason: AssetMiss::CountCeilingReached,
    }];
    archive.write_capture(new).expect("capture is written");
    let engine = ScriptedEngine::new(Vec::new());

    let first = repass_archive(
        &engine,
        &archive,
        &SiteRules::default(),
        RepassOptions::default(),
    )
    .expect("first repass succeeds");
    let second = repass_archive(
        &engine,
        &archive,
        &SiteRules::default(),
        RepassOptions::default(),
    )
    .expect("second repass succeeds");

    assert_eq!(first.assets_still_missing, 1);
    assert_eq!(second.asset_fetches, 0);
    assert_eq!(engine.fetched(), ["https://example.com/missing.css"]);
}

#[test]
fn a_repass_asset_retry_still_refuses_private_addresses_before_the_engine_sees_them() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a").expect("valid URL");
    let mut new = html_capture(&url, &article_page(""));
    new.assets_missed = vec![MissedAsset {
        url: "http://169.254.169.254/style.css".to_owned(),
        reason: AssetMiss::CountCeilingReached,
    }];
    let capture = archive.write_capture(new).expect("capture is written");
    let engine = ScriptedEngine::new(vec![PageEvent::Response(PageResponse {
        requested_url: "http://169.254.169.254/style.css".to_owned(),
        final_url: "http://169.254.169.254/style.css".to_owned(),
        status: 200,
        headers: Vec::new(),
        body: b"secret".to_vec(),
        body_truncated: false,
        fetched_at: at("2026-07-25T14:03:23Z"),
    })]);

    let run = repass_archive(
        &engine,
        &archive,
        &SiteRules::default(),
        RepassOptions::default(),
    )
    .expect("repass succeeds");

    assert!(engine.fetched().is_empty());
    assert_eq!(run.assets_still_missing, 1);
    let read_back = archive
        .read_capture(&url, &capture.id)
        .expect("capture reads");
    assert_eq!(read_back.assets_missed[0].reason, AssetMiss::InsideANetwork);
}

/// A page whose only title is its `<title>` element, stored when the extractor still kept the
/// character references in it. The metadata record is repaired by the version move, and the
/// article's heading is only repaired with it because the article counts as stale when the
/// record it was derived from does.
#[test]
fn an_article_built_on_metadata_this_pass_replaces_is_rebuilt_with_it() {
    let dir = TempDir::new().expect("temp dir");
    let archive = archive_in(&dir);
    let url = CanonicalUrl::parse("https://example.com/a").expect("valid URL");
    let page = format!("<title>Isn&#x27;t Efficient</title>{}", article_page(""));
    let capture = archive
        .write_capture(html_capture(&url, &page))
        .expect("capture is written");
    let mut stale = metadata::extract(PageSource {
        body: page.as_bytes(),
        content_type: Some("text/html; charset=utf-8"),
        final_url: url.as_str(),
    })
    .expect("the page reads")
    .expect("the page is html");
    stale.extractor_version = metadata::EXTRACTOR_VERSION - 1;
    stale.title = stale.title.map(|title| archeion::metadata::Attributed {
        value: "Isn&#x27;t Efficient".to_owned(),
        source: title.source,
    });
    archive
        .write_metadata(&url, &capture.id, &stale)
        .expect("metadata is written");
    archive
        .write_article(
            &url,
            &capture.id,
            &Article {
                markdown: "# Isn&#x27;t Efficient\n\nBread is mostly patience.".to_owned(),
                record: article_record(readability::EXTRACTOR_VERSION, ExtractionRules::Heuristic),
            },
        )
        .expect("an article carrying the old spelling is written");

    let run = repass_archive(
        &ScriptedEngine::new(Vec::new()),
        &archive,
        &SiteRules::default(),
        RepassOptions::default(),
    )
    .expect("repass succeeds");

    assert_eq!(run.metadata_written, 1);
    assert_eq!(run.articles_written, 1);
    let article = archive
        .read_article(&url, &capture.id)
        .expect("article reads")
        .expect("article exists");
    assert!(
        article.markdown.starts_with("# Isn't Efficient"),
        "the heading is rebuilt from the metadata this pass rewrote, got {:?}",
        article.markdown.lines().next()
    );
}
