//! Running a seed into the archive: the one place a crawl engine and the store meet.
//!
//! Nothing here knows which engine is underneath, and the engine knows nothing about the
//! archive. What connects them is a page event turning into a capture, which is where
//! canonicalization decides the address the page is filed under.

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use crate::canonical_url::{CanonicalUrl, InvalidCanonicalUrl};
use crate::crawl::{
    CrawlEngine, CrawlError, CrawlStop, FetchFailure, PageEvent, PageResponse, Seed,
    points_inside_a_network,
};
use crate::metadata::{self, PageMetadata, PageSource, UnreadablePage};
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
    /// Pages the engine fetched that never reached the archive, straight from the engine.
    pub pages_dropped: usize,
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

    let outcome = engine.crawl(seed, &mut |event| {
        let answer = capture_page(
            event,
            archive,
            seed.allow_private_addresses,
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
    allow_private_addresses: bool,
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
    if !allow_private_addresses && points_inside_a_network(&page.final_url) {
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
    let capture = match archive.write_capture(new_capture(canonical.clone(), page)) {
        Ok(capture) => capture,
        Err(error) => {
            *write_failure = Some(error);
            return ControlFlow::Break(());
        }
    };
    run.captures_written += 1;

    if let Some(metadata) = extracted
        && let Err(error) = archive.write_metadata(&canonical, &capture.id, &metadata)
    {
        *write_failure = Some(error);
        return ControlFlow::Break(());
    }
    ControlFlow::Continue(())
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

fn new_capture(canonical_url: CanonicalUrl, page: PageResponse) -> NewCapture {
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
        // Assets are captured by their own pass over the page, which does not exist yet.
        assets: Vec::new(),
        assets_missed: Vec::new(),
    }
}

/// The media type without its parameters: `text/html` out of `text/html; charset=utf-8`.
/// Nothing is lost by narrowing it, since the header survives verbatim in the record, and
/// the field then holds what its name promises instead of a string every reader re-parses.
fn media_type_of(headers: &[Header]) -> Option<String> {
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
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;
    use crate::crawl::CrawlOutcome;

    /// A crawl engine that replays a written-down list of page events instead of fetching
    /// anything. It is the whole reason the boundary exists: the pipeline above it is
    /// testable without a network, and what it does with a 404 or with a page it cannot
    /// address is decided by the test rather than by whatever the web answered today.
    struct ScriptedCrawlEngine {
        pages: Vec<PageEvent>,
        outcome: CrawlOutcome,
        /// What the pipeline answered for each page, so a test can prove the crawl stopped
        /// rather than infer it from a count.
        answers: RefCell<Vec<ControlFlow<()>>>,
    }

    impl ScriptedCrawlEngine {
        fn new(pages: Vec<PageEvent>) -> Self {
            Self {
                pages,
                outcome: CrawlOutcome::default(),
                answers: RefCell::new(Vec::new()),
            }
        }

        fn pages_offered(&self) -> usize {
            self.answers.borrow().len()
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
            PageEvent::NoResponse(FetchFailure {
                url: url.to_owned(),
                reason: "this fake was given nothing to answer with".to_owned(),
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
}
