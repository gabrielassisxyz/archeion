//! Running a seed into the archive: the one place a crawl engine and the store meet.
//!
//! Nothing here knows which engine is underneath, and the engine knows nothing about the
//! archive. What connects them is a page event turning into a capture, which is where
//! canonicalization decides the address the page is filed under.

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use crate::assets::{AssetCapture, CapturedAssets};
use crate::canonical_url::{CanonicalUrl, InvalidCanonicalUrl};
use crate::crawl::{
    CrawlEngine, CrawlError, CrawlStop, FetchFailure, PageEvent, PageResponse, Seed,
    points_inside_a_network,
};
use crate::metadata::{self, PageMetadata, PageSource, ReferencedAsset, UnreadablePage};
use crate::readability::{self, Article, UnreadableArticle};
use crate::storage::{Archive, Header, NewCapture, StorageError};

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error(transparent)]
    Crawl(#[from] CrawlError),
    /// The run is carried out with the error because it is the moment the report matters
    /// most: the archive holds whatever was written before the disk refused, and a caller
    /// that only learns the write failed has to go looking for the rest.
    #[error("{source}")]
    Storage {
        #[source]
        source: StorageError,
        /// Behind a pointer because the report grows with every kind of loss a run can
        /// have, and carrying it inline would make every `Result` on this path as wide as
        /// the widest report rather than as wide as an error.
        run: Box<CaptureRun>,
    },
}

/// A page the crawl fetched and the archive has no address for. It is reported rather than
/// counted, because the address is the whole reason it was refused and a number leaves
/// nothing to look at.
#[derive(Debug, PartialEq, Eq)]
pub struct UnaddressablePage {
    pub url: String,
    pub reason: InvalidCanonicalUrl,
}

/// What one seed left behind. Every URL the engine reported is in exactly one of these,
/// so a run that archived less than expected says where the rest went.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CaptureRun {
    pub captures_written: usize,
    pub unaddressable_pages: Vec<UnaddressablePage>,
    /// Pages that ended on an address existing only inside a network, which a run that did
    /// not ask for those addresses had no business reaching. They are reported rather than
    /// counted: the address is the whole reason the page was refused, and it is also the
    /// only evidence that something redirected the crawl there.
    pub pages_inside_a_network: Vec<String>,
    /// URLs no server answered. They are reported and not stored: there is no response to
    /// archive, and inventing one would put a status in the record nothing ever sent.
    pub failed_fetches: Vec<FetchFailure>,
    /// Pages that were stored whole and whose markup the extractor could not read. Only the
    /// derived reading is missing, so the run goes on: the response is in the archive, and
    /// a later pass can read it again without fetching anything.
    pub unreadable_pages: Vec<UnreadablePage>,
    /// Pages whose prose was extracted. It is far fewer than the captures, and that is
    /// correct: most of the web is navigation, and a page that is not an article is not a
    /// failure of anything.
    pub articles_extracted: usize,
    /// Pages whose prose was refused because reading it would have cost too much. Reported
    /// rather than counted, for the same reason as the pages above: the response is stored
    /// whole and the URL is what someone needs in order to go and look at it.
    pub unreadable_articles: Vec<UnreadableArticle>,
    /// Pages the engine fetched that never reached the archive, straight from the engine.
    pub pages_dropped: usize,
    /// Subresources stored beside the captures of this run.
    pub assets_stored: usize,
    /// Subresources a page referenced and its capture does not hold. Each one is in the
    /// capture record with the reason, which is where it belongs: this count is for the run,
    /// and the run is over by the time anyone asks why a page looks wrong.
    pub assets_missed: usize,
    /// Requests the subresource pass made. The gap between this and the two counts above is
    /// the run recognising a file shared by many pages instead of asking for it once per page.
    pub asset_fetches: usize,
    /// Why the run ended. A run that stopped at its deadline archived a prefix of a site
    /// rather than the site, and the difference is not visible in any of the counts above.
    pub stopped: CrawlStop,
}

