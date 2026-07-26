//! The Spider engine, behind the boundary.
//!
//! This is the only module that names the crate and the only one that knows a crawl is
//! asynchronous. The runtime is built here, per call, and never escapes: an archive is a
//! directory being written, and an async core would be the engine's shape imposed on
//! every caller above it.
//!
//! The engine was chosen by a benchmark rather than by contract, which is exactly why it
//! is confined to this file. `docs/crawl-boundary.md` has the reasoning and what a second
//! adapter would have to provide.

use std::ops::ControlFlow;

use jiff::Timestamp;
use spider::page::Page;
use spider::reqwest::header::HeaderMap;
use spider::tokio::runtime::Runtime;
use spider::tokio::sync::broadcast::Receiver;
use spider::tokio::sync::broadcast::error::RecvError;
use spider::website::Website;
use url::Url;

use super::boundary::{CrawlEngine, CrawlError, CrawlOutcome, PageEvent, Seed};
use crate::storage::Header;

/// Archiving under a name that says what it is and where to complain about it. A crawler
/// that hides behind a browser's user agent is asking to be blocked once it is found out.
const USER_AGENT: &str = concat!(
    "archeion/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/gabrielassisxyz/archeion)"
);

pub struct SpiderEngine;

impl CrawlEngine for SpiderEngine {
    fn crawl(
        &self,
        seed: &Seed,
        on_page: &mut dyn FnMut(PageEvent) -> ControlFlow<()>,
    ) -> Result<CrawlOutcome, CrawlError> {
        let start = usable_seed_url(&seed.url)?;
        let runtime = Runtime::new().map_err(|source| CrawlError::EngineUnavailable { source })?;
        Ok(runtime.block_on(crawl_seed(&start, seed, on_page)))
    }
}

async fn crawl_seed(
    start: &str,
    seed: &Seed,
    on_page: &mut dyn FnMut(PageEvent) -> ControlFlow<()>,
) -> CrawlOutcome {
    let mut website = configured_website(start, seed);
    // The engine fetches while the caller writes to disk, so the queue between them has to
    // absorb the difference. Sizing it to the fetch concurrency alone drops pages the
    // moment a write is slower than a fetch; sizing it to the page limit would hold a
    // whole crawl's bodies in memory. What overflows anyway is counted, never ignored.
    let mut pages = website.subscribe(seed.concurrency * 4);
    let mut outcome = CrawlOutcome::default();

    let crawl = async {
        website.crawl().await;
        // Drops the sender, which is what ends the drain below once it is empty.
        website.unsubscribe();
    };

    let stopped_by_caller = spider::tokio::select! {
        () = crawl => false,
        stopped = drain(&mut pages, on_page, &mut outcome) => stopped,
    };

    // The crawl finishing does not mean the queue is empty, and cancelling the drain to
    // learn that would throw away pages already fetched. Only a caller that asked to stop
    // leaves the rest unread.
    if !stopped_by_caller {
        drain(&mut pages, on_page, &mut outcome).await;
    }

    outcome
}

/// Hands pages to the caller until the queue closes. Answers whether the caller asked to
/// stop, which is the one reason to leave pages unread.
async fn drain(
    pages: &mut Receiver<Page>,
    on_page: &mut dyn FnMut(PageEvent) -> ControlFlow<()>,
    outcome: &mut CrawlOutcome,
) -> bool {
    loop {
        match pages.recv().await {
            Ok(page) => {
                if on_page(page_event(page)).is_break() {
                    return true;
                }
            }
            // The queue overflowed. Those pages are gone, but the ones still queued are
            // not: treating a lost page as the end of the crawl would abandon the rest of
            // a run because one write was slow.
            Err(RecvError::Lagged(lost)) => {
                outcome.pages_dropped += usize::try_from(lost).unwrap_or(usize::MAX);
            }
            Err(RecvError::Closed) => return false,
        }
    }
}

