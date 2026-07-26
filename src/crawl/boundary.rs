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
///
/// A seed is one host: subdomains and other TLDs of the same name are separate seeds, so
/// the budget one seed gets is also the budget that host gets. There is no second, narrower
/// per-domain knob because there is no second domain for it to apply to.
#[derive(Debug, Clone)]
pub struct Seed {
    pub url: String,
    pub max_pages: u32,
    pub max_depth: usize,
    pub concurrency: usize,
    /// How long to wait between requests, which is the politeness knob rather than a limit:
    /// raising it slows the crawl down without bounding it.
    pub delay: Duration,
    /// The wall clock the whole crawl has. It bounds fetching, not the writing of what was
    /// already fetched: pages sitting in the queue when it expires are still archived,
    /// because they cost their bytes already and a local write is not what ran out.
    ///
    /// `None` is a run that is deliberately unbounded, which is a decision to make on
    /// purpose rather than one to reach by leaving a field alone.
    pub deadline: Option<Duration>,
    /// How long one request may take before it counts as no response at all. Unlike the
    /// deadline this has no way to be turned off: a request with no ceiling holds one of
    /// `concurrency` slots for as long as a server feels like holding it open, and nothing
    /// in the record would say the run was one slot narrower for the rest of its life.
    pub request_timeout: Duration,
    /// How many times a request that failed in a way worth repeating is repeated. What is
    /// worth repeating and how long to wait between attempts belongs to the engine, since
    /// it is the only thing that can fetch again; how much of the budget to spend on it is
    /// the archive's call and lives here.
    pub max_retries: u8,
}

impl Seed {
    /// The page count, depth, concurrency and delay are the settings the engine comparison
    /// ran under, so a run started with them produces a known result. The rest is the
    /// policy that comparison showed was missing: one of its domains spent 402 seconds of a
    /// 573 second run, which a per-page limit did nothing about.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            max_pages: 200,
            max_depth: 2,
            concurrency: 16,
            delay: Duration::ZERO,
            // Under the deadline the run that went wrong would have been cut with two
            // thirds of its pages already archived, instead of owning the whole afternoon.
            deadline: Some(Duration::from_secs(300)),
            // The engine's own default is 120 seconds, which at the default concurrency is
            // one dead connection holding a sixteenth of the run for a third of its
            // deadline. Thirty seconds is long for a page and short against the budget.
            request_timeout: Duration::from_secs(30),
            max_retries: 2,
        }
    }
}

/// Why a crawl ended. A run that archived less than expected says which of these it was,
/// rather than leaving a page count to be compared against an expectation nobody wrote down.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CrawlStop {
    /// Nothing was left to fetch inside the seed's limits.
    #[default]
    Exhausted,
    /// The seed's wall-clock budget ran out and the rest of the crawl was cancelled.
    DeadlineReached,
    /// The caller asked to stop, on a page it was handed.
    CallerStopped,
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
    pub stopped: CrawlStop,
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
/// continuing is pointless. A failed write to the archive is one case: the next two hundred
/// pages will fail the same way, so the crawl stops on the first.
///
/// Enforcing `Seed::deadline` is the engine's job, and it is not optional: an engine that
/// stalls with nothing to report never calls `on_page` at all, so nothing above this line
/// gets a turn to end it. What an engine is expected to do when the budget expires is stop
/// fetching, hand over what it already fetched, and answer `CrawlStop::DeadlineReached`. The
/// caller keeps a backstop for an engine that ignores the field, but it fires a good margin
/// after the budget, precisely so that handover is never the thing it cuts short.
pub trait CrawlEngine {
    fn crawl(
        &self,
        seed: &Seed,
        on_page: &mut dyn FnMut(PageEvent) -> ControlFlow<()>,
    ) -> Result<CrawlOutcome, CrawlError>;
}