/// Crawls a seed and stores every page it produces, for as long as the seed's budget lasts.
///
/// The deadline is the engine's to enforce, because a host that accepts a connection and
/// then says nothing produces no page, and a callback that is never called cannot end
/// anything. What lives here is the backstop for the opposite failure, an engine that
/// ignores the field, and it deliberately fires late rather than on the instant: see
/// `engine_overran_its_deadline`.
pub fn capture_seed(
    engine: &dyn CrawlEngine,
    archive: &Archive,
    seed: &Seed,
) -> Result<CaptureRun, CaptureError> {
    let mut run = CaptureRun::default();
    let mut write_failure: Option<StorageError> = None;
    let started = Instant::now();
    let deadline = seed.deadline;
    let mut engine_overran = false;
    // The pass outlives every page because what it learned about one page's subresources is
    // the answer for the next page that references them.
    let mut assets = AssetCapture::new(engine, archive, seed, started);

    let outcome = engine.crawl(seed, &mut |event| {
        let answer = capture_page(
            event,
            archive,
            seed,
            &mut assets,
            &mut run,
            &mut write_failure,
        );
        if answer.is_break() {
            return ControlFlow::Break(());
        }
        // Read after the page is filed rather than before: it arrived already fetched, and
        // refusing to write what is in hand spends the bytes without keeping anything.
        if engine_overran_its_deadline(deadline, started.elapsed()) {
            engine_overran = true;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    })?;

    run.pages_dropped = outcome.pages_dropped;
    run.asset_fetches = assets.fetches();
    // The engine reports that its caller stopped it. This is that caller, and it knows why.
    run.stopped = if engine_overran {
        CrawlStop::DeadlineReached
    } else {
        outcome.stopped
    };
    if let Some(source) = write_failure {
        return Err(CaptureError::Storage {
            source,
            run: Box::new(run),
        });
    }
    Ok(run)
}

/// Whether the engine is still handing over pages long after the budget it was given.
///
/// The margin is the whole point. An engine that honors the deadline cancels its fetching
/// when the budget expires and then hands over the pages it had already paid for, and a
/// guard that fired on the same instant would break out on the first of them and count the
/// rest as lost. That handover is local writes, so a tenth of the budget is far more room
/// than it needs, while still bounding an engine that ignores the field.
fn engine_overran_its_deadline(deadline: Option<Duration>, elapsed: Duration) -> bool {
    deadline.is_some_and(|budget| elapsed >= budget.saturating_add(budget / 10))
}

/// Files one page under the address the archive knows it by.
///
/// Two shapes are skipped and reported rather than stored, and neither ends the run: a page
/// the archive cannot address, since one URL the canonical rules refuse says nothing about
/// the other two hundred, and a page that ended inside a network the run was not pointed
/// at. A failed write is the opposite, and stops the run: the disk that rejected this
/// capture will reject the next one, and a crawl that keeps fetching after that spends a
/// site's bandwidth on nothing.
fn capture_page(
    event: PageEvent,
    archive: &Archive,
    seed: &Seed,
    assets: &mut AssetCapture<'_>,
    run: &mut CaptureRun,
    write_failure: &mut Option<StorageError>,
) -> ControlFlow<()> {
    let page = match event {
        PageEvent::Response(page) => page,
        PageEvent::NoResponse(failure) => {
            run.failed_fetches.push(failure);
            return ControlFlow::Continue(());
        }
    };

    // A seed is screened before anything is dialled, so a page that ends on one of these
    // addresses got there by redirect, through a guard inside the engine. This boundary
    // keeps the same predicate because the engine is replaceable and storing the response
    // is what turns a blind fetch into a durable copy of whatever answered on the machine
    // the archive runs on. The run that asked for local addresses gets them, which is the
    // only way a locally served site is archived at all.
    if !seed.allow_private_addresses && points_inside_a_network(&page.final_url) {
        run.pages_inside_a_network.push(page.final_url);
        return ControlFlow::Continue(());
    }

    // The final URL and not the requested one: after a redirect, the content is at the
    // destination, and filing it under the address that pointed there would give the
    // same page a second identity for every link that reaches it.
    let canonical = match CanonicalUrl::parse(&page.final_url) {
        Ok(canonical) => canonical,
        Err(reason) => {
            run.unaddressable_pages.push(UnaddressablePage {
                url: page.final_url,
                reason,
            });
            return ControlFlow::Continue(());
        }
    };

    // Read before the capture is written, because the bytes are still in hand, and stored
    // after, because the response is what cannot be recovered: a run cut short then leaves
    // a capture with no reading of it, which a later pass can produce on its own.
    let extracted = read_page(&page, run);
    let prose = read_prose(&page, extracted.as_ref(), run);
    // The subresources are acquired before the capture is written because the record has to
    // name them, and their bytes are stored before the page's own for the reason every body
    // is stored before every record: a blob nobody references costs disk space, and a record
    // naming bytes that are absent is broken. What it trades is a page lost whole if the
    // process dies mid-pass, rather than a capture on disk claiming subresources it never got.
    let captured = match assets.of_page(referenced_assets(extracted.as_ref())) {
        Ok(captured) => captured,
        Err(error) => {
            *write_failure = Some(error);
            return ControlFlow::Break(());
        }
    };
    let (stored, missed) = (captured.stored.len(), captured.missed.len());
    let capture = match archive.write_capture(new_capture(canonical.clone(), page, captured)) {
        Ok(capture) => capture,
        Err(error) => {
            *write_failure = Some(error);
            return ControlFlow::Break(());
        }
    };
    run.captures_written += 1;
    // Counted with the capture rather than with the pass. A subresource whose capture never
    // reached the disk is a blob nothing references, and reporting it beside a capture that
    // does not exist would describe an archive nobody has.
    run.assets_stored += stored;
    run.assets_missed += missed;

    if let Some(metadata) = &extracted
        && let Err(error) = archive.write_metadata(&canonical, &capture.id, metadata)
    {
        *write_failure = Some(error);
        return ControlFlow::Break(());
    }

    if let Some(article) = prose
        && let Err(error) = archive.write_article(&canonical, &capture.id, &article)
    {
        *write_failure = Some(error);
        return ControlFlow::Break(());
    }
    ControlFlow::Continue(())
}

/// Separates the page's prose from the furniture around it, or reports that it could not.
///
/// Like the metadata above, a page this fails on is not a failed capture: the response is
/// stored whole, and the only thing missing is a reading of it that a later pass can redo
/// without fetching anything.
///
/// The title comes from the metadata rather than from this page's markup, so that the
/// precedence rules that live there decide it once instead of this forming a second opinion.
fn read_prose(
    page: &PageResponse,
    metadata: Option<&PageMetadata>,
    run: &mut CaptureRun,
) -> Option<Article> {
    let title = metadata
        .and_then(|metadata| metadata.title.as_ref())
        .map(|title| title.value.as_str());
    match readability::extract(
        PageSource {
            body: &page.body,
            content_type: content_type_of(&page.headers),
            final_url: &page.final_url,
        },
        title,
    ) {
        Ok(Some(article)) => {
            run.articles_extracted += 1;
            Some(article)
        }
        Ok(None) => None,
        Err(refused) => {
            run.unreadable_articles.push(refused);
            None
        }
    }
}

/// Reads what the page says about itself, or reports that it could not be read.
///
/// A page the parser gives up on is not a failed capture. The response was fetched and is
/// about to be stored whole, and the only thing missing is a reading of it that costs
/// nothing to redo later, so the run keeps going and says which page it was.
fn read_page(page: &PageResponse, run: &mut CaptureRun) -> Option<PageMetadata> {
    match metadata::extract(PageSource {
        body: &page.body,
        content_type: content_type_of(&page.headers),
        final_url: &page.final_url,
    }) {
        Ok(extracted) => extracted,
        Err(unreadable) => {
            run.unreadable_pages.push(unreadable);
            None
        }
    }
}

/// What a page referenced, or nothing at all when there was no reading of it. A capture that
/// is not a page has no subresources to acquire, and neither has one whose markup the
/// extractor could not read: the references are in the part it failed on.
fn referenced_assets(extracted: Option<&PageMetadata>) -> &[ReferencedAsset] {
    extracted.map_or(&[], |metadata| metadata.assets.as_slice())
}

fn new_capture(
    canonical_url: CanonicalUrl,
    page: PageResponse,
    captured: CapturedAssets,
) -> NewCapture {
    let media_type = media_type_of(&page.headers);
    NewCapture {
        canonical_url,
        requested_url: page.requested_url,
        final_url: page.final_url,
        status: page.status,
        media_type,
        response_headers: page.headers,
        body: page.body,
        body_truncated: page.body_truncated,
        fetched_at: page.fetched_at,
        assets: captured.stored,
        assets_missed: captured.missed,
    }
}

/// The media type without its parameters: `text/html` out of `text/html; charset=utf-8`.
/// Nothing is lost by narrowing it, since the header survives verbatim in the record, and
/// the field then holds what its name promises instead of a string every reader re-parses.
pub(crate) fn media_type_of(headers: &[Header]) -> Option<String> {
    content_type_of(headers)
        .map(|content_type| {
            let (media_type, _parameters) =
                content_type.split_once(';').unwrap_or((content_type, ""));
            media_type.trim().to_ascii_lowercase()
        })
        .filter(|media_type| !media_type.is_empty())
}

fn content_type_of(headers: &[Header]) -> Option<&str> {
    headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("content-type"))
        .map(|header| header.value.as_str())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;
    use crate::crawl::CrawlOutcome;
    use crate::storage::AssetMiss;

    /// A crawl engine that replays a written-down list of page events instead of fetching
    /// anything. It is the whole reason the boundary exists: the pipeline above it is
    /// testable without a network, and what it does with a 404 or with a page it cannot
    /// address is decided by the test rather than by whatever the web answered today.
    struct ScriptedCrawlEngine {
        pages: Vec<PageEvent>,
        /// What each URL fetched on its own answers with. A URL that is not written down
        /// answered nothing, which is the only honest default for a fake with no network.
        subresources: HashMap<String, PageEvent>,
        outcome: CrawlOutcome,
        /// What the pipeline answered for each page, so a test can prove the crawl stopped
        /// rather than infer it from a count.
        answers: RefCell<Vec<ControlFlow<()>>>,
        /// Every URL fetched on its own, in order. A file shared by two pages should cost one
        /// request, and a count is the only way a test can say so.
        fetched: RefCell<Vec<String>>,
    }

    impl ScriptedCrawlEngine {
        fn new(pages: Vec<PageEvent>) -> Self {
            Self {
                pages,
                subresources: HashMap::new(),
                outcome: CrawlOutcome::default(),
                answers: RefCell::new(Vec::new()),
                fetched: RefCell::new(Vec::new()),
            }
        }

        /// The subresources this engine will answer for, keyed by the URL they are asked for.
        fn serving(mut self, subresources: Vec<PageEvent>) -> Self {
            for event in subresources {
                let url = match &event {
                    PageEvent::Response(response) => response.requested_url.clone(),
                    PageEvent::NoResponse(failure) => failure.url.clone(),
                };
                self.subresources.insert(url, event);
            }
            self
        }

        fn pages_offered(&self) -> usize {
            self.answers.borrow().len()
        }

        fn urls_fetched(&self) -> Vec<String> {
            self.fetched.borrow().clone()
        }
    }

    impl CrawlEngine for ScriptedCrawlEngine {
        fn crawl(
            &self,
            _seed: &Seed,
            on_page: &mut dyn FnMut(PageEvent) -> ControlFlow<()>,
        ) -> Result<CrawlOutcome, CrawlError> {
            for page in &self.pages {
                let answer = on_page(page.clone());
                self.answers.borrow_mut().push(answer);
                if answer.is_break() {
                    break;
                }
            }
            Ok(self.outcome.clone())
        }

        fn fetch(&self, url: &str, _seed: &Seed) -> PageEvent {
            self.fetched.borrow_mut().push(url.to_owned());
            self.subresources.get(url).cloned().unwrap_or_else(|| {
                PageEvent::NoResponse(FetchFailure {
                    url: url.to_owned(),
                    reason: "this fake was given nothing to answer with".to_owned(),
                })
            })
        }
    }

    fn page(url: &str, status: u16, body: &str) -> PageEvent {
        PageEvent::Response(PageResponse {
            requested_url: url.to_owned(),
            final_url: url.to_owned(),
            status,
            headers: vec![Header {
                name: "content-type".to_owned(),
                value: "text/html; charset=utf-8".to_owned(),
            }],
            body: body.as_bytes().to_vec(),
            body_truncated: false,
            fetched_at: "2026-07-25T14:03:22Z".parse().expect("valid timestamp"),
        })
    }

    fn response_of(event: &mut PageEvent) -> &mut PageResponse {
        match event {
            PageEvent::Response(response) => response,
            PageEvent::NoResponse(failure) => panic!("expected a response, got {failure:?}"),
        }
    }

    fn archive_in(dir: &TempDir) -> Archive {
        Archive::open(dir.path()).expect("archive opens in an empty directory")
    }

    #[test]
    fn a_page_is_archived_with_a_reading_of_it_beside_the_capture() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![page(
            "https://example.com/a",
            200,
            r#"<html><head><title>A page</title></head>
               <body><a href="/b">b</a></body></html>"#,
        )]);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert!(run.unreadable_pages.is_empty());
        let url = CanonicalUrl::parse("https://example.com/a").expect("valid url");
        let captures = archive.list_captures(&url).expect("captures are listed");
        let metadata = archive
            .read_metadata(&url, &captures[0])
            .expect("the reading is stored")
            .expect("a page has a reading");

        assert_eq!(metadata.title.expect("a title").value, "A page");
        assert_eq!(metadata.links[0].url, "https://example.com/b");
    }

    #[test]
    fn an_article_is_archived_as_a_markdown_document_beside_the_capture() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![page(
            "https://example.com/a",
            200,
            &format!(
                r#"<html><head><title>A page for the tab</title>
                   <meta property="og:title" content="How to bake bread"></head>
                   <body><nav><a href="/b">b</a></nav><article>{}</article>
                   <footer>Subscribe to our newsletter</footer></body></html>"#,
                "<p>Bread is mostly patience, and the dough will tell you when it is ready.</p>"
                    .repeat(8)
            ),
        )]);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.articles_extracted, 1);
        assert!(run.unreadable_articles.is_empty());
        let url = CanonicalUrl::parse("https://example.com/a").expect("valid url");
        let captures = archive.list_captures(&url).expect("captures are listed");
        let article = archive
            .read_article(&url, &captures[0])
            .expect("the prose is stored")
            .expect("an article has prose");

        // The heading is the title the metadata rules resolved, not the raw `<title>`.
        assert!(
            article.markdown.starts_with("# How to bake bread\n"),
            "{}",
            article.markdown
        );
        assert!(article.markdown.contains("Bread is mostly patience"));
        assert!(!article.markdown.contains("Subscribe to our newsletter"));
        assert!(article.record.word_count > 0);
    }

    /// Most of what a crawl answers is navigation. A capture with no prose in it is the
    /// ordinary case, and writing an empty article for each would bury the ones worth having.
    #[test]
    fn a_capture_with_no_prose_in_it_gets_no_article_and_no_complaint() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![page(
            "https://example.com/index",
            200,
            r#"<html><head><title>Index</title></head><body><ul>
               <li><a href="/a">one</a></li><li><a href="/b">two</a></li>
               <li><a href="/c">three</a></li></ul></body></html>"#,
        )]);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert_eq!(run.articles_extracted, 0);
        assert!(run.unreadable_articles.is_empty());
        let url = CanonicalUrl::parse("https://example.com/index").expect("valid url");
        let captures = archive.list_captures(&url).expect("captures are listed");
        assert_eq!(
            archive
                .read_article(&url, &captures[0])
                .expect("no prose is not an error"),
            None
        );
    }

    /// The archive keeps whatever answered, and most of what answers a crawl is not a page.
    /// A capture with nothing to read is the ordinary case, not a failure to record.
    #[test]
    fn a_capture_that_is_not_a_page_gets_no_reading_and_no_complaint() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut event = page(
            "https://example.com/logo.png",
            200,
            "<html>not markup</html>",
        );
        response_of(&mut event).headers = vec![Header {
            name: "content-type".to_owned(),
            value: "image/png".to_owned(),
        }];
        let engine = ScriptedCrawlEngine::new(vec![event]);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert!(run.unreadable_pages.is_empty());
        let url = CanonicalUrl::parse("https://example.com/logo.png").expect("valid url");
        let captures = archive.list_captures(&url).expect("captures are listed");
        assert_eq!(
            archive
                .read_metadata(&url, &captures[0])
                .expect("no reading is not an error"),
            None
        );
    }

    /// The response is the part that cannot be fetched again, so a page whose encoding the
    /// extractor has to work out is still archived byte for byte, and the reading of it is
    /// what has to cope.
    #[test]
    fn a_page_in_a_legacy_encoding_is_stored_verbatim_and_read_correctly() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let body = b"<html><head><title>caf\xe9</title></head></html>".to_vec();
        let mut event = page("https://example.com/a", 200, "");
        let response = response_of(&mut event);
        response.body = body.clone();
        response.headers = vec![Header {
            name: "content-type".to_owned(),
            value: "text/html; charset=windows-1252".to_owned(),
        }];
        let engine = ScriptedCrawlEngine::new(vec![event]);

        capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        let url = CanonicalUrl::parse("https://example.com/a").expect("valid url");
        let captures = archive.list_captures(&url).expect("captures are listed");
        let capture = archive
            .read_capture(&url, &captures[0])
            .expect("the capture reads back");
        let metadata = archive
            .read_metadata(&url, &captures[0])
            .expect("the reading is stored")
            .expect("a page has a reading");

        assert_eq!(archive.read_body(&capture.body.sha256).expect("body"), body);
        assert_eq!(metadata.title.expect("a title").value, "café");
    }

    #[test]
    fn every_page_the_crawl_produced_becomes_a_capture() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![
            page("https://example.com/a", 200, "<html>a</html>"),
            page("https://example.com/b", 200, "<html>b</html>"),
        ]);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 2);
        let url = CanonicalUrl::parse("https://example.com/a").expect("valid url");
        let captures = archive.list_captures(&url).expect("captures are listed");
        assert_eq!(captures.len(), 1);
        let capture = archive
            .read_capture(&url, &captures[0])
            .expect("the capture reads back");
        assert_eq!(capture.status, 200);
        assert_eq!(capture.media_type.as_deref(), Some("text/html"));
        assert_eq!(
            archive.read_body(&capture.body.sha256).expect("body"),
            b"<html>a</html>"
        );
    }

    #[test]
    fn a_page_that_failed_is_archived_with_the_status_it_failed_with() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![page("https://example.com/gone", 404, "")]);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        let url = CanonicalUrl::parse("https://example.com/gone").expect("valid url");
        let captures = archive.list_captures(&url).expect("captures are listed");
        let capture = archive
            .read_capture(&url, &captures[0])
            .expect("the capture reads back");
        assert_eq!(capture.status, 404);
    }

    #[test]
    fn two_spellings_of_one_page_land_on_one_item() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![
            page("https://www.example.com/a", 200, "<html>a</html>"),
            page("https://example.com/a?utm_source=x", 200, "<html>a</html>"),
        ]);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 2);
        let url = CanonicalUrl::parse("https://example.com/a").expect("valid url");
        assert_eq!(
            archive
                .list_captures(&url)
                .expect("captures are listed")
                .len(),
            2
        );
    }

    #[test]
    fn a_page_the_archive_cannot_address_is_reported_and_the_crawl_goes_on() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![
            page("ftp://example.com/a", 200, "<html>a</html>"),
            page("https://example.com/b", 200, "<html>b</html>"),
        ]);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert_eq!(run.unaddressable_pages.len(), 1);
        assert_eq!(run.unaddressable_pages[0].url, "ftp://example.com/a");
        assert_eq!(engine.pages_offered(), 2);
    }

    /// The page arrives with two URLs because that is the only way it can exist: the seed
    /// guard refused this address before the crawl started, so a page wearing it got there
    /// by a redirect the engine followed.
    fn page_redirected_inside_a_network(final_url: &str) -> PageEvent {
        let mut event = page("https://example.com/a", 200, "<html>internal</html>");
        response_of(&mut event).final_url = final_url.to_owned();
        event
    }

    #[test]
    fn a_page_that_ended_inside_a_network_is_refused_and_the_crawl_goes_on() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![
            page_redirected_inside_a_network("http://0.0.0.1/"),
            page("https://example.com/b", 200, "<html>b</html>"),
        ]);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert_eq!(run.pages_inside_a_network, vec!["http://0.0.0.1/"]);
        assert_eq!(engine.pages_offered(), 2);
        let url = CanonicalUrl::parse("http://0.0.0.1/").expect("valid url");
        assert_eq!(
            archive
                .list_captures(&url)
                .expect("captures are listed")
                .len(),
            0,
            "the response from inside the network reached the archive"
        );
    }

    /// The refusal above is a guard against an address the run never asked for, so a run
    /// that did ask has to keep working. Archiving a locally served site is the whole
    /// purpose of the flag, and it is also the only way the real fetch path is exercised.
    #[test]
    fn a_run_that_asked_for_local_addresses_still_archives_them() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine =
            ScriptedCrawlEngine::new(vec![page_redirected_inside_a_network("http://127.0.0.1/")]);
        let mut seed = Seed::new("http://127.0.0.1/");
        seed.allow_private_addresses = true;

        let run = capture_seed(&engine, &archive, &seed).expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert!(run.pages_inside_a_network.is_empty());
    }

    #[test]
    fn a_failed_write_stops_the_crawl_instead_of_repeating_itself() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        // A file where the item records need a directory fails every write the way a full
        // or read-only disk would, and fails it on the first page rather than a later one.
        std::fs::write(dir.path().join("items"), b"not a directory")
            .expect("the write target is blocked");

        let engine = ScriptedCrawlEngine::new(vec![
            page("https://example.com/a", 200, "<html>a</html>"),
            page("https://example.com/b", 200, "<html>b</html>"),
        ]);

        let error = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect_err("the write fails");

        assert!(matches!(error, CaptureError::Storage { .. }));
        assert_eq!(engine.pages_offered(), 1);
    }

    #[test]
    fn a_run_cut_short_by_a_failed_write_still_reports_what_it_did() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![
            page("ftp://example.com/a", 200, "<html>a</html>"),
            page("https://example.com/b", 200, "<html>b</html>"),
        ]);
        // Blocked only after the archive exists, so the first page is refused for its
        // address and the second one for the disk.
        std::fs::write(dir.path().join("items"), b"not a directory")
            .expect("the write target is blocked");

        let error = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect_err("the write fails");

        match error {
            CaptureError::Storage { run, .. } => {
                assert_eq!(run.unaddressable_pages.len(), 1);
                assert_eq!(run.captures_written, 0);
            }
            other => panic!("expected a storage failure, got {other:?}"),
        }
    }

    #[test]
    fn a_url_no_server_answered_is_reported_and_never_archived() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![
            PageEvent::NoResponse(FetchFailure {
                url: "https://example.com/unreachable".to_owned(),
                reason: "error sending request: dns error".to_owned(),
            }),
            page("https://example.com/b", 200, "<html>b</html>"),
        ]);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert_eq!(run.failed_fetches.len(), 1);
        assert_eq!(run.failed_fetches[0].url, "https://example.com/unreachable");
        let url = CanonicalUrl::parse("https://example.com/unreachable").expect("valid url");
        assert!(
            archive
                .list_captures(&url)
                .expect("captures are listed")
                .is_empty(),
            "a fetch that reached no server left a record behind"
        );
    }

    #[test]
    fn pages_the_engine_lost_are_carried_into_the_report() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut engine =
            ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, "<html>a</html>")]);
        engine.outcome.pages_dropped = 3;

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert_eq!(run.pages_dropped, 3);
    }

    /// The engine here replays a list and knows nothing about a deadline, which is exactly
    /// the case this guard is for: an engine that ignores the field is stopped from above.
    /// A budget of zero has a margin of zero, so the guard is armed on the first page.
    #[test]
    fn a_seed_out_of_budget_stops_after_the_page_it_is_holding() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![
            page("https://example.com/a", 200, "<html>a</html>"),
            page("https://example.com/b", 200, "<html>b</html>"),
            page("https://example.com/c", 200, "<html>c</html>"),
        ]);
        let mut seed = Seed::new("https://example.com/");
        seed.deadline = Some(Duration::ZERO);

        let run = capture_seed(&engine, &archive, &seed).expect("the run completes");

        assert_eq!(engine.pages_offered(), 1);
        assert_eq!(
            run.captures_written, 1,
            "the page in hand was fetched already and should not be thrown away"
        );
        assert_eq!(run.stopped, CrawlStop::DeadlineReached);
    }

    #[test]
    fn a_run_inside_its_budget_says_it_ran_out_of_pages_and_not_of_time() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine =
            ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, "<html>a</html>")]);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert_eq!(run.stopped, CrawlStop::Exhausted);
    }

    /// An engine that honors the deadline is mid-handover when the budget expires, passing
    /// up the pages it had already fetched. Cutting it on the instant would lose exactly the
    /// pages the deadline was careful to keep, so the guard above it has to arrive later.
    #[test]
    fn an_engine_still_handing_over_at_its_deadline_is_left_to_finish() {
        let budget = Duration::from_secs(300);

        assert!(!engine_overran_its_deadline(Some(budget), budget));
        assert!(!engine_overran_its_deadline(
            Some(budget),
            budget + Duration::from_secs(29)
        ));
    }

    #[test]
    fn an_engine_fetching_well_past_the_budget_is_cut_from_above() {
        let budget = Duration::from_secs(300);

        assert!(engine_overran_its_deadline(
            Some(budget),
            budget + Duration::from_secs(31)
        ));
    }

    #[test]
    fn a_seed_that_asked_for_no_deadline_is_never_cut_from_above() {
        assert!(!engine_overran_its_deadline(
            None,
            Duration::from_secs(86_400)
        ));
    }

    /// The engine has its own reach on the deadline: it can end a crawl that produced no
    /// page at all, which is a stop nothing above it would ever see happen.
    #[test]
    fn a_crawl_the_engine_cut_short_says_so_in_the_run() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut engine =
            ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, "<html>a</html>")]);
        engine.outcome.stopped = CrawlStop::DeadlineReached;

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.stopped, CrawlStop::DeadlineReached);
    }

    /// The reason the flag exists rather than being inferred: a body cut short still parses
    /// and still arrives under a status that promises the whole page, so the only place the
    /// shortfall can be seen is a record that says so.
    #[test]
    fn a_page_that_arrived_short_is_archived_saying_so() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut cut_short = page("https://example.com/a", 200, "<html>a");
        response_of(&mut cut_short).body_truncated = true;
        let engine = ScriptedCrawlEngine::new(vec![cut_short]);

        capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        let url = CanonicalUrl::parse("https://example.com/a").expect("valid url");
        let captures = archive.list_captures(&url).expect("captures are listed");
        let capture = archive
            .read_capture(&url, &captures[0])
            .expect("the capture reads back");
        assert!(capture.body_truncated);
    }

    #[test]
    fn the_media_type_is_the_type_without_its_parameters() {
        let header = |value: &str| {
            vec![Header {
                name: "Content-Type".to_owned(),
                value: value.to_owned(),
            }]
        };

        assert_eq!(
            media_type_of(&header("text/HTML; charset=utf-8")).as_deref(),
            Some("text/html")
        );
        assert_eq!(
            media_type_of(&header("application/pdf")).as_deref(),
            Some("application/pdf")
        );
        assert_eq!(media_type_of(&header("")), None);
        assert_eq!(media_type_of(&[]), None);
    }

    #[test]
    fn the_capture_keeps_where_the_fetch_started_and_where_it_ended() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut redirected = page("https://example.com/final", 200, "<html>a</html>");
        response_of(&mut redirected).requested_url = "https://example.com/short-link".to_owned();
        let engine = ScriptedCrawlEngine::new(vec![redirected]);

        capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        let url = CanonicalUrl::parse("https://example.com/final").expect("valid url");
        let captures = archive.list_captures(&url).expect("captures are listed");
        let capture = archive
            .read_capture(&url, &captures[0])
            .expect("the capture reads back");
        assert_eq!(capture.requested_url, "https://example.com/short-link");
        assert_eq!(capture.final_url, "https://example.com/final");
    }

    const STYLED_PAGE: &str = r#"<html><head><link rel="stylesheet" href="/style.css"></head>
           <body><img src="/logo.png"></body></html>"#;

    fn subresource(url: &str, media_type: &str, body: &[u8]) -> PageEvent {
        PageEvent::Response(PageResponse {
            requested_url: url.to_owned(),
            final_url: url.to_owned(),
            status: 200,
            headers: vec![Header {
                name: "content-type".to_owned(),
                value: media_type.to_owned(),
            }],
            body: body.to_vec(),
            body_truncated: false,
            fetched_at: "2026-07-25T14:03:22Z".parse().expect("valid timestamp"),
        })
    }

    fn only_capture_of(archive: &Archive, url: &str) -> crate::storage::Capture {
        let url = CanonicalUrl::parse(url).expect("valid url");
        let captures = archive.list_captures(&url).expect("captures are listed");
        archive
            .read_capture(&url, &captures[0])
            .expect("the capture reads back")
    }

    #[test]
    fn a_page_is_archived_with_the_files_it_needs_to_still_render() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine =
            ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, STYLED_PAGE)])
                .serving(vec![
                    subresource("https://example.com/style.css", "text/CSS", b"body{}"),
                    subresource("https://example.com/logo.png", "image/png", b"\x89PNG"),
                ]);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.assets_stored, 2);
        assert_eq!(run.assets_missed, 0);
        assert_eq!(run.asset_fetches, 2);
        let capture = only_capture_of(&archive, "https://example.com/a");
        assert!(capture.assets_missed.is_empty());
        let stored: Vec<(&str, Option<&str>)> = capture
            .assets
            .iter()
            .map(|asset| (asset.final_url.as_str(), asset.media_type.as_deref()))
            .collect();
        assert_eq!(
            stored,
            [
                ("https://example.com/style.css", Some("text/css")),
                ("https://example.com/logo.png", Some("image/png")),
            ]
        );
        assert_eq!(
            archive
                .read_body(&capture.assets[0].body.sha256)
                .expect("the stylesheet reads back"),
            b"body{}"
        );
    }

    /// The reason the pass remembers anything. A stylesheet belongs to every page of a site
    /// that links it, and asking the server for it once per page is two hundred requests for
    /// one file, which is not something an archive gets to do to somebody else's host.
    #[test]
    fn a_file_two_pages_share_is_asked_for_once_and_stored_once() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![
            page(
                "https://example.com/a",
                200,
                r#"<html><link rel="stylesheet" href="/style.css"></html>"#,
            ),
            page(
                "https://example.com/b",
                200,
                r#"<html><link rel="stylesheet" href="/style.css"></html>"#,
            ),
        ])
        .serving(vec![subresource(
            "https://example.com/style.css",
            "text/css",
            b"body{}",
        )]);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 2);
        assert_eq!(run.assets_stored, 2, "both captures reference the file");
        assert_eq!(
            engine.urls_fetched(),
            ["https://example.com/style.css"],
            "the shared file was asked for more than once"
        );
        assert_eq!(run.asset_fetches, 1);
        let first = only_capture_of(&archive, "https://example.com/a");
        let second = only_capture_of(&archive, "https://example.com/b");
        assert_eq!(first.assets[0].body.sha256, second.assets[0].body.sha256);
    }

    /// A subresource that is gone is gone for every page of the site that references it, so
    /// the answer is remembered exactly like a successful one. A run that asked again per page
    /// would spend a request each time to be told the same thing.
    #[test]
    fn a_subresource_no_server_answered_is_recorded_and_asked_for_only_once() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let referencing = r#"<html><link rel="stylesheet" href="/gone.css"></html>"#;
        let engine = ScriptedCrawlEngine::new(vec![
            page("https://example.com/a", 200, referencing),
            page("https://example.com/b", 200, referencing),
        ]);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 2, "the pages are archived regardless");
        assert_eq!(run.assets_stored, 0);
        assert_eq!(run.assets_missed, 2);
        assert_eq!(run.asset_fetches, 1);
        let capture = only_capture_of(&archive, "https://example.com/a");
        assert_eq!(capture.assets_missed.len(), 1);
        assert_eq!(capture.assets_missed[0].url, "https://example.com/gone.css");
        assert!(matches!(
            capture.assets_missed[0].reason,
            AssetMiss::NoResponse { .. }
        ));
    }

    /// Half a stylesheet is not a stylesheet, and a subresource record has nowhere to say the
    /// bytes are partial, so it is refused rather than stored as though it were whole. A page
    /// in the same state is kept, because a page cut short is still the page.
    #[test]
    fn a_subresource_that_arrived_short_is_not_stored_as_if_it_were_whole() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut cut_short = subresource("https://example.com/style.css", "text/css", b"body{col");
        response_of(&mut cut_short).body_truncated = true;
        let engine =
            ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, STYLED_PAGE)])
                .serving(vec![cut_short]);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.assets_stored, 0, "the logo was never served either");
        let capture = only_capture_of(&archive, "https://example.com/a");
        let short = capture
            .assets_missed
            .iter()
            .find(|missed| missed.url == "https://example.com/style.css")
            .expect("the stylesheet is reported");
        assert_eq!(short.reason, AssetMiss::ArrivedShort { byte_len: 8 });
    }

    /// The same guard the pages get, one hop further in. A public page whose stylesheet
    /// redirects into the machine the archive runs on is the same request as a seed pointed
    /// there, and the durable half of the harm is the bytes that would be written.
    #[test]
    fn a_subresource_that_ended_inside_a_network_is_refused() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut redirected = subresource("https://example.com/style.css", "text/css", b"body{}");
        response_of(&mut redirected).final_url = "http://169.254.169.254/latest/".to_owned();
        let engine =
            ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, STYLED_PAGE)])
                .serving(vec![redirected]);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert_eq!(run.assets_stored, 0);
        let capture = only_capture_of(&archive, "https://example.com/a");
        let refused = capture
            .assets_missed
            .iter()
            .find(|missed| missed.url == "https://example.com/style.css")
            .expect("the stylesheet is reported");
        assert_eq!(refused.reason, AssetMiss::InsideANetwork);
    }

    /// The other side of that guard: archiving a site served locally is the whole reason the
    /// flag exists, and a local site's stylesheet is on the same local address.
    #[test]
    fn a_run_that_asked_for_local_addresses_still_gets_their_subresources() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut local = page("http://127.0.0.1:8000/a", 200, STYLED_PAGE);
        response_of(&mut local).final_url = "http://127.0.0.1:8000/a".to_owned();
        let engine = ScriptedCrawlEngine::new(vec![local]).serving(vec![subresource(
            "http://127.0.0.1:8000/style.css",
            "text/css",
            b"body{}",
        )]);
        let mut seed = Seed::new("http://127.0.0.1:8000/");
        seed.allow_private_addresses = true;

        let run = capture_seed(&engine, &archive, &seed).expect("the run completes");

        assert_eq!(run.assets_stored, 1);
        let capture = only_capture_of(&archive, "http://127.0.0.1:8000/a");
        assert_eq!(
            capture.assets[0].final_url,
            "http://127.0.0.1:8000/style.css"
        );
    }

    /// A run out of time stops asking. The page is archived either way, and a subresource is
    /// the cheapest thing in a run to give up on, so the budget is read plainly here rather
    /// than with the margin the backstop over the engine allows itself.
    #[test]
    fn a_run_out_of_time_stops_asking_for_subresources_and_says_so() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine =
            ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, STYLED_PAGE)])
                .serving(vec![subresource(
                    "https://example.com/style.css",
                    "text/css",
                    b"body{}",
                )]);
        let mut seed = Seed::new("https://example.com/");
        seed.deadline = Some(Duration::ZERO);

        let run = capture_seed(&engine, &archive, &seed).expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert_eq!(
            run.asset_fetches, 0,
            "the run asked for a file it had no time for"
        );
        let capture = only_capture_of(&archive, "https://example.com/a");
        assert_eq!(capture.assets_missed.len(), 2);
        assert!(
            capture
                .assets_missed
                .iter()
                .all(|missed| missed.reason == AssetMiss::DeadlineReached),
            "{:#?}",
            capture.assets_missed
        );
    }

    /// A capture that is not a page references nothing, and neither does one whose markup the
    /// extractor could not read: the references live in the part it failed on.
    #[test]
    fn a_capture_with_no_reading_of_it_asks_for_nothing() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut image = page("https://example.com/logo.png", 200, "not markup");
        response_of(&mut image).headers = vec![Header {
            name: "content-type".to_owned(),
            value: "image/png".to_owned(),
        }];
        let engine = ScriptedCrawlEngine::new(vec![image]);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert_eq!(run.asset_fetches, 0);
        assert_eq!(run.assets_missed, 0);
    }

    /// A page that references more files than a capture holds is a page the ceiling was
    /// written for, and the tail of it is recorded rather than dropped quietly. Nothing past
    /// the ceiling is asked for: refusing after fetching would spend the requests anyway.
    #[test]
    fn a_page_referencing_more_files_than_a_capture_holds_records_the_tail() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let references = 130;
        let mut markup = String::from("<html><body>");
        let mut served = Vec::new();
        for index in 0..references {
            markup.push_str(&format!(r#"<img src="/{index}.png">"#));
            served.push(subresource(
                &format!("https://example.com/{index}.png"),
                "image/png",
                format!("{index}").as_bytes(),
            ));
        }
        markup.push_str("</body></html>");
        let engine = ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, &markup)])
            .serving(served);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        let capture = only_capture_of(&archive, "https://example.com/a");
        assert_eq!(capture.assets.len(), 128);
        assert_eq!(capture.assets_missed.len(), references - 128);
        assert_eq!(run.asset_fetches, 128, "a refused file was fetched anyway");
        assert!(
            capture
                .assets_missed
                .iter()
                .all(|missed| missed.reason == AssetMiss::CountCeilingReached)
        );
    }

    /// A page referencing a host that accepts connections and says nothing is what holds the
    /// whole pipeline: the pass runs inside the callback the crawl hands pages through, so every
    /// request it waits out is time the engine spends fetching pages nothing is reading. After
    /// enough silence the capture stops asking, and what it did not ask for says so.
    #[test]
    fn a_capture_stops_asking_once_nothing_is_answering() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut markup = String::from("<html><body>");
        for index in 0..6 {
            markup.push_str(&format!(r#"<img src="/{index}.png">"#));
        }
        markup.push_str("</body></html>");
        // The last reference would have answered, and the point is that it is never asked.
        let engine = ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, &markup)])
            .serving(vec![subresource(
                "https://example.com/5.png",
                "image/png",
                b"\x89PNG",
            )]);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert_eq!(run.assets_stored, 0);
        assert_eq!(
            run.asset_fetches, 3,
            "the capture kept waiting on a host that was not answering"
        );
        let capture = only_capture_of(&archive, "https://example.com/a");
        let reasons: Vec<&AssetMiss> = capture
            .assets_missed
            .iter()
            .map(|missed| &missed.reason)
            .collect();
        assert!(
            matches!(
                reasons.as_slice(),
                [
                    AssetMiss::NoResponse { .. },
                    AssetMiss::NoResponse { .. },
                    AssetMiss::NoResponse { .. },
                    AssetMiss::NothingWasAnswering,
                    AssetMiss::NothingWasAnswering,
                    AssetMiss::NothingWasAnswering,
                ]
            ),
            "{reasons:#?}"
        );
    }

    /// Silence has to be consecutive to mean anything. A host that answers between two failures
    /// is answering, and a page that references a couple of files that are gone is an ordinary
    /// page rather than a reason to stop capturing it.
    #[test]
    fn a_host_that_answers_between_failures_is_not_taken_for_a_dead_one() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![page(
            "https://example.com/a",
            200,
            r#"<html><body><img src="/gone-1.png"><img src="/gone-2.png"><img src="/here.png">
               <img src="/gone-3.png"><img src="/gone-4.png"><img src="/also-here.png"></body></html>"#,
        )])
        .serving(vec![
            subresource("https://example.com/here.png", "image/png", b"\x89PNG1"),
            subresource("https://example.com/also-here.png", "image/png", b"\x89PNG2"),
        ]);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.assets_stored, 2);
        assert_eq!(run.asset_fetches, 6, "a reference was never asked for");
        let capture = only_capture_of(&archive, "https://example.com/a");
        assert!(
            capture
                .assets_missed
                .iter()
                .all(|missed| matches!(missed.reason, AssetMiss::NoResponse { .. })),
            "{:#?}",
            capture.assets_missed
        );
    }

    /// A refusal that never became a request cost no wait, so it is not silence. Counting it
    /// would let a page full of addresses this archive will not dial stop it from capturing the
    /// files that are perfectly reachable further down the same page.
    #[test]
    fn a_reference_refused_before_being_dialled_is_not_counted_as_silence() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![page(
            "https://example.com/a",
            200,
            r#"<html><body><img src="http://127.0.0.1/1.png"><img src="http://10.0.0.1/2.png">
               <img src="http://[::1]/3.png"><img src="http://169.254.169.254/4.png">
               <img src="/here.png"></body></html>"#,
        )])
        .serving(vec![subresource(
            "https://example.com/here.png",
            "image/png",
            b"\x89PNG",
        )]);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.assets_stored, 1);
        assert_eq!(run.asset_fetches, 1);
        assert_eq!(run.assets_missed, 4);
    }

    /// The ceiling on bytes counts what came over the wire, not what was kept, and this is the
    /// page that tells the two apart: every file on it is just over the size one subresource
    /// may spend, so nothing is ever stored. A ceiling that counted stored bytes would sit at
    /// zero forever while the run transferred a gigabyte per page.
    #[test]
    fn a_page_of_oversized_files_stops_costing_transfers_at_the_ceiling() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let oversized = vec![b'x'; 8 * 1024 * 1024 + 1];
        let mut markup = String::from("<html><body>");
        let mut served = Vec::new();
        for index in 0..5 {
            markup.push_str(&format!(r#"<img src="/{index}.png">"#));
            served.push(subresource(
                &format!("https://example.com/{index}.png"),
                "image/png",
                &oversized,
            ));
        }
        markup.push_str("</body></html>");
        let engine = ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, &markup)])
            .serving(served);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.assets_stored, 0);
        // Four transfers of eight megabytes crosses the thirty-two the capture may spend, and
        // the overshoot is one file rather than every remaining reference.
        assert_eq!(
            run.asset_fetches, 4,
            "a refused transfer was not charged to the capture"
        );
        let capture = only_capture_of(&archive, "https://example.com/a");
        let reasons: Vec<&AssetMiss> = capture
            .assets_missed
            .iter()
            .map(|missed| &missed.reason)
            .collect();
        assert_eq!(
            reasons,
            [
                &AssetMiss::TooLarge {
                    byte_len: 8 * 1024 * 1024 + 1
                },
                &AssetMiss::TooLarge {
                    byte_len: 8 * 1024 * 1024 + 1
                },
                &AssetMiss::TooLarge {
                    byte_len: 8 * 1024 * 1024 + 1
                },
                &AssetMiss::TooLarge {
                    byte_len: 8 * 1024 * 1024 + 1
                },
                &AssetMiss::ByteCeilingReached,
            ]
        );
    }

    /// An address the archive can judge on its own is refused without a request. Asking the
    /// engine would spend a call that never leaves the machine and come back as a reason to be
    /// read out of an error string.
    #[test]
    fn a_reference_pointing_inside_a_network_is_refused_without_being_asked_for() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![page(
            "https://example.com/a",
            200,
            r#"<html><link rel="stylesheet" href="http://169.254.169.254/latest/"></html>"#,
        )]);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.asset_fetches, 0);
        assert!(engine.urls_fetched().is_empty());
        let capture = only_capture_of(&archive, "https://example.com/a");
        assert_eq!(capture.assets_missed[0].reason, AssetMiss::InsideANetwork);
    }

    /// The shape an archiver is aimed at something with: one page, a thousand addresses on a
    /// host somebody else picked, none of them answering. It is also where all three bounds meet,
    /// so what each one refused is asserted rather than only the total.
    #[test]
    fn a_page_referencing_a_thousand_dead_addresses_costs_three_requests() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut markup = String::from("<html><body>");
        for index in 0..1_000 {
            markup.push_str(&format!(
                r#"<img src="https://victim.example/{index}.png">"#
            ));
        }
        markup.push_str("</body></html>");
        let engine = ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, &markup)]);

        let run = capture_seed(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.assets_stored, 0);
        assert_eq!(
            run.asset_fetches, 3,
            "the run kept asking a host that had answered nothing three times over"
        );
        let capture = only_capture_of(&archive, "https://example.com/a");
        let counted = |wanted: &AssetMiss| {
            capture
                .assets_missed
                .iter()
                .filter(|missed| {
                    std::mem::discriminant(&missed.reason) == std::mem::discriminant(wanted)
                })
                .count()
        };
        // Three requests, then the capture stops asking, and the count ceiling takes over from
        // there: it is what keeps the record itself from growing with a page's ambitions.
        assert_eq!(
            counted(&AssetMiss::NoResponse {
                detail: String::new()
            }),
            3
        );
        assert_eq!(counted(&AssetMiss::NothingWasAnswering), 125);
        assert_eq!(counted(&AssetMiss::CountCeilingReached), 872);
        assert_eq!(capture.assets_missed.len(), 1_000);
    }
}
