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

use std::future::Future;
use std::ops::ControlFlow;
use std::sync::Once;
use std::time::Duration;

use jiff::Timestamp;
use spider::configuration::RedirectPolicy;
use spider::page::Page;
use spider::reqwest::header::{CONTENT_LENGTH, HeaderMap};
use spider::tokio::runtime::{Builder, Runtime};
use spider::tokio::sync::broadcast::Receiver;
use spider::tokio::sync::broadcast::error::{RecvError, TryRecvError};
use spider::website::Website;
use url::Url;

use super::boundary::{
    CrawlEngine, CrawlError, CrawlOutcome, CrawlStop, FetchFailure, PageEvent, PageResponse, Seed,
    is_internal_host,
};
use crate::storage::Header;

/// Archiving under a name that says what it is and where to complain about it. A crawler
/// that hides behind a browser's user agent is asking to be blocked once it is found out.
const USER_AGENT: &str = concat!(
    "archeion/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/gabrielassisxyz/archeion)"
);

/// The most one response may spend before the archive stops reading it.
///
/// This is the only limit here whose absence costs more than the record it applies to. A
/// response that declares no length is read until it ends, and one that never ends fills
/// memory until the process dies with the whole run still inside it. The engine's own
/// ceiling is two gigabytes and applies only to a response that declares its length, which
/// is to say only to the responses that were never the problem.
///
/// Sixty-four megabytes is far above any page and far below what losing a run costs. What
/// exceeds it is kept up to the ceiling and marked as short, which is the trade the number
/// buys: a partial record that says it is partial, rather than no run at all.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// The smallest ceiling the engine will honour. It raises anything between one byte and this
/// to this, silently, so a caller asking for less would be told a number no run applies. It
/// is published for the callers that have to refuse such a request rather than pass it on.
pub const SMALLEST_MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// The engine reads its byte ceiling from here and from nowhere else on the plain HTTP
/// path: both of its configurable byte limits are browser-only.
const RESPONSE_BYTE_CEILING: &str = "SPIDER_MAX_SIZE_BYTES";

/// How long a redirect chain may get before the URL is abandoned.
const MAX_REDIRECTS: usize = 7;

pub struct SpiderEngine;

impl CrawlEngine for SpiderEngine {
    fn check_seed(&self, seed: &Seed) -> Result<(), CrawlError> {
        usable_seed_url(seed).map(|_url| ())
    }

    fn crawl(
        &self,
        seed: &Seed,
        on_page: &mut dyn FnMut(PageEvent) -> ControlFlow<()>,
    ) -> Result<CrawlOutcome, CrawlError> {
        apply_response_byte_ceiling();
        let start = usable_seed_url(seed)?;
        let runtime = Runtime::new().map_err(|source| CrawlError::EngineUnavailable { source })?;
        Ok(runtime.block_on(crawl_seed(&start, seed, on_page)))
    }

    fn fetch(&self, url: &str, seed: &Seed) -> PageEvent {
        apply_response_byte_ceiling();
        match usable_url(url, seed.allow_private_addresses) {
            Ok(target) => fetch_off_the_crawl_runtime(&target, seed),
            Err(reason) => PageEvent::NoResponse(FetchFailure {
                url: url.to_owned(),
                reason,
            }),
        }
    }
}

/// Fetches one URL on a thread of this call's own.
///
/// The thread is not an optimization, it is the only place the fetch can happen. A runtime
/// cannot be entered from a thread that is already driving one, and a fetch reaches here
/// through the page callback of a crawl, which runs inside the runtime `crawl` built: doing
/// the work on this thread would panic on the first subresource of the first page.
///
/// It also contains a panic inside the engine to the one subresource it happened on. A run
/// that has archived four hundred pages should not lose the rest of a site because a
/// stylesheet took a dependency down a path it does not handle.
fn fetch_off_the_crawl_runtime(url: &str, seed: &Seed) -> PageEvent {
    std::thread::scope(
        |threads| match threads.spawn(|| fetch_one_url(url, seed)).join() {
            Ok(event) => event,
            Err(_) => PageEvent::NoResponse(FetchFailure {
                url: url.to_owned(),
                reason: "the crawl engine panicked while fetching".to_owned(),
            }),
        },
    )
}

