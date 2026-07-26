//! Running a seed into the archive: the one place a crawl engine and the store meet.
//!
//! Nothing here knows which engine is underneath, and the engine knows nothing about the
//! archive. What connects them is a page event turning into a capture, which is where
//! canonicalization decides the address the page is filed under.

use std::ops::ControlFlow;

use crate::canonical_url::{CanonicalUrl, InvalidCanonicalUrl};
use crate::crawl::{CrawlEngine, CrawlError, PageEvent, Seed};
use crate::storage::{Archive, Header, NewCapture, StorageError};

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error(transparent)]
    Crawl(#[from] CrawlError),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// A page the crawl fetched and the archive has no address for. It is reported rather than
/// counted, because the address is the whole reason it was refused and a number leaves
/// nothing to look at.
#[derive(Debug, PartialEq, Eq)]
pub struct UnaddressablePage {
    pub url: String,
    pub reason: InvalidCanonicalUrl,
}

/// What one seed left behind. Every page the engine produced is in exactly one of these
/// counts, so a run that archived less than expected says where the rest went.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CaptureRun {
    pub captures_written: usize,
    pub unaddressable_pages: Vec<UnaddressablePage>,
    /// Pages the engine fetched that never reached the archive, straight from the engine.
    pub pages_dropped: usize,
}

/// Crawls a seed and stores every page it produces.
///
/// A page the archive cannot address is skipped and reported: one URL the canonical rules
/// refuse says nothing about the other two hundred. A failed write is the opposite, and
/// stops the run: the disk that rejected this capture will reject the next one, and a
/// crawl that keeps fetching after that spends a site's bandwidth on nothing.
pub fn capture_seed(
    engine: &dyn CrawlEngine,
    archive: &Archive,
    seed: &Seed,
) -> Result<CaptureRun, CaptureError> {
    let mut run = CaptureRun::default();
    let mut write_failure: Option<StorageError> = None;

    let outcome = engine.crawl(seed, &mut |page| {
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

        match archive.write_capture(new_capture(canonical, page)) {
            Ok(_) => {
                run.captures_written += 1;
                ControlFlow::Continue(())
            }
            Err(error) => {
                write_failure = Some(error);
                ControlFlow::Break(())
            }
        }
    })?;

    if let Some(error) = write_failure {
        return Err(error.into());
    }
    run.pages_dropped = outcome.pages_dropped;
    Ok(run)
}

fn new_capture(canonical_url: CanonicalUrl, page: PageEvent) -> NewCapture {
    let media_type = media_type_of(&page.headers);
    NewCapture {
        canonical_url,
        requested_url: page.requested_url,
        final_url: page.final_url,
        status: page.status,
        media_type,
        response_headers: page.headers,
        body: page.body,
        fetched_at: page.fetched_at,
        // Assets are captured by their own pass over the page, which does not exist yet.
        assets: Vec::new(),
    }
}

/// The media type without its parameters: `text/html` out of `text/html; charset=utf-8`.
/// Nothing is lost by narrowing it, since the header survives verbatim in the record, and
/// the field then holds what its name promises instead of a string every reader re-parses.
fn media_type_of(headers: &[Header]) -> Option<String> {
    headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("content-type"))
        .map(|header| {
            header
                .value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
        })
        .filter(|media_type| !media_type.is_empty())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

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
    }

    fn page(url: &str, status: u16, body: &str) -> PageEvent {
        PageEvent {
            requested_url: url.to_owned(),
            final_url: url.to_owned(),
            status,
            headers: vec![Header {
                name: "content-type".to_owned(),
                value: "text/html; charset=utf-8".to_owned(),
            }],
            body: body.as_bytes().to_vec(),
            fetched_at: "2026-07-25T14:03:22Z".parse().expect("valid timestamp"),
        }
    }

    fn archive_in(dir: &TempDir) -> Archive {
        Archive::open(dir.path()).expect("archive opens in an empty directory")
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

        assert!(matches!(error, CaptureError::Storage(_)));
        assert_eq!(engine.pages_offered(), 1);
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
        redirected.requested_url = "https://example.com/short-link".to_owned();
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
