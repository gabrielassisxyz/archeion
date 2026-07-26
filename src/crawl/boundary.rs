//! The line between the archival core and whatever crawls the web for it.
//!
//! Everything above this file depends on the types here and never on an engine. All an
//! engine may say is `PageEvent`, all it is asked for is a `Seed`, and both are written in
//! the archive's terms rather than the engine's, so swapping the engine is a new adapter
//! and not a rewrite of the code that stores what it produced.

use std::ops::ControlFlow;
use std::time::Duration;

use jiff::Timestamp;

use crate::storage::Header;

/// Where a crawl starts and the limits it has to stay inside.
#[derive(Debug, Clone)]
pub struct Seed {
    pub url: String,
    pub max_pages: u32,
    pub max_depth: usize,
    pub concurrency: usize,
    /// How long to wait between requests, which is the only politeness knob here: a slow
    /// domain still consumes a whole run, and stopping that is the execution policy the
    /// archive owns rather than a limit passed to an engine.
    pub delay: Duration,
}

impl Seed {
    /// The defaults are the settings the engine comparison ran under. They are a starting
    /// point that produced a known result, not a recommendation: the comparison also
    /// showed a per-page limit is not by itself an execution policy.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            max_pages: 200,
            max_depth: 2,
            concurrency: 16,
            delay: Duration::ZERO,
        }
    }
}

/// What a crawl produced for one URL: a response, or the report that there was none.
///
/// The split is not a detail of one engine. A fetch that never reached a server has no
/// status, no headers and no body, and an engine with nowhere to say so invents them: this
/// one answers 599 for a DNS failure and 524 for a connection timeout. Archiving that as a
/// response would put a status in the record that no server ever sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageEvent {
    Response(PageResponse),
    NoResponse(FetchFailure),
}

/// A URL the crawl never got an answer for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchFailure {
    pub url: String,
    pub reason: String,
}

/// One response, as the engine surfaced it and before anything archival happens to it.
///
/// A non-200 is a response like any other. An archive that keeps only successes cannot
/// answer why something is missing from it, and a 404 recorded at a date is the evidence
/// that the page was already gone then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageResponse {
    /// The address the engine asked for, which differs from the final URL when it
    /// redirected. Both are kept: identity is derived from where the content actually is,
    /// diagnosis needs where the archive went looking.
    pub requested_url: String,
    pub final_url: String,
    pub status: u16,
    pub headers: Vec<Header>,
    pub body: Vec<u8>,
    /// Stamped where the page crosses the boundary, which is the closest an adapter can
    /// get to the fetch it is reporting. A clock read further in would date the write
    /// instead, and would leave the pipeline with a hidden input no test can fix.
    pub fetched_at: Timestamp,
}

/// What a crawl produced beyond its pages. It exists so that a loss is reported rather
/// than inferred from a page count nobody knew to expect.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrawlOutcome {
    /// Pages the engine fetched and the archive never saw, because it could not keep up
    /// with them. Bytes were spent on those fetches and nothing was kept.
    pub pages_dropped: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum CrawlError {
    #[error("{url} cannot be crawled: {reason}")]
    UnusableSeed { url: String, reason: String },
    #[error("the crawl engine could not be started: {source}")]
    EngineUnavailable {
        #[source]
        source: std::io::Error,
    },
}

/// The one thing the archival core asks of a crawl engine.
///
/// The call blocks until the crawl ends, and every page reaches the caller through
/// `on_page` while it runs, so nothing accumulates in memory waiting for the end. An
/// engine that is asynchronous underneath keeps its runtime inside its own adapter: the
/// archive writes to a filesystem, and making the core async to accommodate an engine
/// would let the dependency dictate the shape of everything above it.
///
/// `on_page` answers with a `ControlFlow` because the caller is the one that knows when
/// continuing is pointless. A failed write to the archive is the case that exists today:
/// the next two hundred pages will fail the same way, so the crawl stops on the first.
pub trait CrawlEngine {
    fn crawl(
        &self,
        seed: &Seed,
        on_page: &mut dyn FnMut(PageEvent) -> ControlFlow<()>,
    ) -> Result<CrawlOutcome, CrawlError>;
}