/// One request, under the same configuration a crawl of this URL would have run under.
///
/// The website exists for its client and is never crawled. That is what carries the policy
/// across: the redirect screening and the chain limit, the request timeout, the user agent,
/// and the byte ceiling the environment already holds. It is built around the URL being
/// fetched rather than around the page that referenced it, so a subresource on a content
/// network is judged against its own host, which is where a redirect of it would have to
/// stay.
///
/// Two settings on the seed do not reach a fetch. The retry budget belongs to the crawl loop,
/// so a subresource that failed is not asked twice and is reported as missed. The politeness
/// delay is the crawl's too, and what stands in for it here is that the pass making these
/// calls makes them one at a time.
fn fetch_one_url(url: &str, seed: &Seed) -> PageEvent {
    let runtime = match Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(source) => {
            return PageEvent::NoResponse(FetchFailure {
                url: url.to_owned(),
                reason: format!("the crawl engine could not be started: {source}"),
            });
        }
    };
    runtime.block_on(async {
        let client = configured_website(url, seed).configure_http_client();
        page_event(Page::new_page(url, &client).await)
    })
}

/// Puts the byte ceiling where the engine will look for it, once per process.
///
/// The environment is the only channel available, so the ceiling is process-wide rather
/// than a number on the seed, and the engine reads it on its first fetch and keeps that
/// value for the life of the process. This runs before the runtime that performs a fetch is
/// built, which is what makes the write both effective and the last moment it can happen.
///
/// It is also the reason the write is placed here and not deeper: nothing this crate starts
/// is running yet. A caller that crawls from two threads at once is outside what that
/// argument covers, and can settle the variable itself before starting either.
fn apply_response_byte_ceiling() {
    static APPLIED: Once = Once::new();
    APPLIED.call_once(|| {
        let settled = std::env::var_os(RESPONSE_BYTE_CEILING);
        if let Some(ceiling) = response_byte_ceiling(settled.is_some()) {
            // SAFETY: the first crawl or fetch of the process, before its runtime exists.
            unsafe { std::env::set_var(RESPONSE_BYTE_CEILING, ceiling) };
        }
    });
}

/// What to put in the environment, given whether it already carries something. An operator
/// who set a ceiling is making this same decision with a number they chose, so theirs
/// stands, including a zero that turns the ceiling off.
fn response_byte_ceiling(already_settled: bool) -> Option<String> {
    if already_settled {
        None
    } else {
        Some(DEFAULT_MAX_RESPONSE_BYTES.to_string())
    }
}

/// Chooses the ceiling for this process, ahead of the default above.
///
/// It exists because the ceiling has no other channel: the engine reads one environment
/// variable and nothing else, so a caller that wants a different number has to write the
/// same variable this file does, and the variable's name is not something a second place
/// should know. What is settled here stands, since the default is only applied to an
/// environment that carries nothing.
///
/// The value is process-wide and read by the engine on its first fetch, which is a property
/// of the engine rather than a choice made here. A caller for which that is a lie, one
/// process running two seeds that want different ceilings, cannot have what it is asking
/// for and should not be calling this.
///
/// A number under `SMALLEST_MAX_RESPONSE_BYTES` is not the number that will be applied: the
/// engine raises it to that floor without saying so. Refusing such a request belongs to the
/// caller, which is the only place that knows how to tell somebody, so what is passed here
/// is written down as it arrives rather than quietly corrected twice.
///
/// # Safety
///
/// This writes to the environment, so no other thread of the process may be running. In
/// practice that means the top of `main`, before anything else has started.
pub unsafe fn settle_response_byte_ceiling(bytes: usize) {
    // SAFETY: the caller carries the promise above, that this process is still one thread.
    unsafe { std::env::set_var(RESPONSE_BYTE_CEILING, bytes.to_string()) };
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
    let mut pages = website.subscribe(fetch_concurrency(seed) * 4);
    let mut outcome = CrawlOutcome::default();

    // Scoped so the borrow of the website ends with the crawl it was driving.
    let mut stopped = {
        let crawl = async {
            website.crawl().await;
            // Drops the sender, which is what ends the drain below once it is empty.
            website.unsubscribe();
        };
        crawl_until(crawl, seed.deadline, &mut pages, on_page, &mut outcome).await
    };

    match stopped {
        // The crawl finishing does not mean the queue is empty, and cancelling the drain to
        // learn that would throw away pages already fetched.
        CrawlStop::Exhausted => {
            if drain(&mut pages, on_page, &mut outcome).await {
                outcome.pages_dropped += pages.len();
                stopped = CrawlStop::CallerStopped;
            }
        }
        // What the caller leaves unread is counted like any other loss: those pages cost a
        // fetch each and the archive does not have them. The count is a floor, since a task
        // still in flight can queue another page after the length is read.
        CrawlStop::CallerStopped => outcome.pages_dropped += pages.len(),
        CrawlStop::DeadlineReached => drain_queued(&mut pages, on_page, &mut outcome),
    }

    outcome.stopped = stopped;
    outcome
}

