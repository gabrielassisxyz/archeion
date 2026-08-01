//! The crawl boundary: what the archive asks a crawl engine for, and the one adapter that
//! answers today.
//!
//! The reasoning behind the shape of the boundary is written down in
//! `docs/crawl-boundary.md`.

mod boundary;
mod robots;
mod spider_engine;

pub(crate) use boundary::points_inside_a_network;
pub use boundary::{
    CrawlEngine, CrawlError, CrawlOutcome, CrawlStop, FetchFailure, PageEvent, PageResponse, Seed,
    SessionCookie,
};
pub use spider_engine::{
    DEFAULT_MAX_RESPONSE_BYTES, SMALLEST_MAX_RESPONSE_BYTES, SpiderEngine,
    settle_response_byte_ceiling,
};
