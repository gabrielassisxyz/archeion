//! The crawl boundary: what the archive asks a crawl engine for, and the one adapter that
//! answers today.
//!
//! The reasoning behind the shape of the boundary is written down in
//! `docs/crawl-boundary.md`.

mod boundary;
mod spider_engine;

pub(crate) use boundary::points_inside_a_network;
pub use boundary::{
    CrawlEngine, CrawlError, CrawlOutcome, CrawlStop, FetchFailure, PageEvent, PageResponse, Seed,
};
pub use spider_engine::SpiderEngine;