/// Runs a crawl against the caller and the clock, and answers which of the three ended it.
///
/// The crawl arrives as a future rather than as a website because this is the only place a
/// deadline is enforced, and a deadline that can only be exercised by crawling something is
/// a deadline nothing headless can prove. Given a future that never finishes, this is the
/// stalled host the deadline exists for.
async fn crawl_until(
    crawl: impl Future<Output = ()>,
    deadline: Option<Duration>,
    pages: &mut Receiver<Page>,
    on_page: &mut dyn FnMut(PageEvent) -> ControlFlow<()>,
    outcome: &mut CrawlOutcome,
) -> CrawlStop {
    spider::tokio::select! {
        () = crawl => CrawlStop::Exhausted,
        () = budget_spent(deadline) => CrawlStop::DeadlineReached,
        stopped = drain(pages, on_page, outcome) => {
            if stopped { CrawlStop::CallerStopped } else { CrawlStop::Exhausted }
        }
    }
}

/// Completes when the seed's budget is gone, and never when it has none. A branch that
/// answered straight away for a seed without a deadline would end every unbounded crawl on
/// its first poll.
async fn budget_spent(deadline: Option<Duration>) {
    match deadline {
        Some(budget) => spider::tokio::time::sleep(budget).await,
        None => std::future::pending().await,
    }
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

/// Hands over what is already queued and stops there, without waiting for the queue to
/// close. Those pages were paid for before the budget ran out and writing them is local
/// work, but a cancelled crawl leaves senders in tasks that are still winding down: waiting
/// for the queue to close would hand the end of the run back to the thing the deadline just
/// took it from. What the caller does not read is counted, as everywhere else.
fn drain_queued(
    pages: &mut Receiver<Page>,
    on_page: &mut dyn FnMut(PageEvent) -> ControlFlow<()>,
    outcome: &mut CrawlOutcome,
) {
    loop {
        match pages.try_recv() {
            Ok(page) => {
                if on_page(page_event(page)).is_break() {
                    outcome.pages_dropped += pages.len();
                    return;
                }
            }
            Err(TryRecvError::Lagged(lost)) => {
                outcome.pages_dropped += usize::try_from(lost).unwrap_or(usize::MAX);
            }
            Err(TryRecvError::Empty | TryRecvError::Closed) => return,
        }
    }
}

/// Zero permits is a crawl that waits forever on its own semaphore, with no page to stop it
/// through and no deadline to end it, so a zero is corrected rather than obeyed. It is read
/// through here and not at the call sites, which is what keeps the queue sized against the
/// concurrency the engine was actually given.
fn fetch_concurrency(seed: &Seed) -> usize {
    seed.concurrency.max(1)
}

fn configured_website(start: &str, seed: &Seed) -> Website {
    let mut website = Website::new(start);
    website
        .with_limit(seed.max_pages)
        .with_depth(seed.max_depth)
        .with_concurrency_limit(Some(fetch_concurrency(seed)))
        .with_delay(u64::try_from(seed.delay.as_millis()).unwrap_or(u64::MAX))
        // A ceiling on one request, not on the crawl. It reaches the HTTP client, so a
        // request that outlives it is cancelled and reported as the failure it was, with no
        // status invented for it. The engine's own default is 120 seconds.
        .with_request_timeout(Some(seed.request_timeout))
        // The engine repeats a request whose status says repeating might work, which is a
        // 429, a 408 or a server error other than 501, 505 and 511, and never a DNS failure
        // or a redirect loop, since none of those answer differently the second time. Between
        // attempts it waits the longer of an exponential backoff and what the response
        // asked for, which for a 429 is its `Retry-After`. The default is no retry at all.
        .with_retry(seed.max_retries)
        // A seed is one host, and identity comes from where the content ended up, so a
        // redirect off that host files a page under an address the run was never pointed
        // at. Under this policy a hop that leaves the seed's host is not followed: the
        // redirect itself is archived, which is the honest record of what the host said.
        // Every hop is screened for internal addresses under it, as under the looser one.
        .with_redirect_policy(RedirectPolicy::Strict)
        // A chain has to end somewhere, and a loop that ends by exhausting the crawl's
        // budget spends the whole seed on one URL. Seven is the engine's own number, set
        // here so it is a decision this file made rather than a default it inherited.
        .with_redirect_limit(MAX_REDIRECTS)
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

    // A fetch that reached no server still arrives here as a page, carrying a status the
    // engine made up for it: 599 for a DNS failure, 524 for a connection timeout. The
    // error it recorded is the only part of that page that came from reality.
    if let Some(reason) = page.error_status.clone() {
        return PageEvent::NoResponse(FetchFailure {
            url: requested_url,
            reason,
        });
    }

    let final_url = page
        .final_redirect_destination
        .clone()
        .unwrap_or_else(|| requested_url.clone());
    let body = page.get_html_bytes_u8().to_vec();

    PageEvent::Response(PageResponse {
        requested_url,
        final_url,
        status: page.status_code.as_u16(),
        body_truncated: body_is_incomplete(&page, &body),
        headers: headers_of(page.headers.as_ref()),
        body,
        // The engine does not date its responses, so this is when the page reached the
        // archive: later than the fetch by the time it sat in the queue, and the closest
        // honest value available here.
        fetched_at: Timestamp::now(),
    })
}

/// Whether less arrived than the response said would.
///
/// Two shapes reach here and only one of them is labelled. The engine marks a body it cut
/// short itself, which is a stream that errored, one that went idle mid-transfer, or one
/// that ran past a size limit. The shape it does not mark is a response whose declared
/// length was over that limit: it is handed over with the status and headers a server
/// really sent and no body at all, and read as an empty page that is the exact corruption
/// a size limit exists to prevent.
fn body_is_incomplete(page: &Page, body: &[u8]) -> bool {
    if page.content_truncated {
        return true;
    }
    body.is_empty() && status_carries_a_body(page.status_code.as_u16()) && declared_length(page) > 0
}

/// A status that sends no body of its own has nothing to be short of. The 304 is the one
/// that matters, because the etag cache produces them and a 304 repeats the length of the
/// entity it is deliberately not sending.
fn status_carries_a_body(status: u16) -> bool {
    !matches!(status, 100..=199 | 204 | 304)
}

fn declared_length(page: &Page) -> u64 {
    page.headers
        .as_ref()
        .and_then(|headers| headers.get(CONTENT_LENGTH))
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
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

/// Refuses a seed before the engine dials anything.
fn usable_seed_url(seed: &Seed) -> Result<String, CrawlError> {
    usable_url(&seed.url, seed.allow_private_addresses).map_err(|reason| CrawlError::UnusableSeed {
        url: seed.url.clone(),
        reason,
    })
}

/// Whether this engine will dial a URL at all, and the address it would dial.
///
/// The scheme decides what gets opened, and `file:` or `data:` reaching a crawler is the
/// archive reading the local machine. The address decides what gets reached, and it is
/// checked here because the engine screens every redirect hop for the same ranges but never
/// the URL it was handed, which it dials directly.
///
/// Both halves apply to a subresource as much as to a seed, and for the same reason: a page
/// arriving from the open web is the one deciding which addresses the next requests of the
/// run are aimed at, so a reference is screened before it is followed rather than trusted
/// because the archive resolved it a moment ago.
fn usable_url(url: &str, allow_private_addresses: bool) -> Result<String, String> {
    let parsed = Url::parse(url).map_err(|error| error.to_string())?;

    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!(
            "{} is not a scheme this archive fetches",
            parsed.scheme()
        ));
    }
    let Some(host) = parsed.host() else {
        return Err("no host to fetch from".to_owned());
    };
    if !allow_private_addresses && is_internal_host(&host) {
        return Err(format!("{host} is inside a network rather than on the web"));
    }

    Ok(parsed.to_string())
}