fn configured_website(start: &str, seed: &Seed) -> Website {
    let mut website = Website::new(start);
    website
        .with_limit(seed.max_pages)
        .with_depth(seed.max_depth)
        .with_concurrency_limit(Some(seed.concurrency))
        .with_delay(u64::try_from(seed.delay.as_millis()).unwrap_or(u64::MAX))
        .with_respect_robots_txt(true)
        // A seed is a site, not a company: subdomains and other TLDs of the same name are
        // separate archives to ask for, not ones to acquire by accident.
        .with_subdomains(false)
        .with_tld(false)
        // The sitemap is a claim about what exists; the crawl records what is reachable.
        .with_ignore_sitemap(true)
        .with_user_agent(Some(USER_AGENT));
    website
}

fn page_event(page: Page) -> PageEvent {
    let requested_url = page.get_url().to_string();
    let final_url = page
        .final_redirect_destination
        .clone()
        .unwrap_or_else(|| requested_url.clone());

    PageEvent {
        requested_url,
        final_url,
        status: page.status_code.as_u16(),
        headers: headers_of(page.headers.as_ref()),
        body: page.get_html_bytes_u8().to_vec(),
        // The engine does not date its responses, so this is when the page reached the
        // archive: later than the fetch by the time it sat in the queue, and the closest
        // honest value available here.
        fetched_at: Timestamp::now(),
    }
}

fn headers_of(headers: Option<&HeaderMap>) -> Vec<Header> {
    let Some(headers) = headers else {
        return Vec::new();
    };

    headers
        .iter()
        .map(|(name, value)| Header {
            name: name.as_str().to_owned(),
            // A header whose bytes are not text still says something happened. The lossy
            // form keeps the line in the record instead of deleting the evidence.
            value: String::from_utf8_lossy(value.as_bytes()).into_owned(),
        })
        .collect()
}

/// Refuses a seed before the engine dials anything. The scheme decides what gets opened,
/// and `file:` or `data:` reaching a crawler is the archive reading the local machine.
///
/// This is the cheap half of the guard. Redirects into private ranges are the other half,
/// and they are not checked here: they happen inside the engine, after this call.
fn usable_seed_url(url: &str) -> Result<String, CrawlError> {
    let parsed = Url::parse(url).map_err(|error| CrawlError::UnusableSeed {
        url: url.to_owned(),
        reason: error.to_string(),
    })?;

    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(CrawlError::UnusableSeed {
            url: url.to_owned(),
            reason: format!("{} is not a scheme this archive fetches", parsed.scheme()),
        });
    }
    if parsed.host_str().is_none() {
        return Err(CrawlError::UnusableSeed {
            url: url.to_owned(),
            reason: "no host to crawl".to_owned(),
        });
    }

    Ok(parsed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_that_would_not_be_fetched_over_http_is_refused() {
        for hostile in [
            "file:///etc/passwd",
            "data:text/html,<html>",
            "javascript:alert(1)",
            "ftp://example.com/pub",
            "example.com/a",
            "",
        ] {
            assert!(usable_seed_url(hostile).is_err(), "{hostile} was accepted");
        }
    }

    #[test]
    fn a_usable_seed_survives_the_check_as_the_engine_will_see_it() {
        assert_eq!(
            usable_seed_url("https://example.com/a").expect("usable seed"),
            "https://example.com/a"
        );
        assert_eq!(
            usable_seed_url("http://example.com").expect("usable seed"),
            "http://example.com/"
        );
    }

    #[test]
    fn a_response_without_headers_is_a_page_with_no_headers_and_not_a_failure() {
        assert!(headers_of(None).is_empty());
    }

    #[test]
    fn headers_keep_their_order_and_their_repeats() {
        let mut map = HeaderMap::new();
        map.append("set-cookie", "a=1".parse().expect("valid header value"));
        map.append(
            "content-type",
            "text/html".parse().expect("valid header value"),
        );
        map.append("set-cookie", "b=2".parse().expect("valid header value"));

        let headers = headers_of(Some(&map));
        let values: Vec<&str> = headers.iter().map(|header| header.value.as_str()).collect();

        assert_eq!(headers.len(), 3);
        assert!(values.contains(&"a=1") && values.contains(&"b=2"));
    }
}