#[cfg(test)]
mod tests {
    use spider::tokio::sync::broadcast;

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
            assert!(
                usable_seed_url(&Seed::new(hostile)).is_err(),
                "{hostile} was accepted"
            );
        }
    }

    /// The half of the guard the engine cannot cover: it screens every redirect hop for
    /// these ranges and dials the seed straight, so a seed pointed at the cloud metadata
    /// service, at the machine's own ports or at the network around it is refused here or
    /// nowhere.
    #[test]
    fn a_seed_pointed_inside_a_network_is_refused() {
        for internal in [
            "http://169.254.169.254/latest/meta-data/",
            "http://metadata.google.internal/computeMetadata/v1/",
            "http://metadata.goog/",
            "http://localhost:8000/",
            "http://LocalHost./",
            "http://api.localhost/",
            "http://127.0.0.1/",
            "http://127.1/",
            "http://10.0.0.1/",
            "http://192.168.1.1/",
            "http://172.16.0.1/",
            "http://0.0.0.0/",
            "http://0.0.0.1/",
            "http://0.1.2.3/",
            "http://255.255.255.255/",
            "http://[::1]/",
            "http://[::]/",
            "http://[fc00::1]/",
            "http://[fd00:ec2::254]/",
            "http://[fe80::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[::ffff:10.0.0.1]/",
        ] {
            assert!(
                usable_seed_url(&Seed::new(internal)).is_err(),
                "{internal} was accepted"
            );
        }
    }

    /// Archiving a site served locally is a real thing to ask for, and it is the only way
    /// the fetch path is ever exercised. It has to be asked for, which is the whole
    /// difference between this and the test above.
    #[test]
    fn a_seed_inside_a_network_is_reachable_when_the_run_asked_for_it() {
        let mut seed = Seed::new("http://127.0.0.1:8000/index.html");
        seed.allow_private_addresses = true;

        assert_eq!(
            usable_seed_url(&seed).expect("a local seed the run asked for"),
            "http://127.0.0.1:8000/index.html"
        );
    }

    /// A public address is not refused by a guard that reads every private range as a
    /// prefix, which is the failure that turns a security check into an outage.
    #[test]
    fn a_seed_on_the_public_web_is_not_mistaken_for_a_private_one() {
        for public in [
            "https://example.com/a",
            "http://8.8.8.8/",
            "http://172.32.0.1/",
            "http://[2001:db8::1]/",
            "http://notlocalhost.com/",
        ] {
            assert!(
                usable_seed_url(&Seed::new(public)).is_ok(),
                "{public} was refused"
            );
        }
    }

    /// A subresource is screened by the rule the seed is screened by, and refused before a
    /// socket is opened. The page that named it arrived from the open web, so a capture of a
    /// public page whose stylesheet points at the metadata service is the same request as a
    /// seed pointed there, made one hop later.
    #[test]
    fn a_subresource_this_engine_will_not_dial_is_refused_without_being_fetched() {
        for refused in [
            "http://169.254.169.254/latest/meta-data/",
            "http://metadata.google.internal/computeMetadata/v1/",
            "http://localhost:8000/style.css",
            "http://127.0.0.1/style.css",
            "http://[::1]/style.css",
            "http://10.0.0.1/style.css",
            "file:///etc/passwd",
            "data:text/css,body{}",
            "/style.css",
        ] {
            match SpiderEngine.fetch(refused, &Seed::new("https://example.com/")) {
                PageEvent::NoResponse(failure) => assert_eq!(failure.url, refused),
                PageEvent::Response(response) => {
                    panic!("{refused} was fetched and answered {}", response.status)
                }
            }
        }
    }

    #[test]
    fn a_usable_seed_survives_the_check_as_the_engine_will_see_it() {
        assert_eq!(
            usable_seed_url(&Seed::new("https://example.com/a")).expect("usable seed"),
            "https://example.com/a"
        );
        assert_eq!(
            usable_seed_url(&Seed::new("http://example.com")).expect("usable seed"),
            "http://example.com/"
        );
    }

    #[test]
    fn a_response_without_headers_is_a_page_with_no_headers_and_not_a_failure() {
        assert!(headers_of(None).is_empty());
    }

    /// A header map groups by name and says nothing about the order two different names
    /// arrived in, so what is checked here is the part that is actually preserved: a name
    /// that repeats keeps every one of its values, in the order it sent them.
    #[test]
    fn a_header_that_repeats_keeps_all_of_its_values() {
        let mut map = HeaderMap::new();
        map.append("set-cookie", "a=1".parse().expect("valid header value"));
        map.append(
            "content-type",
            "text/html".parse().expect("valid header value"),
        );
        map.append("set-cookie", "b=2".parse().expect("valid header value"));

        let headers = headers_of(Some(&map));
        let cookies: Vec<&str> = headers
            .iter()
            .filter(|header| header.name == "set-cookie")
            .map(|header| header.value.as_str())
            .collect();

        assert_eq!(headers.len(), 3);
        assert_eq!(cookies, ["a=1", "b=2"]);
    }

    #[test]
    fn a_fetch_that_reached_no_server_is_not_reported_as_a_response() {
        let mut page = Page::default();
        page.error_status = Some("error sending request: dns error".to_owned());

        match page_event(page) {
            PageEvent::NoResponse(failure) => {
                assert_eq!(failure.reason, "error sending request: dns error");
            }
            PageEvent::Response(response) => {
                panic!("a failed fetch was archived as status {}", response.status)
            }
        }
    }

    #[test]
    fn a_redirected_page_is_reported_at_the_address_it_ended_on() {
        let mut page = Page::default();
        page.final_redirect_destination = Some("https://example.com/final".to_owned());

        match page_event(page) {
            PageEvent::Response(response) => {
                assert_eq!(response.final_url, "https://example.com/final");
            }
            PageEvent::NoResponse(failure) => panic!("a response was lost: {}", failure.reason),
        }
    }

    fn response_of(event: PageEvent) -> PageResponse {
        match event {
            PageEvent::Response(response) => response,
            PageEvent::NoResponse(failure) => panic!("a response was lost: {}", failure.reason),
        }
    }

    fn page_declaring(status: u16, content_length: &str) -> Page {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_LENGTH,
            content_length.parse().expect("valid header value"),
        );
        let mut page = Page::default();
        page.status_code = status.try_into().expect("valid status");
        page.headers = Some(headers);
        page
    }

    /// The ceiling is process-wide because the engine gives no other channel for it, so the
    /// one decision left to make is what happens when the process already carries one. An
    /// operator who set a number chose it, including a zero meaning no ceiling at all, and
    /// overwriting that would be this file deciding something it was told.
    #[test]
    fn the_byte_ceiling_is_ours_to_set_and_not_ours_to_override() {
        assert_eq!(
            response_byte_ceiling(false).as_deref(),
            Some("67108864"),
            "the ceiling reaching the engine is not the one this file decided on"
        );
        assert_eq!(response_byte_ceiling(true), None);
    }

    /// A body cut short is the failure the archive can least afford, because nothing about
    /// the record shows it: the status still says 200 and the bytes still parse.
    #[test]
    fn a_body_the_engine_cut_short_is_marked_as_short_in_the_record() {
        let mut page = Page::default();
        page.content_truncated = true;

        assert!(response_of(page_event(page)).body_truncated);
    }

    /// The shape the engine does not mark. A response whose declared length is over the
    /// size limit arrives with the status and headers a server really sent and no body,
    /// which stored as an empty page is what a size limit is supposed to prevent.
    #[test]
    fn a_response_whose_body_was_refused_for_its_size_is_not_stored_as_an_empty_page() {
        let response = response_of(page_event(page_declaring(200, "5242880")));

        assert!(response.body.is_empty());
        assert!(response.body_truncated);
    }

    /// A status that sends no body of its own is not short of one, and the etag cache makes
    /// the 304 a page this crawl really produces.
    #[test]
    fn a_status_that_sends_no_body_is_not_reported_as_truncated() {
        for status in [204, 304] {
            assert!(
                !response_of(page_event(page_declaring(status, "5242880"))).body_truncated,
                "{status} was read as a page missing its body"
            );
        }
    }

    #[test]
    fn a_whole_page_is_not_marked_as_short() {
        assert!(!response_of(page_event(Page::default())).body_truncated);
        assert!(!response_of(page_event(page_declaring(200, "0"))).body_truncated);
    }

    #[test]
    fn a_page_that_did_not_redirect_ends_where_it_started() {
        match page_event(Page::default()) {
            PageEvent::Response(response) => {
                assert_eq!(response.final_url, response.requested_url);
            }
            PageEvent::NoResponse(failure) => panic!("a response was lost: {}", failure.reason),
        }
    }

    /// The queue between the engine and the archive is the one part of this adapter that
    /// can lose data, so it is exercised directly: a channel, more pages than it holds, and
    /// no network anywhere.
    #[test]
    fn a_queue_that_overflowed_costs_those_pages_and_not_the_rest_of_the_crawl() {
        let runtime = Runtime::new().expect("a runtime for the test");
        runtime.block_on(async {
            let (sender, mut pages) = broadcast::channel::<Page>(2);
            for _ in 0..5 {
                sender.send(Page::default()).expect("the receiver is alive");
            }
            drop(sender);

            let mut received = 0usize;
            let mut outcome = CrawlOutcome::default();
            let stopped = drain(
                &mut pages,
                &mut |_| {
                    received += 1;
                    ControlFlow::Continue(())
                },
                &mut outcome,
            )
            .await;

            assert!(!stopped, "the caller never asked to stop");
            assert_eq!(received, 2, "the pages still queued were delivered");
            assert_eq!(outcome.pages_dropped, 3);
        });
    }

    #[test]
    fn a_caller_that_asks_to_stop_is_obeyed_on_the_page_it_asked() {
        let runtime = Runtime::new().expect("a runtime for the test");
        runtime.block_on(async {
            // The sender stays alive, so a drain that ignored the answer would hang here
            // rather than end quietly on a closed queue.
            let (sender, mut pages) = broadcast::channel::<Page>(4);
            sender.send(Page::default()).expect("the receiver is alive");
            sender.send(Page::default()).expect("the receiver is alive");

            let mut received = 0usize;
            let mut outcome = CrawlOutcome::default();
            // Under a deadline, because the failure being guarded against is a drain that
            // keeps reading: without one, the regression stops the test run instead of
            // failing it.
            let stopped = spider::tokio::time::timeout(
                Duration::from_secs(5),
                drain(
                    &mut pages,
                    &mut |_| {
                        received += 1;
                        ControlFlow::Break(())
                    },
                    &mut outcome,
                ),
            )
            .await
            .expect("the drain answered the caller instead of reading on");

            assert!(stopped);
            assert_eq!(received, 1);
        });
    }

    /// The run this whole policy exists for: a host that accepts the connection and then
    /// says nothing. There is no page to hand over, so nothing but the clock can end it.
    #[test]
    fn a_crawl_that_produces_nothing_ends_when_the_seed_budget_does() {
        let runtime = Runtime::new().expect("a runtime for the test");
        runtime.block_on(async {
            // The sender stays alive throughout, which is what a stalled crawl looks like
            // from here: the queue never closes on its own.
            let (_sender, mut pages) = broadcast::channel::<Page>(4);
            let mut outcome = CrawlOutcome::default();

            let stop = spider::tokio::time::timeout(
                Duration::from_secs(5),
                crawl_until(
                    std::future::pending::<()>(),
                    Some(Duration::from_millis(50)),
                    &mut pages,
                    &mut |_| ControlFlow::Continue(()),
                    &mut outcome,
                ),
            )
            .await
            .expect("the crawl ended at its deadline instead of waiting on the host");

            assert_eq!(stop, CrawlStop::DeadlineReached);
        });
    }

    #[test]
    fn a_crawl_that_finishes_inside_its_budget_is_not_reported_as_cut_short() {
        let runtime = Runtime::new().expect("a runtime for the test");
        runtime.block_on(async {
            let (_sender, mut pages) = broadcast::channel::<Page>(4);
            let mut outcome = CrawlOutcome::default();

            let stop = crawl_until(
                std::future::ready(()),
                Some(Duration::from_secs(300)),
                &mut pages,
                &mut |_| ControlFlow::Continue(()),
                &mut outcome,
            )
            .await;

            assert_eq!(stop, CrawlStop::Exhausted);
        });
    }

    /// A seed with no deadline has no timer, and the branch that would carry one has to stay
    /// pending forever rather than answer at once: an eager `None` would end every unbounded
    /// crawl on its first poll, which is the opposite of what it asked for.
    #[test]
    fn a_seed_with_no_deadline_is_not_cut_immediately() {
        let runtime = Runtime::new().expect("a runtime for the test");
        runtime.block_on(async {
            let (_sender, mut pages) = broadcast::channel::<Page>(4);
            let mut outcome = CrawlOutcome::default();

            let stop = crawl_until(
                spider::tokio::time::sleep(Duration::from_millis(20)),
                None,
                &mut pages,
                &mut |_| ControlFlow::Continue(()),
                &mut outcome,
            )
            .await;

            assert_eq!(stop, CrawlStop::Exhausted);
        });
    }

    /// What the deadline cancels is the fetching. Pages that were already paid for are in
    /// memory, and writing them is local work the budget never covered.
    #[test]
    fn the_pages_already_fetched_when_the_budget_ran_out_are_still_handed_over() {
        // The sender stays alive, as it does when a crawl is cancelled with tasks still
        // winding down. A drain that waited for the queue to close would never return.
        let (sender, mut pages) = broadcast::channel::<Page>(4);
        for _ in 0..3 {
            sender.send(Page::default()).expect("the receiver is alive");
        }

        let mut received = 0usize;
        let mut outcome = CrawlOutcome::default();
        drain_queued(
            &mut pages,
            &mut |_| {
                received += 1;
                ControlFlow::Continue(())
            },
            &mut outcome,
        );

        assert_eq!(received, 3);
        assert_eq!(outcome.pages_dropped, 0);
    }

    #[test]
    fn a_caller_that_stops_after_the_deadline_still_costs_what_it_leaves_queued() {
        let (sender, mut pages) = broadcast::channel::<Page>(4);
        for _ in 0..3 {
            sender.send(Page::default()).expect("the receiver is alive");
        }

        let mut received = 0usize;
        let mut outcome = CrawlOutcome::default();
        drain_queued(
            &mut pages,
            &mut |_| {
                received += 1;
                ControlFlow::Break(())
            },
            &mut outcome,
        );

        assert_eq!(received, 1);
        assert_eq!(outcome.pages_dropped, 2);
    }

    /// The seed's policy is worth nothing if it stops at this adapter, and the network path
    /// that would show that up is the one path no test may take. What can be checked without
    /// a socket is that the numbers arrive where the engine reads them.
    #[test]
    fn the_seeds_execution_policy_reaches_the_engine_configuration() {
        let mut seed = Seed::new("https://example.com/");
        seed.request_timeout = Duration::from_secs(7);
        seed.max_retries = 3;

        let website = configured_website("https://example.com/", &seed);

        assert_eq!(
            website.configuration.request_timeout,
            Some(Duration::from_secs(7))
        );
        assert_eq!(website.configuration.retry, 3);
        // The engine's first-byte watchdog answers a stalled request with a 504 it made up,
        // carrying no body and no error mark, which this adapter would read as a response a
        // server sent. The request timeout above is the guard that reports a failure as one.
        assert_eq!(
            website.configuration.http_first_byte_timeout, None,
            "the watchdog that invents a status is on"
        );
    }

    /// A redirect is the one hop the seed guard never sees, and the engine screens it only
    /// under a policy that says to. Which policy is in force is therefore the whole guard
    /// on that side, and losing it looks like nothing at all from here: the crawl still
    /// runs, the pages still arrive, and a hop into the metadata service is followed.
    #[test]
    fn a_redirect_is_screened_and_bounded_rather_than_followed_wherever_it_leads() {
        let website =
            configured_website("https://example.com/", &Seed::new("https://example.com/"));

        assert_eq!(
            website.configuration.redirect_policy,
            RedirectPolicy::Strict,
            "a redirect off the seed's host would be followed and archived under it"
        );
        assert_eq!(website.configuration.redirect_limit, MAX_REDIRECTS);
    }
}
