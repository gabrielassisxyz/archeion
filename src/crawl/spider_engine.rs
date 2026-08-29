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

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::ops::ControlFlow;
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use html_escape::decode_html_entities;
use jiff::Timestamp;
use lol_html::{HtmlRewriter, MemorySettings, Settings, element};
use spider::CaseInsensitiveString;
use spider::RelativeSelectors;
// The vendored `Page::page_links` field is a `hashbrown::HashSet`, not the standard
// library's, since that is the set type `spider` builds its own page model on; aliased so
// the two never read as the same type by name alone the way the compiler already refuses
// to treat them as the same type by structure.
use spider::configuration::RedirectPolicy;
use spider::hashbrown::HashSet as PageLinkSet;
use spider::packages::robotparser::parser::Entry;
use spider::page::Page;
use spider::reqwest::Client;
use spider::reqwest::header::{CONTENT_LENGTH, COOKIE, HeaderMap, HeaderValue, SET_COOKIE};
use spider::tokio::runtime::{Builder, Runtime};
use spider::tokio::sync::broadcast::Receiver;
use spider::tokio::sync::broadcast::error::{RecvError, TryRecvError};
use spider::website::{OnLinkFindCallback, Website};
use url::Url;

use super::boundary::{
    CrawlEngine, CrawlError, CrawlOutcome, CrawlStop, FetchFailure, PageEvent, PageResponse, Seed,
    is_internal_host,
};
use super::robots::{Group, RobotRules, Rule};
use crate::storage::Header;

/// Archiving under a name that says what it is and where to complain about it. A crawler
/// that hides behind a browser's user agent is asking to be blocked once it is found out.
///
/// This is what a seed with no `user_agent` of its own runs under; `docs/cli.md` publishes it
/// as `--user-agent`'s default, which is why it is public rather than confined to this file.
pub const DEFAULT_USER_AGENT: &str = concat!(
    "archeion/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/gabrielassisxyz/archeion)"
);

/// The identity a seed runs under: its own choice if it made one, and the compiled default
/// otherwise. Read once by `configure_for_seed` and once by `robots_rules`, so the request a
/// run sends and the robots group it is judged against never name two different requesters.
fn user_agent_of(seed: &Seed) -> &str {
    seed.user_agent.as_deref().unwrap_or(DEFAULT_USER_AGENT)
}

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

/// A crawl engine that remembers the last `Website` it configured, keyed by the origin it
/// was built for.
///
/// Each URL a sitemap lists becomes its own seed once `--max-depth` is given, and every seed
/// used to crawl behind a `Website` built from scratch, none of which had ever read this
/// host's `robots.txt` before: a sitemap of a few hundred posts turned into as many requests
/// for the same small file. `Website::set_url` documents itself as the way to "re-use
/// configuration and data", and what it carries forward that this project has no other way
/// to rebuild is the robots parser inside it.
///
/// The raw, undecoded rule lines `robots_rules` sometimes recovers from a second fetch (see
/// its own doc comment) are kept in the same slot rather than a second one: they are only
/// ever valid for the same origin the cached `Website` was fetched for, and folding them
/// together is what keeps that scope from being able to drift apart into two independent
/// answers about which origin is current.
///
/// One slot is enough. A run works through one host at a time: a `Seed` is one host by
/// construction, and `capture_sitemap`'s own sub-crawls, the case this exists for, already
/// refuse a listed URL on another host before it ever reaches this engine.
/// An origin, the `Website` last configured for it, and whatever raw rule lines
/// `robots_rules` recovered for it, if any rule needed them.
type CachedForOrigin = (String, Website, Option<Vec<RawRuleLine>>);

#[derive(Default)]
pub struct SpiderEngine {
    reused: Mutex<Option<CachedForOrigin>>,
}

impl SpiderEngine {
    /// The `Website` last configured for this seed's origin, and whatever raw rule lines were
    /// recovered for it, or a fresh `Website` and no lines when there is no entry or it was
    /// configured for a different origin.
    ///
    /// A `robots.txt` read for one origin is not permission to skip reading another's, so a
    /// cache entry for a different origin is dropped rather than carried into a crawl it was
    /// never fetched for.
    fn cached_for(&self, start: &str) -> (Website, Option<Vec<RawRuleLine>>) {
        let origin = origin_of(start);
        let mut slot = self
            .reused
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match slot.take() {
            Some((cached_origin, website, raw_lines)) if cached_origin == origin => {
                (website, raw_lines)
            }
            _ => (Website::new(start), None),
        }
    }

    /// Files a `Website` and its raw rule lines under the origin they were just crawled for,
    /// for the next seed on this engine to ask for.
    fn keep(&self, start: &str, website: Website, raw_lines: Option<Vec<RawRuleLine>>) {
        let mut slot = self
            .reused
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = Some((origin_of(start), website, raw_lines));
    }
}

/// The scope a cached `Website` is good for. `robots.txt` is a per-origin promise, so a
/// `Website` built for one scheme, host and port is never handed to a seed on another: that
/// would carry an answer the site actually being crawled was never asked for.
fn origin_of(url: &str) -> String {
    Url::parse(url)
        .map(|parsed| parsed.origin().ascii_serialization())
        .unwrap_or_else(|_| url.to_owned())
}

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
        let (mut website, mut raw_lines_cache) = self.cached_for(&start);
        let outcome = runtime.block_on(crawl_seed(
            &start,
            seed,
            &mut website,
            &mut raw_lines_cache,
            on_page,
        ));
        self.keep(&start, website, raw_lines_cache);
        Ok(outcome)
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
        // A single fetch never crawls, so the callback this feeds `hop_depth_guard` is
        // never invoked and nothing is ever read back out of the map.
        let depths = Arc::new(Mutex::new(HashMap::new()));
        let mut website = configured_website(url, seed, depths);
        // The client comes from the engine's own setup, the same way a crawl gets one, and not
        // from the builder underneath it. A client built straight from that builder cannot
        // send at all: the request fails before a connection is opened, against a server that
        // never sees one, with an error naming the URL and saying nothing else. Which step of
        // that setup the client depends on is not established, only that the whole of it is
        // enough and that the steps this file could reach on their own are not. It stayed
        // invisible for as long as every fetch here followed a crawl, which is what acquiring
        // the subresources of a page a crawl delivered guarantees, and nothing on this path
        // may depend on somebody else having gone first.
        let (client, _handler) = website.setup_base();
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
    website: &mut Website,
    raw_lines_cache: &mut Option<Vec<RawRuleLine>>,
    on_page: &mut dyn FnMut(PageEvent) -> ControlFlow<()>,
) -> CrawlOutcome {
    // The same clock `seed.deadline` counts from, read once here rather than let recovery
    // take a fresh one of its own: a phase that measured its own budget against a new
    // instant would get the whole deadline over again, which is exactly the mistake
    // `AssetCapture` was built to avoid for the subresource pass. This function is the
    // whole of that budget's owner, crawl and recovery alike, so this is the one place the
    // clock may start.
    let seed_started = Instant::now();
    // Shared with `hop_depth_guard` through `configure_for_seed`, and read again below once
    // the crawl ends: every same-host link the guard judged inside `max_depth` lands here,
    // whether or not the engine ever came back to fetch it.
    let depths: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    configure_for_seed(website, start, seed, Arc::clone(&depths));
    let robots = robots_rules(website, raw_lines_cache, user_agent_of(seed)).await;
    // The engine fetches while the caller writes to disk, so the queue between them has to
    // absorb the difference. Sizing it to the fetch concurrency alone drops pages the
    // moment a write is slower than a fetch; sizing it to the page limit would hold a
    // whole crawl's bodies in memory. What overflows anyway is counted, never ignored.
    let mut pages = website.subscribe(fetch_concurrency(seed) * 4);
    let mut outcome = CrawlOutcome::default();

    // Every URL this crawl actually handed to the caller, response or not. Compared against
    // `depths` once the crawl claims to be done, this is what tells a link the engine
    // dropped in its own frontier apart from one this adapter never promised to follow.
    let mut fetched: HashSet<String> = HashSet::new();
    let scheme = frontier_scheme(start);
    // Named apart from the caller's own `on_page`, and not a shadow of it, because
    // `recover_lost_links` below needs a second closure built the same way once this one's
    // own borrow of `fetched` has ended; a shadow could not be told apart from the
    // parameter it wraps once this scope needed to build another.
    let mut filtered_on_page =
        |event: PageEvent| filter_and_forward(event, &robots, &mut fetched, &scheme, on_page);

    // Scoped so the borrow of the website ends with the crawl it was driving.
    let mut stopped = {
        let crawl = async {
            website.crawl().await;
            // Drops the sender, which is what ends the drain below once it is empty.
            website.unsubscribe();
        };
        crawl_until(
            crawl,
            seed.deadline,
            &mut pages,
            &mut filtered_on_page,
            &mut outcome,
        )
        .await
    };

    match stopped {
        // The crawl finishing does not mean the queue is empty, and cancelling the drain to
        // learn that would throw away pages already fetched.
        CrawlStop::Exhausted => {
            if drain(&mut pages, &mut filtered_on_page, &mut outcome).await {
                outcome.pages_dropped += pages.len();
                stopped = CrawlStop::CallerStopped;
            }
        }
        // What the caller leaves unread is counted like any other loss: those pages cost a
        // fetch each and the archive does not have them. The count is a floor, since a task
        // still in flight can queue another page after the length is read.
        CrawlStop::CallerStopped => outcome.pages_dropped += pages.len(),
        // This engine never answers with the page ceiling. All it learns is that its own
        // crawl future finished, whether that was the whole site or the count it was given,
        // so a phase bounded by the count says so from above this line. The arm is grouped
        // with the deadline rather than left to a wildcard so that an engine that does come
        // to report it hands over the pages it already fetched instead of dropping them.
        CrawlStop::DeadlineReached | CrawlStop::PageCeilingReached => {
            drain_queued(&mut pages, &mut filtered_on_page, &mut outcome);
        }
    }

    outcome.stopped = stopped;
    // Gated by `frontier_claim_is_trustworthy` below. A run stopped by its deadline or by the
    // caller already has an honest reason for what it left behind, and comparing against
    // `depths` there would report the budget as if it were this defect.
    //
    // `robots` is this project's own decision and the only one consulted here, never the
    // engine's: `website.is_allowed_robots` answers a question this project does not ask
    // anymore, because trusting it either silences a real loss or reports one that was
    // never real, in either direction. The engine takes the first rule that matches while
    // RFC 9309 takes the longest, so a `Disallow: /p/` followed by an `Allow: /p/keep` is a
    // link the frontier never queued and this project would have allowed; asking the
    // engine too used to exclude that link from ever being noticed. A `robots.txt` naming
    // the identity the run is using is the other direction: the vendored parser cannot
    // distinguish "no named group applies" from "the named group applies and disallows",
    // both being one `false` out of the same function, so it falls back to the `*` group's
    // own answer for a path the named group already disallowed, and, for a path the named
    // group left unmentioned, returns *allowed* by the named group directly, which is also
    // this project's own answer. That makes every link on such a page pass the engine's
    // frontier gate, which removes the one thing that otherwise keeps the frontier's own
    // race, documented below `recover_lost_links`, from ever losing a page a fresh fetch
    // would recover: a `Disallow` for someone else, or for `*`, usually blocks one of a
    // page's links outright, and a link the frontier never queues at all cannot be caught
    // mid-flight by that race, so a robots file that happens to clear every link for the
    // running identity is exactly the file least likely to have that safety net. Asking
    // only this project's own decision has a cost the engine's own gate used to absorb for
    // free: every remaining disagreement between the two now costs a live request to a path
    // the site's own frontier-visible rules refuse, which is exactly why that request is
    // bound by the same page budget, deadline and pacing as everything else here.
    //
    // A page this project's own decision refuses is still never delivered:
    // `filter_and_forward` already returns before `fetched` ever gains an entry for one,
    // whether or not the engine fetched it, so a refused link never becomes a recovery
    // candidate here either.
    if frontier_claim_is_trustworthy(outcome.stopped, outcome.pages_dropped) {
        let discovered = depths
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let candidates = links_discovered_but_never_fetched(
            &discovered,
            &fetched,
            start,
            seed.max_pages,
            |url| robots.allows(url),
        );
        // The depth each candidate was discovered at, carried alongside its address so
        // recovery can still tell whether a page it recovers is itself allowed to expand,
        // exactly as `hop_depth_guard` would have judged it had the engine fetched it.
        let candidates: Vec<(String, usize)> = candidates
            .into_iter()
            .map(|url| {
                let depth = discovered.get(&url).copied().unwrap_or(seed.max_depth);
                (url, depth)
            })
            .collect();
        drop(discovered);

        // `--max-pages` bounds the whole seed, crawl and recovery together, not recovery's
        // own count starting over from zero: a run that spent eight of its ten pages
        // before the frontier lost anything has two left for recovery, not ten.
        let already_fetched = fetched.len();
        let seed_host = Url::parse(start)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned));
        let selectors = website.setup_selectors();
        // The larger of the two, never the site's alone: `robots_rules` already overwrote
        // `website`'s own delay with the site's `Crawl-delay` whenever one was declared,
        // which on a site asking for less than `--delay` would otherwise let recovery run
        // faster than the crawl itself just did.
        let effective_delay = seed.delay.max(website.get_delay());

        // `filtered_on_page`'s own borrow of `fetched` ended with its last use above, so a
        // second closure built the same way, over the same `robots`, `fetched` and
        // `scheme`, can still reach the same caller.
        let mut recovery_on_page =
            |event: PageEvent| filter_and_forward(event, &robots, &mut fetched, &scheme, on_page);
        let (still_missing, links_recovered, recovery_stop) = recover_lost_links(
            candidates,
            seed,
            already_fetched,
            seed_started,
            &selectors,
            seed_host.as_deref(),
            &scheme,
            effective_delay,
            &depths,
            &robots,
            &mut recovery_on_page,
        );
        outcome.links_recovered = links_recovered;
        outcome.links_never_followed = still_missing;
        outcome.stopped = match recovery_stop {
            RecoveryStop::Exhausted => outcome.stopped,
            RecoveryStop::DeadlineReached => CrawlStop::DeadlineReached,
            RecoveryStop::CallerStopped => CrawlStop::CallerStopped,
        };
    }
    outcome
}

/// What ended `recover_lost_links`'s own attempt at what the crawl's frontier lost, folded
/// into the same `CrawlStop` the crawl phase itself already answered with: a run whose
/// recovery ran out of time or budget is exactly as incomplete as a run whose crawl phase
/// did, and the report says so the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryStop {
    /// Every candidate, and everything discovered while fetching one, was answered: either
    /// archived or reported.
    Exhausted,
    /// The seed's own deadline ran out mid-recovery, read from the same clock the crawl
    /// phase's own budget counts from. Whatever is still queued is left unreported, on the
    /// same reasoning `frontier_claim_is_trustworthy` already applies to the crawl phase: an
    /// honestly incomplete run is not this guard's to flag.
    DeadlineReached,
    /// The caller asked to stop, on a page recovery handed it.
    CallerStopped,
}

/// The one place a page the site refused can still be stopped, and the one place a page
/// recovered by `recover_lost_links` is folded into the same bookkeeping as one the crawl
/// found on its own. The engine's own frontier reads a `Disallow` with an interior wildcard
/// as a literal prefix nothing begins with, so it queues and fetches the page regardless of
/// what `robots.txt` said; refusing it here is what keeps it out of the archive, out of the
/// extraction pass, and out of the subresource requests that would follow it. The request
/// itself is already spent by the time this runs, and no hook this engine offers can
/// prevent it: see `docs/crawl-boundary.md`.
fn filter_and_forward(
    event: PageEvent,
    robots: &RobotRules,
    fetched: &mut HashSet<String>,
    scheme: &str,
    on_page: &mut dyn FnMut(PageEvent) -> ControlFlow<()>,
) -> ControlFlow<()> {
    if !robots.allows(requested_url_of(&event)) {
        return ControlFlow::Continue(());
    }
    fetched.insert(depth_key(requested_url_of(&event), scheme));
    on_page(event)
}

/// Whether a status is worth asking again, the same three shapes `configure_for_seed`'s own
/// retry policy already names for the crawl itself: a 429, a 408, or a server error other
/// than 501, 505 and 511. Kept as one predicate so recovery's own retry loop asks the
/// identical question rather than a second guess at it.
fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 408)
        || ((500..=599).contains(&status) && !matches!(status, 501 | 505 | 511))
}

/// Fetches, and where it can, expands, every link the frontier discovered in scope and this
/// project's own `robots.allows` would have let through, but the engine never asked the
/// site for at all.
///
/// A link reaches here only because the frontier's own account of what it fetched already
/// disagrees with this project's robots decision, and that gap has two causes, neither of
/// which a second trip through the frontier could repair: the vendored matcher disagreeing
/// with `RobotRules` about a rule (see the comment above this function's call site), and the
/// frontier's own well-known race at a concurrency of one, where the crawl decides it is
/// done by asking whether its own newly-found-links accumulator is empty and nothing is in
/// flight, without asking whether the batch it already knew about still had one link left to
/// dequeue. A direct fetch, aimed by this adapter rather than queued through the frontier
/// that already lost it, is the only way to still recover the page.
///
/// A recovered page is not a leaf. `record_discovered_links` folds its own outbound links
/// into the same `depths` bookkeeping a page the crawl queued itself would have gone
/// through, subject to the same `--max-depth`, so a link two hops past the one the frontier
/// lost is still answered rather than silently missing; this project's own robots decision
/// gates a newly discovered child before it is ever queued, exactly as it gates every other
/// candidate here.
///
/// Every bound the crawl itself stayed inside binds recovery too, checked once per
/// candidate rather than once for the whole batch, since the batch itself grows as pages
/// are recovered: `--max-pages`, counted from `already_fetched` rather than from zero, so a
/// seed that spent eight of its ten pages before the frontier lost anything has two left
/// for recovery rather than ten more, and a batch built one page at a time can still cross
/// that ceiling a single check taken at the top would have missed; the seed's own deadline,
/// read from the same clock this call started on rather than a fresh one, because a run
/// that finished its crawl with time to spare can still spend the rest of it recovering
/// forever; and the effective delay, the larger of `--delay` and the site's own
/// `Crawl-delay`, waited between fetches, because the case this exists for, a site polite
/// enough to name this crawler in its `robots.txt`, is exactly the site this must not
/// answer by asking faster than the crawl itself would have.
///
/// A response the site sent is delivered like any other, archived and counted, because a
/// capture is what the server answered; a status of 400 or higher is retried up to the
/// seed's own retry budget and, if it is still refusing past that budget, does not count as
/// having recovered the link. A page nothing answered is delivered as the failure it is and
/// does not count either. Only a status under 400 clears a URL from what this returns.
///
/// What comes back is the URLs still missing once the attempt is over, how many were
/// recovered, counting a child discovered along the way exactly like an initial candidate,
/// and why recovery itself stopped: a network or status failure recovering one is the same
/// loss `links_never_followed` already reports, not a new kind of one; a caller that stops
/// mid-recovery leaves every URL from there on exactly as unfetched as it already was; and
/// a page or deadline ceiling reached mid-recovery leaves whatever is still queued
/// unreported, on the same reasoning `frontier_claim_is_trustworthy` already applies to the
/// crawl phase.
#[allow(clippy::too_many_arguments)]
fn recover_lost_links(
    initial: Vec<(String, usize)>,
    seed: &Seed,
    already_fetched: usize,
    seed_started: Instant,
    selectors: &RelativeSelectors,
    seed_host: Option<&str>,
    seed_scheme: &str,
    effective_delay: Duration,
    depths: &Mutex<HashMap<String, usize>>,
    robots: &RobotRules,
    on_page: &mut dyn FnMut(PageEvent) -> ControlFlow<()>,
) -> (Vec<String>, usize, RecoveryStop) {
    let mut seen: HashSet<String> = initial.iter().map(|(url, _)| url.clone()).collect();
    let mut queue: VecDeque<(String, usize)> = initial.into();
    let mut still_missing = Vec::new();
    // Starts at what the crawl itself already spent, not at zero: `--max-pages` bounds the
    // seed as a whole, and a run that used eight of its ten pages before the frontier lost
    // anything has two left for recovery, not ten more.
    let mut total_fetched = already_fetched;
    let mut recovered_count = 0usize;

    while let Some((url, depth)) = queue.pop_front() {
        // Checked per candidate, since the queue this pulls from can still grow while it
        // runs: a single check taken before this loop began would let a page discovered
        // three iterations in cross a ceiling that check never saw coming.
        if total_fetched >= seed.max_pages as usize {
            return (still_missing, recovered_count, RecoveryStop::Exhausted);
        }
        if seed
            .deadline
            .is_some_and(|budget| seed_started.elapsed() >= budget)
        {
            return (
                still_missing,
                recovered_count,
                RecoveryStop::DeadlineReached,
            );
        }
        if !effective_delay.is_zero() {
            std::thread::sleep(effective_delay);
        }

        let mut attempts = 0u8;
        let (event, has_absolute_base_href, page_links) = loop {
            let attempt = fetch_recovered_page(&url, seed, selectors);
            let should_retry = matches!(
                &attempt.0,
                PageEvent::Response(response) if is_retryable_status(response.status)
            ) && attempts < seed.max_retries;
            if !should_retry {
                break attempt;
            }
            attempts += 1;
            if !effective_delay.is_zero() {
                std::thread::sleep(effective_delay);
            }
        };

        let recovered = matches!(&event, PageEvent::Response(response) if response.status < 400);
        if recovered {
            total_fetched += 1;
            recovered_count += 1;
            if depth < seed.max_depth {
                let mut depths_guard = depths
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let children = record_discovered_links(
                    &url,
                    has_absolute_base_href,
                    page_links.as_deref(),
                    seed_host,
                    seed_scheme,
                    depth,
                    &mut depths_guard,
                    None,
                );
                drop(depths_guard);
                for child in children {
                    if seen.insert(child.clone()) && robots.allows(&child) {
                        queue.push_back((child, depth + 1));
                    }
                }
            }
        } else {
            still_missing.push(url.clone());
        }

        if on_page(event).is_break() {
            still_missing.extend(queue.into_iter().map(|(url, _)| url));
            still_missing.sort();
            return (still_missing, recovered_count, RecoveryStop::CallerStopped);
        }
    }
    still_missing.sort();
    (still_missing, recovered_count, RecoveryStop::Exhausted)
}

/// Fetches one URL exactly as `fetch_one_url` does, on a thread of this call's own for the
/// same reason, and additionally runs the same link extraction a crawl would run on the
/// page, so a page `recover_lost_links` reaches directly is not a dead end for the links it
/// names.
///
/// `base` is always `None`: the vendored `Page::base` field, which the engine's own crawl
/// loop reads to resolve against an absolute `<base href>`, is private to that crate and
/// unreachable from here. `None` is also what the overwhelming majority of pages, which
/// declare no `<base>` at all, already resolve against during an ordinary crawl, and the
/// one page shape it is not is excluded before this project ever reads `page.page_links`
/// at all: `page_declares_an_absolute_base_href` is computed straight off the response
/// body, independently of whatever `Page::links` resolved its own return value against,
/// which is discarded here and never read.
///
/// A runtime, a `Website` and a client of its own, one per call, exactly like `fetch_one_url`
/// beside it: neither reuses a connection across candidates. That is free at the one lost
/// link this was built for and is not free at a batch recovery's own page budget can still
/// let through, and the decision here is to leave it, on the same reasoning `fetch_one_url`
/// already stands on for a subresource pass: recovery is bounded by `--max-pages`, `--delay`
/// and the seed's own deadline exactly as the crawl is, so the batch it ever runs against is
/// the same size the operator already chose to accept, not an unbounded one a connection
/// pool would be answering for.
fn fetch_recovered_page(
    url: &str,
    seed: &Seed,
    selectors: &RelativeSelectors,
) -> (
    PageEvent,
    bool,
    Option<Box<PageLinkSet<CaseInsensitiveString>>>,
) {
    std::thread::scope(|threads| {
        match threads
            .spawn(|| fetch_recovered_page_on_this_thread(url, seed, selectors))
            .join()
        {
            Ok(result) => result,
            Err(_) => (
                PageEvent::NoResponse(FetchFailure {
                    url: url.to_owned(),
                    reason: "the crawl engine panicked while fetching".to_owned(),
                }),
                false,
                None,
            ),
        }
    })
}

fn fetch_recovered_page_on_this_thread(
    url: &str,
    seed: &Seed,
    selectors: &RelativeSelectors,
) -> (
    PageEvent,
    bool,
    Option<Box<PageLinkSet<CaseInsensitiveString>>>,
) {
    let runtime = match Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(source) => {
            return (
                PageEvent::NoResponse(FetchFailure {
                    url: url.to_owned(),
                    reason: format!("the crawl engine could not be started: {source}"),
                }),
                false,
                None,
            );
        }
    };
    runtime.block_on(async {
        let depths = Arc::new(Mutex::new(HashMap::new()));
        let mut website = configured_website(url, seed, depths);
        let (client, _handler) = website.setup_base();
        let mut page = Page::new_page(url, &client).await;
        // `Page::links` only collects into `page_links` when the field already holds
        // `Some`, gated on that before the rewriter it drives ever runs: an ordinary crawl
        // arrives here already seeded by `with_return_page_links(true)`, and a page fetched
        // directly has nothing to seed it, so this is that seeding's one other call site.
        page.page_links = Some(Default::default());
        let _ = page.links(selectors, &None).await;
        let has_absolute_base_href = page_declares_an_absolute_base_href(&page);
        let page_links = page.page_links.clone();
        (page_event(page), has_absolute_base_href, page_links)
    })
}
async fn robots_rules(
    website: &mut Website,
    raw_lines_cache: &mut Option<Vec<RawRuleLine>>,
    user_agent: &str,
) -> RobotRules {
    let (client, _control) = website.setup_base();
    website.configure_robots_parser(&client).await;
    let (groups, crawl_delay) = {
        let Some(parser) = website.get_robots_parser().as_deref() else {
            return RobotRules::everything_allowed();
        };
        // The states of the file that are not rules, a `robots.txt` the host answered 401 or
        // 403 for and one it answered a 4xx for, are deliberately not read here. Measured
        // against a loopback server answering 403 for its own `robots.txt`: the engine
        // screens its own seed through `is_allowed_robots` before fetching it, so a run
        // archives nothing at all and this is never asked. Reading the flags anyway would be
        // a second copy of a decision that has no case left to decide.
        //
        // That reasoning now also carries `recover_lost_links`, which asks nothing but this
        // return value: a 401 or 403 leaves `entries` and the base entry empty exactly as a
        // missing file would, `RobotRules::for_agent` reads that as everything allowed, and
        // the seed being refused before its first fetch means `crawl_seed` never discovers a
        // link to hand recovery in the first place. A 4xx the parser reads as allow-all is
        // the same case from the other side, and needs no separate answer either.
        let entries: Vec<&Entry> = parser
            .get_entries()
            .iter()
            .chain(std::iter::once(parser.get_base_entry()))
            .collect();
        let raw_lines: Vec<RawRuleLine> = if needs_raw_rules(&entries) {
            match raw_lines_cache.as_ref() {
                Some(cached) => cached.clone(),
                None => {
                    let fetched = raw_rule_lines(&client, website.get_url_parsed()).await;
                    *raw_lines_cache = Some(fetched.clone());
                    fetched
                }
            }
        } else {
            Vec::new()
        };
        let mut raw_lines = raw_lines.into_iter();
        let groups = entries
            .into_iter()
            .map(|entry| group_of(entry, &mut raw_lines))
            .collect();
        let crawl_delay = parser.get_crawl_delay(&website.configuration.user_agent);
        (groups, crawl_delay)
    };
    if let Some(delay) = crawl_delay {
        website.with_delay(u64::try_from(delay.as_millis().min(60_000)).unwrap_or(u64::MAX));
    }
    RobotRules::for_agent(groups, user_agent)
}

/// Whether any already-decoded rule could have come from an escaped `%2A` or `%24`: the
/// vendored parser's decode is the identity function on every other octet this matcher
/// treats specially, so a pattern with neither character present, decoded or otherwise,
/// cannot have lost anything a second fetch would recover.
fn needs_raw_rules(entries: &[&Entry]) -> bool {
    entries
        .iter()
        .flat_map(|entry| &entry.rulelines)
        .any(|line| line.path.contains(['*', '$']))
}

/// One `Allow:` or `Disallow:` line exactly as `robots.txt` spelled its value, read before
/// anything decodes it. Cloned rather than borrowed out of `SpiderEngine`'s per-origin cache,
/// since each `robots_rules` call consumes its own iterator over the lines to correlate them
/// against that call's own rules, and the cache has to stay intact for the next one.
#[derive(Clone)]
struct RawRuleLine {
    raw_pattern: String,
    allowed: bool,
}

/// The same `robots.txt` the vendored parser already read, fetched again through the same
/// client so `group_of` can hand the matcher a rule's undecoded spelling instead of the
/// vendor's fully percent-decoded one. A second request is the cost of that, paid only when
/// `needs_raw_rules` found a reason to: the vendored parser's `path` field is the only thing
/// it publishes for a rule, decoding already applied and irreversible, so there is no way to
/// recover an escaped octet from it after the fact.
///
/// Scanning here is deliberately shallow, a comment stripped, a line trimmed, a keyword
/// matched case-insensitively, rather than a second copy of the vendored parser's grouping.
/// It does not need to be a full parse: `group_of` below only consumes a candidate whose
/// fully-decoded value and allowance match the vendored `RuleLine` it is currently reading,
/// so a line this scan wrongly includes (one before any `User-agent:` line, or one from a
/// second `User-agent: *` group the vendored parser already drops) is simply never claimed by
/// anything and costs nothing.
///
/// A fetch that fails, or a response that is not valid UTF-8, leaves the caller with no raw
/// lines at all: `group_of` then falls back to the vendor's decoded spelling for every rule,
/// which is the behavior this bead found and not a new failure this introduces.
async fn raw_rule_lines(client: &Client, base_url: &Option<Box<Url>>) -> Vec<RawRuleLine> {
    let Some(base_url) = base_url.as_deref() else {
        return Vec::new();
    };
    let Ok(robots_url) = base_url.join("/robots.txt") else {
        return Vec::new();
    };
    let Ok(response) = client.get(robots_url).send().await else {
        return Vec::new();
    };
    let Ok(text) = response.text().await else {
        return Vec::new();
    };

    let mut seen_user_agent = false;
    let mut lines = Vec::new();
    for line in text.lines() {
        let line = match line.find('#') {
            Some(hash) => &line[..hash],
            None => line,
        }
        .trim();
        let Some(colon) = line.find(':') else {
            continue;
        };
        let keyword = line[..colon].trim();
        let value = line[colon + 1..].trim();
        if keyword.eq_ignore_ascii_case("user-agent") {
            seen_user_agent = true;
        } else if seen_user_agent && keyword.eq_ignore_ascii_case("disallow") {
            lines.push(RawRuleLine {
                raw_pattern: value.to_owned(),
                allowed: false,
            });
        } else if seen_user_agent && keyword.eq_ignore_ascii_case("allow") {
            lines.push(RawRuleLine {
                raw_pattern: value.to_owned(),
                allowed: true,
            });
        }
    }
    lines
}

/// What the vendored parser's own `percent_decode` reduces a rule's raw value to, applied the
/// same way here so a candidate from `raw_rule_lines` can be matched against the `RuleLine`
/// the vendored parser produced from it. This is not the RFC 9309 representation the matcher
/// wants; it is the lossy one the matcher does not, and it exists only to recognize which
/// raw line a decoded `RuleLine` came from.
fn vendor_percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let decoded = (bytes[i] == b'%')
            .then(|| bytes.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match decoded {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

/// One parsed group, as this project's own matcher wants it. The engine keeps the groups that
/// name a crawler apart from the one that names `*`, which it holds as a base entry; both are
/// ordinary groups here, since which of them applies is the question the matcher answers.
///
/// `raw_lines` is consumed in file order and never rewound: for each of `entry`'s rules, it is
/// advanced past every candidate that does not decode to that rule's own path and allowance,
/// and the first one that does is what the pattern is built from. An empty `raw_lines` (no
/// second fetch happened, or none is left) falls back to the vendored parser's own decoded
/// path, unchanged from before this module normalized anything.
fn group_of(entry: &Entry, raw_lines: &mut impl Iterator<Item = RawRuleLine>) -> Group {
    Group {
        agents: entry.useragents.clone(),
        rules: entry
            .rulelines
            .iter()
            .map(|line| {
                let raw = raw_lines.find(|candidate| {
                    candidate.allowed == line.allowance
                        && vendor_percent_decode(&candidate.raw_pattern) == line.path
                });
                let pattern =
                    raw.map_or_else(|| line.path.clone(), |candidate| candidate.raw_pattern);
                Rule::new(pattern, line.allowance)
            })
            .collect(),
    }
}

/// Whether `CrawlStop::Exhausted` is a claim this run can actually stand behind, which is the
/// gate `crawl_seed` gives the map above before trusting it at all.
///
/// A page counted into `pages_dropped` never reached `on_page`, so it is missing from
/// `fetched` without anything having been refused by a budget: it is the queue between the
/// engine and the archive losing a page it already paid for, which is a different failure
/// with its own honest count already sitting in the outcome. Comparing `depths` against
/// `fetched` while that count is nonzero would blame every link the crawl genuinely left in
/// the frontier behind the lost page on this guard instead, which on a run that lost only a
/// couple of pages out of a large crawl is thousands of lines on stderr for a loss the report
/// already states in one number.
fn frontier_claim_is_trustworthy(stopped: CrawlStop, pages_dropped: usize) -> bool {
    stopped == CrawlStop::Exhausted && pages_dropped == 0
}

/// What the engine discovered as a followable, same-host link and never handed back at
/// all, once a crawl claims there was nothing left to fetch.
///
/// A link only reaches `discovered` if `hop_depth_guard` already judged it in scope, so
/// this is never firing on the depth budget, on the whitelist or the blacklist, since this
/// adapter never sets either, or on a link already visited, since `discovered` only ever
/// holds one entry per URL regardless of how many pages named it. `max_pages` is excluded
/// on purpose too: a link left over when the crawl was told to stop at some number of pages
/// is that budget working as asked, not a link the engine lost, and comparing against the
/// pages this adapter actually received rather than a count read from inside the engine is
/// what keeps the two from being confused. `robots_allows` excludes the one remaining
/// reason the engine declines a link on its own: a rule the site's `robots.txt` states,
/// asked of the engine's own parser rather than reimplemented here.
fn links_discovered_but_never_fetched(
    discovered: &HashMap<String, usize>,
    fetched: &HashSet<String>,
    seed_url: &str,
    max_pages: u32,
    robots_allows: impl Fn(&str) -> bool,
) -> Vec<String> {
    if fetched.len() >= max_pages as usize {
        return Vec::new();
    }
    let seed_key = depth_key(seed_url, &frontier_scheme(seed_url));
    let mut missing: Vec<String> = discovered
        .keys()
        .filter(|url| **url != seed_key && !fetched.contains(*url) && robots_allows(url))
        .cloned()
        .collect();
    missing.sort();
    missing
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

fn configured_website(
    start: &str,
    seed: &Seed,
    depths: Arc<Mutex<HashMap<String, usize>>>,
) -> Website {
    let mut website = Website::new(start);
    configure_for_seed(&mut website, start, seed, depths);
    website
}

/// Applies a seed's policy onto a `Website`, fresh or already carrying an earlier seed's
/// configuration from a previous call `SpiderEngine::crawl` made on it.
///
/// Every setter here replaces what it configures rather than merging with what was already
/// there, which is what makes it safe to run again on a `Website` the engine is reusing
/// across seeds that share a host: the previous seed's concurrency, delay, retries or cookie
/// header do not survive alongside the new one. `set_url` runs first and is not left to
/// `Website::new`, since a reused instance already carries the URL of the seed before this
/// one.
fn configure_for_seed(
    website: &mut Website,
    start: &str,
    seed: &Seed,
    depths: Arc<Mutex<HashMap<String, usize>>>,
) {
    // Fed by `hop_depth_guard`, from the same `page_links` it already walks for its own
    // reason, and read by `rewrite_escaped_href` below: the one place still holding an
    // href's raw text is not the one asked to fetch it, so the correction has to cross
    // from one callback to the other rather than being computed where it is used.
    let corrections: HrefCorrections = Arc::new(Mutex::new(HashMap::new()));
    website
        .set_url(start)
        .with_limit(seed.max_pages)
        // The engine's own depth budget counts path segments of the candidate URL, which
        // is a different question from distance in hops: a chain of one-segment URLs
        // passes it at any length, and a two-segment sibling of the seed fails it at hop
        // one. Zero turns that budget off, and `hop_depth_guard` below is what actually
        // answers to `--max-depth`, fed by the links `with_return_page_links` puts on
        // every page this callback sees.
        .with_depth(0)
        .with_return_page_links(true)
        .with_on_should_crawl_callback_closure(Some(hop_depth_guard(
            start,
            seed.max_depth,
            depths,
            corrections.clone(),
        )))
        .with_on_link_find_callback(Some(rewrite_escaped_href(corrections)))
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
        .with_user_agent(Some(user_agent_of(seed)));
    // Asked about `start` and not about the seed's own URL, because `start` is the address the
    // requests of this client are aimed at: a crawl is built around its seed, and a single
    // fetch around the subresource or listed URL being acquired. A cookie that belongs to
    // another origin is simply not configured, so the run asks for that page anonymously
    // rather than refusing it.
    //
    // Set unconditionally, `None` included, rather than left alone when there is nothing to
    // carry: a `Website` this engine is reusing may still hold a header from the seed before
    // this one, and leaving it would send this seed's requests under a credential it was
    // never given.
    let headers = seed
        .session_cookie
        .as_ref()
        .and_then(|cookie| cookie.value_for(start))
        // A value with a character no header can carry is refused by the command line before a
        // run starts, so this only ever declines one a caller assembled by hand.
        .and_then(|value| HeaderValue::from_str(value).ok())
        .map(|mut cookie| {
            // The guarantee that a credential is never printed stops resting on no logger
            // existing: a sensitive value prints as `Sensitive` from anything that formats
            // the map.
            cookie.set_sensitive(true);
            let mut headers = HeaderMap::new();
            headers.insert(COOKIE, cookie);
            headers
        });
    website.with_headers(headers);
}

/// Answers, for a page the engine just fetched, whether the crawl should follow that
/// page's own links, by tracking how many hops each discovered link is from the seed.
///
/// The engine calls this once per fetched page, after it has already gathered that
/// page's links onto `page.page_links` and before deciding whether to queue them, so a
/// link can only be dequeued once this recorded its depth as the page it was found on
/// plus one. That makes the seed the only page ever missing from the map, which is why
/// it is seeded here at zero.
///
/// A page this cannot place is read as the seed, which fails open: its links are followed
/// as if it were one hop from the start. That is the deliberate direction. A page reaching
/// the fallback is one whose spelling this failed to match, and the two ways to be wrong
/// are not equal for an archive: failing open costs a crawl some pages it did not need,
/// bounded anyway by the page limit and the deadline, while failing closed would silently
/// return less of a site than the run found, which is the outcome this project treats as
/// an actual failure rather than as the web misbehaving.
///
/// What keeps the fallback rare is `depth_key`, because the engine does not queue a URL
/// under the characters the page wrote.
///
/// Pages arrive through concurrent tasks, so the same link is found on two pages at once
/// and the shorter distance wins whichever order they arrive in. It is still not
/// guaranteed to be the shortest distance from the seed: nothing here controls the order
/// the engine dequeues, so a page reachable in two hops and in four can be fetched under
/// the longer one before the shorter is ever recorded. The error only ever refuses pages
/// rather than admitting them, and correcting it means owning the frontier, which is a
/// different piece of work.
///
/// A poisoned lock, which would only mean some other call to this closure panicked mid
/// update, is recovered from rather than allowed to fail every page for the rest of the
/// run.
///
/// `depths` arrives from the caller rather than being built here, because `crawl_seed`
/// reads it again once the crawl ends, to compare what it discovered against what it
/// actually fetched. The seed itself is entered at zero before anything else, which is
/// also why it is the one page never missing from the map.
///
/// `corrections` is unrelated to depth and rides along for a narrower reason: this closure
/// is the only place that ever sees an href as the page wrote it, character references
/// included, before anything resolves it into a URL. `corrected_resolution` uses that to
/// work out what the engine should have requested for a link whose href spelled `&` as
/// `&amp;`, `&#38;` or `&#x26;`, and leaves the answer here for `rewrite_escaped_href` to
/// find when the engine actually asks for it.
fn hop_depth_guard(
    seed_url: &str,
    max_depth: usize,
    depths: Arc<Mutex<HashMap<String, usize>>>,
    corrections: HrefCorrections,
) -> impl Fn(&Page) -> bool + Send + Sync + 'static {
    let seed_host = Url::parse(seed_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned));
    let seed_scheme = frontier_scheme(seed_url);
    depths
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry(depth_key(seed_url, &seed_scheme))
        .or_insert(0);
    move |page: &Page| {
        let mut depths = depths
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let depth = depths
            .get(&depth_key(page.get_url(), &seed_scheme))
            .copied()
            .unwrap_or(0);
        if depth >= max_depth {
            // Nothing found here can be expanded, so recording it would only be a page's
            // chance to spend this crawl's memory on addresses the crawl will never visit.
            return false;
        }
        let mut corrections_guard = corrections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        record_discovered_links(
            page.get_url(),
            page_declares_an_absolute_base_href(page),
            page.page_links.as_deref(),
            seed_host.as_deref(),
            &seed_scheme,
            depth,
            &mut depths,
            Some(&mut corrections_guard),
        );
        true
    }
}

/// Records, into `depths`, every same-host in-scope link a fetched page discovered, one hop
/// past its own depth, and returns the fetch-target address of each one.
///
/// Shared between `hop_depth_guard`, called by the engine once per page it fetched itself,
/// and `recover_lost_links`, called once per page it fetched directly instead: both answer
/// the same question, what a page found and where the crawl's own bookkeeping should file
/// it, and the two have to agree for the comparison `crawl_seed` runs at the end to mean
/// anything.
///
/// `corrections` is `hop_depth_guard`'s alone, `None` from `recover_lost_links`. It exists
/// so `rewrite_escaped_href` can rewrite a request the engine's own frontier is about to
/// send, and recovery never sends a request through that frontier: every corrected spelling
/// this returns is already the address recovery fetches next, with nothing left for a
/// second lookup to find.
#[allow(clippy::too_many_arguments)]
fn record_discovered_links(
    page_url: &str,
    has_absolute_base_href: bool,
    page_links: Option<&PageLinkSet<CaseInsensitiveString>>,
    seed_host: Option<&str>,
    seed_scheme: &str,
    depth: usize,
    depths: &mut HashMap<String, usize>,
    mut corrections: Option<&mut HashMap<String, String>>,
) -> Vec<String> {
    let mut discovered = Vec::new();
    // A page that declares an absolute `<base href>` resolves every one of its links
    // against that value rather than against its own URL, and this has no way to resolve
    // against the same base without reimplementing the engine's own rule for it. Leaving
    // this page's links out of the map entirely is cheaper than reporting an address the
    // site never had as one the crawl lost: see `page_declares_an_absolute_base_href` for
    // why that is the trade being made.
    if has_absolute_base_href {
        return discovered;
    }
    // `page_links` holds hrefs as the page wrote them, which is relative as often as not,
    // while every page later arrives here identified by its absolute URL: without resolving
    // against this page's own address first, a relative link never matches the key its own
    // fetch looks it up under.
    let Some(base) = Url::parse(page_url).ok() else {
        return discovered;
    };
    let Some(links) = page_links else {
        return discovered;
    };
    for link in links.iter() {
        let Some(resolved) = base.join(link.as_ref()).ok() else {
            continue;
        };
        // A crawl never leaves the host it was pointed at, so a link that does is one this
        // will never be asked about. Keeping it would let one page of outbound links cost
        // the whole run's memory.
        if resolved.host_str() != seed_host {
            continue;
        }
        // The engine's own frontier drops anything that is not http or https before it
        // ever forces the scheme below, so a same-host link in another scheme, `ftp://`
        // being the one seen in the wild, is never queued and recording it here would
        // report a fetch the engine was never going to make in the first place.
        if !matches!(resolved.scheme(), "http" | "https") {
            continue;
        }
        // A link whose href spells its own separator with a character reference is fetched
        // under the corrected spelling below, via `rewrite_escaped_href` on the engine's
        // own frontier, or directly by `recover_lost_links`, so it is the corrected address
        // that lands here too: keying this on the buggy one would compare a page this
        // project never asked for against the one it actually requested, and report the
        // difference as a link the crawl lost.
        let corrected = corrected_resolution(&base, link.as_ref(), seed_scheme);
        if let (Some(corrected), Some(corrections)) = (&corrected, corrections.as_deref_mut()) {
            corrections.insert(
                depth_key(resolved.as_str(), seed_scheme),
                corrected.to_string(),
            );
        }
        let fetch_target = corrected.as_ref().unwrap_or(&resolved);
        let key = depth_key(fetch_target.as_str(), seed_scheme);
        depths
            .entry(key)
            .and_modify(|known| *known = (*known).min(depth + 1))
            .or_insert(depth + 1);
        discovered.push(fetch_target.as_str().to_owned());
    }
    discovered
}

/// The shared table `hop_depth_guard` fills and `rewrite_escaped_href` reads from, mapping
/// the address `push_link` itself resolves a link's href to onto the one its page actually
/// meant.
type HrefCorrections = Arc<Mutex<HashMap<String, String>>>;

/// Decodes whatever character references an href carries, `&amp;`, `&#38;` and `&#x26;`
/// among them: the same decode `arch-a3r` already runs for metadata extraction in
/// `src/metadata/mod.rs`, one layer over, on the href text the crawl resolves into a URL.
fn decode_href_character_references(href: &str) -> String {
    decode_html_entities(href).into_owned()
}

/// The address the engine should request for `href` instead of the one `push_link` itself
/// would resolve it to, when decoding its character references changes it; `None` when
/// there is nothing to correct, `href` having no reference to decode.
///
/// Resolved against the same base and forced onto the same scheme as the buggy resolution
/// it replaces, so the two are comparable everywhere both this closure and `depth_key` treat
/// a resolution that way. It never carries a fragment to drop, because decoding the
/// separator the page actually wrote never produces one, which is exactly the shape
/// `&#38;` and `&#x26;` corrupt into before anything but this closure ever sees the href's
/// own text.
fn corrected_resolution(base: &Url, href: &str, seed_scheme: &str) -> Option<Url> {
    let decoded = decode_href_character_references(href);
    if decoded == href {
        return None;
    }
    let mut corrected = base.join(&decoded).ok()?;
    let _ = corrected.set_scheme(seed_scheme);
    Some(corrected)
}

/// The `on_link_find_callback` this file wires up to rewrite a request before it is sent
/// rather than after the fact, which is the only timing that can still save it: by the time
/// anything else the engine offers runs, the request under the spelling `push_link` resolved
/// has already gone out. A link this has no correction for, every ordinary one, passes
/// through unchanged.
fn rewrite_escaped_href(corrections: HrefCorrections) -> OnLinkFindCallback {
    Arc::new(move |link, referrer| {
        let corrected = corrections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(link.as_ref())
            .cloned();
        match corrected {
            Some(url) => (CaseInsensitiveString::from(url), referrer),
            None => (link, referrer),
        }
    })
}

/// How a URL is spelled in the depth map.
///
/// The engine drops a fragment before queueing a link, so an address discovered as
/// `/a#section` is fetched as `/a` and would look up nothing under the spelling it was
/// stored with. It also forces the scheme of every resolved, in-scope link to the scheme
/// below before the link ever reaches its frontier, regardless of what the page wrote, so a
/// hardcoded `http://` link on an `https://` seed, or the mirror of that, is queued under a
/// spelling that has nothing to do with the href's own text. Everything else is left exactly
/// as it arrived: this is a key for matching one crawl's own URLs against each other, not the
/// archive's canonical form, and the two answer different questions.
fn depth_key(url: &str, scheme: &str) -> String {
    match Url::parse(url) {
        Ok(mut parsed) => {
            parsed.set_fragment(None);
            let _ = parsed.set_scheme(scheme);
            parsed.to_string()
        }
        Err(_) => url.to_owned(),
    }
}

/// The scheme every key in the depth map is forced to, taken once from the seed exactly as
/// the dependency takes its own `parent_host_scheme`: read once, at the start of the crawl,
/// and never rebuilt from a redirect. `push_link` overwrites the scheme of every resolved,
/// in-scope link to this same value before it reaches the frontier, so a page that hardcodes
/// the other scheme in an absolute self link is still fetched and archived, only under the
/// seed's scheme rather than the page's own; matching that here is what keeps `depth_key`
/// from recording the link under a spelling the frontier never uses.
///
/// `usable_seed_url` already refuses a seed whose scheme is not http or https before this
/// runs, so the fallback below is unreached in practice.
fn frontier_scheme(seed_url: &str) -> String {
    Url::parse(seed_url)
        .map(|parsed| parsed.scheme().to_owned())
        .unwrap_or_else(|_| "https".to_owned())
}

/// The most a `<base href>` scan may buffer while it looks for one selector. The page body
/// reaching here already passed the response byte ceiling, so this bounds the parser's own
/// working memory rather than the page, the same way `MAX_PARSER_MEMORY_BYTES` does for the
/// metadata scan.
const MAX_BASE_HREF_SCAN_MEMORY_BYTES: usize = 8 * 1024 * 1024;

/// Whether a page declared a `<base href>` that parses as an absolute URL, which is exactly
/// the condition under which the engine's own base-href handler fires and switches every
/// later link on the page to resolve against that value instead of the page's own address.
/// A relative value, `<base href="/">` included, never parses as absolute and is inert on
/// both sides, so it is not a case this has to detect.
///
/// `hop_depth_guard` resolves a page's links against the page's own URL, and has no way to
/// resolve against the engine's base without reimplementing the engine's own rule for it. A
/// page this reports true for has its links left out of the depth map entirely rather than
/// recorded under the wrong resolution: a link this project can no longer place is read as
/// one hop from the seed the same way a page this cannot place already is, which is the
/// direction this whole guard already fails open in, and it costs far less than reporting an
/// address the site never had as one the crawl lost.
fn page_declares_an_absolute_base_href(page: &Page) -> bool {
    let mut found = false;
    let mut rewriter = HtmlRewriter::new(
        Settings::new()
            .with_memory_settings(
                MemorySettings::new()
                    .with_max_allowed_memory_usage(MAX_BASE_HREF_SCAN_MEMORY_BYTES),
            )
            .with_strict(false)
            .append_element_content_handler(element!("base[href]", |el| {
                if !found {
                    found = el
                        .get_attribute("href")
                        .is_some_and(|href| Url::parse(&href).is_ok());
                }
                Ok(())
            })),
        // The rewritten output is the input, and this only ever reads: dropping it keeps
        // the cost of a large page the size of its tokens rather than of itself.
        |_: &[u8]| {},
    );
    let _ = rewriter.write(page.get_html_bytes_u8());
    let _ = rewriter.end();
    found
}

/// The URL a page event is filed under before any redirect, which is the spelling
/// `hop_depth_guard` recorded a discovered link as. The final URL is where a redirected
/// response ended up, and comparing against that would mark a link followed under an
/// address it was never queued under.
fn requested_url_of(event: &PageEvent) -> &str {
    match event {
        PageEvent::Response(response) => &response.requested_url,
        PageEvent::NoResponse(failure) => &failure.url,
    }
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

/// What stands in the record where a `set-cookie` value arrived. It is unmistakable for a
/// cookie a host actually sent, which an empty value would not be: a server may genuinely send
/// one of those.
const DROPPED_HEADER_VALUE: &str = "(dropped by the archive)";

/// The response headers as the archive keeps them, which is every one of them except what a
/// `set-cookie` said.
///
/// The value of that header is dropped with a session and without one, and the header itself
/// stays: the record says it was sent, and how many times, and not what it set. Three reasons,
/// none of which depends on whether a particular run carried a credential.
///
/// A rule that did depend on that would be wrong once, and an archive written under the wrong
/// answer cannot be repaired by changing the answer later. The response body is what this
/// collection is authoritative about, and a cookie is transport state travelling beside it.
/// And it is a repair rather than a precaution: 247 of the 250 captures of one publication
/// already hold 930 of these values between them, anonymously, being the tracking identifiers
/// a session-less reader is issued.
///
/// The cost is real and is stated rather than hidden: a stored response is no longer byte for
/// byte what arrived. It is paid here, at the boundary, so that nothing above this line ever
/// holds a session token, including the metadata and readability passes that read a response
/// before it is written and are re-run over it later. A second engine adapter has to drop it
/// too, and `docs/crawl-boundary.md` says so; nothing guards an adapter that does not exist.
fn headers_of(headers: Option<&HeaderMap>) -> Vec<Header> {
    let Some(headers) = headers else {
        return Vec::new();
    };

    headers
        .iter()
        .map(|(name, value)| Header {
            name: name.as_str().to_owned(),
            value: if name == SET_COOKIE {
                DROPPED_HEADER_VALUE.to_owned()
            } else {
                // A header whose bytes are not text still says something happened. The lossy
                // form keeps the line in the record instead of deleting the evidence.
                String::from_utf8_lossy(value.as_bytes()).into_owned()
            },
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
    use spider::packages::robotparser::parser::RobotFileParser;
    use spider::tokio::sync::broadcast;

    use super::*;
    use crate::crawl::SessionCookie;

    /// A fresh map for a test that has no depths of its own to seed, which is every test
    /// here except the ones asserting on `hop_depth_guard` and `crawl_seed` directly.
    fn empty_depths() -> Arc<Mutex<HashMap<String, usize>>> {
        Arc::new(Mutex::new(HashMap::new()))
    }

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
            match SpiderEngine::default().fetch(refused, &Seed::new("https://example.com/")) {
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

    fn values_of(headers: &[Header], name: &str) -> Vec<String> {
        headers
            .iter()
            .filter(|header| header.name == name)
            .map(|header| header.value.clone())
            .collect()
    }

    /// A header map groups by name and says nothing about the order two different names
    /// arrived in, so what is checked here is the part that is actually preserved: a name
    /// that repeats keeps every one of its values, in the order it sent them.
    ///
    /// Asked of `link`, which repeats on real responses the same way `set-cookie` does, since
    /// that one is the single header whose values the archive deliberately does not keep.
    #[test]
    fn a_header_that_repeats_keeps_all_of_its_values() {
        let mut map = HeaderMap::new();
        map.append(
            "link",
            "</a>; rel=next".parse().expect("valid header value"),
        );
        map.append(
            "content-type",
            "text/html".parse().expect("valid header value"),
        );
        map.append(
            "link",
            "</b>; rel=prev".parse().expect("valid header value"),
        );

        let headers = headers_of(Some(&map));

        assert_eq!(headers.len(), 3);
        assert_eq!(
            values_of(&headers, "link"),
            ["</a>; rel=next", "</b>; rel=prev"]
        );
    }

    /// The companion to the test above, and the one header it does not hold for. What a
    /// `set-cookie` said is not archived, with a session or without one, while the fact that it
    /// was sent and how many times survives: a collection built to be durable and copied
    /// between machines has no business holding session tokens or the tracking identifiers the
    /// same header carries to an anonymous reader.
    #[test]
    fn a_set_cookie_value_does_not_survive_into_the_record() {
        let mut map = HeaderMap::new();
        map.append(
            "set-cookie",
            "substack.sid=secret; Path=/".parse().expect("valid header"),
        );
        map.append(
            "set-cookie",
            "ab_testing_id=%22abc%22".parse().expect("valid header"),
        );
        map.append(
            "content-type",
            "text/html".parse().expect("valid header value"),
        );

        let headers = headers_of(Some(&map));

        assert_eq!(
            values_of(&headers, "set-cookie"),
            [DROPPED_HEADER_VALUE, DROPPED_HEADER_VALUE],
            "the record says the header was sent twice and not what it set"
        );
        assert!(
            !format!("{headers:?}").contains("secret"),
            "a credential reached the record"
        );
        assert_eq!(values_of(&headers, "content-type"), ["text/html"]);
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

        let website = configured_website("https://example.com/", &seed, empty_depths());

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

    fn seed_with_a_session(seed_url: &str) -> Seed {
        let mut seed = Seed::new(seed_url);
        seed.session_cookie = Some(SessionCookie::bound_to(
            seed_url,
            "substack.sid=secret".to_owned(),
        ));
        seed
    }

    /// What the cookie is configured on, since the header the engine sends is not reachable
    /// from here: the request is made inside the dependency, on a client built from this
    /// configuration, so where the value lands in it is the whole of what a test can say.
    fn configured_header(url: &str, seed: &Seed) -> Option<HeaderValue> {
        configured_website(url, seed, empty_depths())
            .configuration
            .headers
            .as_ref()
            .and_then(|headers| headers.inner().get(COOKIE).cloned())
    }

    fn configured_cookie(url: &str, seed: &Seed) -> Option<String> {
        configured_header(url, seed).map(|value| {
            value
                .to_str()
                .expect("the value went in as text")
                .to_owned()
        })
    }

    /// The reason the whole flag exists: a page the subscription paid for is asked for with the
    /// subscription attached, so the archive holds the post rather than an appeal to subscribe.
    #[test]
    fn the_session_cookie_reaches_a_request_to_the_host_it_is_bound_to() {
        let seed = seed_with_a_session("https://parknotes.substack.com/archive");

        assert_eq!(
            configured_cookie("https://parknotes.substack.com/p/a-paid-post", &seed).as_deref(),
            Some("substack.sid=secret")
        );
    }

    /// The value is marked sensitive on the way in, so the guarantee that a credential is never
    /// printed stops depending on nothing in this process ever formatting a header map. It is
    /// the configuration the dependency holds for the whole crawl, which is exactly the thing a
    /// panic message or a future log line would be most likely to print.
    #[test]
    fn the_configured_session_header_does_not_print_its_value() {
        let seed = seed_with_a_session("https://parknotes.substack.com/archive");
        let header = configured_header("https://parknotes.substack.com/p/a-paid-post", &seed)
            .expect("the cookie is configured");

        assert!(
            !format!("{header:?}").contains("secret"),
            "the configured header printed the credential"
        );
    }

    /// The same client builder, aimed elsewhere. Every picture on these pages lives on a content
    /// network, so this is the ordinary case rather than the exotic one.
    #[test]
    fn a_request_to_any_other_host_is_configured_without_the_cookie() {
        let seed = seed_with_a_session("https://parknotes.substack.com/archive");

        for elsewhere in [
            "https://parkersfiction.substack.com/p/a-story",
            "https://substackcdn.com/image/fetch/w_1456/a.jpeg",
        ] {
            assert_eq!(
                configured_cookie(elsewhere, &seed),
                None,
                "{elsewhere} would have been asked with the session attached"
            );
        }
    }

    /// A run that was given no session sends no `Cookie` at all, rather than an empty one.
    #[test]
    fn a_run_without_a_session_configures_no_cookie() {
        assert_eq!(
            configured_cookie(
                "https://parknotes.substack.com/archive",
                &Seed::new("https://parknotes.substack.com/archive"),
            ),
            None
        );
    }

    /// A redirect is the one hop the seed guard never sees, and the engine screens it only
    /// under a policy that says to. Which policy is in force is therefore the whole guard
    /// on that side, and losing it looks like nothing at all from here: the crawl still
    /// runs, the pages still arrive, and a hop into the metadata service is followed.
    #[test]
    fn a_redirect_is_screened_and_bounded_rather_than_followed_wherever_it_leads() {
        let website = configured_website(
            "https://example.com/",
            &Seed::new("https://example.com/"),
            empty_depths(),
        );

        assert_eq!(
            website.configuration.redirect_policy,
            RedirectPolicy::Strict,
            "a redirect off the seed's host would be followed and archived under it"
        );
        assert_eq!(website.configuration.redirect_limit, MAX_REDIRECTS);
    }

    fn discovered(urls: &[(&str, usize)]) -> HashMap<String, usize> {
        urls.iter()
            .map(|(url, depth)| ((*url).to_owned(), *depth))
            .collect()
    }

    fn fetched(urls: &[&str]) -> HashSet<String> {
        urls.iter().map(|url| (*url).to_owned()).collect()
    }

    /// What every test but the one on `robots_allows` itself is asking about: whether the
    /// frontier came back for a link `robots.txt` never had an opinion on.
    fn allow_all(_url: &str) -> bool {
        true
    }

    /// The ordinary case: every link the guard judged in scope was handed back to the
    /// caller, so a crawl that says it exhausted the frontier is telling the truth.
    #[test]
    fn nothing_is_reported_when_every_discovered_link_was_fetched() {
        let discovered = discovered(&[("https://example.com/", 0), ("https://example.com/a", 1)]);
        let fetched = fetched(&["https://example.com/", "https://example.com/a"]);

        assert!(
            links_discovered_but_never_fetched(
                &discovered,
                &fetched,
                "https://example.com/",
                10,
                allow_all
            )
            .is_empty()
        );
    }

    /// The shape this exists for: a page fetched, a link found on it, and the frontier
    /// never coming back for it while the crawl still reports nothing left to do.
    #[test]
    fn a_link_the_frontier_never_came_back_for_is_reported() {
        let discovered = discovered(&[
            ("https://example.com/", 0),
            ("https://example.com/a", 1),
            ("https://example.com/b", 1),
        ]);
        let fetched = fetched(&["https://example.com/", "https://example.com/a"]);

        assert_eq!(
            links_discovered_but_never_fetched(
                &discovered,
                &fetched,
                "https://example.com/",
                10,
                allow_all
            ),
            vec!["https://example.com/b".to_owned()]
        );
    }

    /// The failure this whole detour exists to close: a link `robots.txt` itself refuses is
    /// not the engine's frontier dropping anything, and reporting it as a loss would make
    /// every site with a disallowed path linked from its own pages fail every capture of it.
    #[test]
    fn a_link_robots_txt_refuses_is_not_reported_as_a_loss() {
        let discovered = discovered(&[
            ("https://example.com/", 0),
            ("https://example.com/allowed", 1),
            ("https://example.com/private", 1),
        ]);
        let fetched = fetched(&["https://example.com/", "https://example.com/allowed"]);

        assert!(
            links_discovered_but_never_fetched(
                &discovered,
                &fetched,
                "https://example.com/",
                10,
                |url| url != "https://example.com/private"
            )
            .is_empty(),
            "the one link left over is exactly the one robots.txt disallows"
        );
    }

    /// `robots_allows` narrows what is reported, it does not blank the report out: a link
    /// the rule has no opinion on and the frontier still never fetched is still a loss.
    #[test]
    fn a_link_robots_txt_has_no_opinion_on_is_still_reported() {
        let discovered = discovered(&[
            ("https://example.com/", 0),
            ("https://example.com/allowed", 1),
            ("https://example.com/private", 1),
        ]);
        let fetched = fetched(&["https://example.com/"]);

        assert_eq!(
            links_discovered_but_never_fetched(
                &discovered,
                &fetched,
                "https://example.com/",
                10,
                |url| url != "https://example.com/private"
            ),
            vec!["https://example.com/allowed".to_owned()]
        );
    }

    /// The seed is always in `discovered`, at depth zero, and it was fetched first rather
    /// than through a link: reporting it back would say the crawl lost the one page that
    /// was never a link to begin with.
    #[test]
    fn the_seed_itself_is_never_reported_as_a_missing_link() {
        let discovered = discovered(&[("https://example.com/", 0)]);
        let fetched = fetched(&[]);

        assert!(
            links_discovered_but_never_fetched(
                &discovered,
                &fetched,
                "https://example.com/",
                10,
                allow_all
            )
            .is_empty()
        );
    }

    /// A link left over when the page count ran out is the budget working as asked, not
    /// the engine dropping something, and reporting it would turn every capped crawl of a
    /// larger site into a false alarm.
    #[test]
    fn a_link_left_over_at_the_page_limit_is_not_reported() {
        let discovered = discovered(&[
            ("https://example.com/", 0),
            ("https://example.com/a", 1),
            ("https://example.com/b", 1),
        ]);
        let fetched = fetched(&["https://example.com/", "https://example.com/a"]);

        assert!(
            links_discovered_but_never_fetched(
                &discovered,
                &fetched,
                "https://example.com/",
                2,
                allow_all
            )
            .is_empty(),
            "two pages were fetched against a limit of two"
        );
    }

    /// The gate `crawl_seed` gives the map above before trusting `Exhausted` at all. A page
    /// counted into `pages_dropped` is a reason of its own for holding less than the crawl
    /// discovered, already stated in that count, and running the comparison anyway would
    /// report every link left in the frontier behind the lost page as if this guard had
    /// caught it.
    #[test]
    fn a_dropped_page_makes_the_frontier_claim_untrustworthy() {
        assert!(!frontier_claim_is_trustworthy(CrawlStop::Exhausted, 1));
        assert!(frontier_claim_is_trustworthy(CrawlStop::Exhausted, 0));
        assert!(!frontier_claim_is_trustworthy(
            CrawlStop::DeadlineReached,
            0
        ));
        assert!(!frontier_claim_is_trustworthy(CrawlStop::CallerStopped, 0));
    }

    /// The scheme every key in the depth map is forced to, mirroring what `push_link` does
    /// to a link before it reaches the frontier: a page carrying an absolute self link in the
    /// other scheme is queued under the seed's, not the page's own.
    #[test]
    fn depth_key_forces_the_seeds_scheme_onto_every_url() {
        assert_eq!(
            depth_key("http://example.com/legacy", "https"),
            "https://example.com/legacy"
        );
        assert_eq!(
            depth_key("https://example.com/a#section", "https"),
            "https://example.com/a"
        );
    }

    /// What decides whether the fix above is needed at all: a seed's own scheme, read once
    /// and independent of any link found on any page.
    #[test]
    fn frontier_scheme_is_read_from_the_seed_and_nothing_else() {
        assert_eq!(frontier_scheme("https://example.com/"), "https");
        assert_eq!(frontier_scheme("http://example.com/"), "http");
    }

    fn page_with_html(html: &str) -> Page {
        let mut page = Page::default();
        page.set_html_bytes(Some(html.as_bytes().to_vec()));
        page
    }

    /// The condition that makes the engine's own base-href handler fire, matched exactly:
    /// a value that parses as an absolute URL.
    #[test]
    fn an_absolute_base_href_is_detected() {
        let page = page_with_html(
            r#"<html><head><base href="https://example.com/"></head><body></body></html>"#,
        );
        assert!(page_declares_an_absolute_base_href(&page));
    }

    /// A relative value never parses as absolute, so it never fires the engine's handler
    /// either: both sides read it the same way, and this has nothing to correct for.
    #[test]
    fn a_relative_base_href_is_not_mistaken_for_an_absolute_one() {
        let page = page_with_html(r#"<html><head><base href="/"></head><body></body></html>"#);
        assert!(!page_declares_an_absolute_base_href(&page));
    }

    #[test]
    fn a_page_with_no_base_element_at_all_is_not_flagged() {
        let page = page_with_html("<html><head></head><body><a href=\"/a\">a</a></body></html>");
        assert!(!page_declares_an_absolute_base_href(&page));
    }

    /// What makes combining groups reachable rather than theoretical: the parse this project
    /// is handed keeps two groups naming the same crawler apart, so a matcher reading only
    /// the first archives every path the second refuses. Driven through the engine's own
    /// parser and the same mapping `robots_rules` uses, which is everything between the file
    /// and the answer except the request that fetched it.
    #[test]
    fn two_groups_naming_this_crawler_are_parsed_apart_and_both_govern() {
        let mut parser = RobotFileParser::new();
        parser.parse_str(
            "User-agent: archeion\nDisallow: /first\n\n\
             User-agent: *\nDisallow: /general\n\n\
             User-agent: Archeion\nDisallow: /second\n",
        );
        assert_eq!(
            parser.get_entries().len(),
            2,
            "the parser stopped keeping repeated groups for one agent apart"
        );

        let mut raw_lines = std::iter::empty();
        let groups = parser
            .get_entries()
            .iter()
            .chain(std::iter::once(parser.get_base_entry()))
            .map(|entry| group_of(entry, &mut raw_lines))
            .collect();
        let rules = RobotRules::for_agent(groups, DEFAULT_USER_AGENT);
        assert!(!rules.allows("https://example.test/first"));
        assert!(!rules.allows("https://example.test/second"));
        assert!(rules.allows("https://example.test/general"));
    }

    /// `configure_for_seed` and `robots_rules` both read `Seed::user_agent`, so a seed's own
    /// choice reaches the HTTP client and the robots matcher together rather than only one of
    /// them, and a seed carrying none of its own sends the same compiled default to both.
    ///
    /// The file names the product token a real `robots.txt` would, `x` rather than `x/1.0`:
    /// RFC 9309 matches on that token alone, so a group written against the full string this
    /// seed sends would never match either identity and the two paths below would come out
    /// identical regardless of which one governed.
    #[test]
    fn a_seeds_own_user_agent_reaches_both_the_client_and_the_robots_matcher() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let addr = listener.local_addr().expect("the bound address");
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = answer_robots_txt(
                    stream,
                    "User-agent: x\nDisallow: /named-group-only\n\n\
                     User-agent: *\nDisallow: /general-group-only\n",
                );
            }
        });
        let start = format!("http://{addr}/");

        let runtime = Runtime::new().expect("a runtime for the test");
        runtime.block_on(async {
            let mut seed = Seed::new(&start);
            seed.user_agent = Some("x/1.0".to_owned());
            let mut website = Website::new(&start);
            configure_for_seed(&mut website, &start, &seed, empty_depths());
            assert_eq!(
                website
                    .configuration
                    .user_agent
                    .as_deref()
                    .map(AsRef::as_ref),
                Some("x/1.0"),
                "the seed's own identity did not reach the HTTP client"
            );

            let mut raw_lines_cache = None;
            let rules =
                robots_rules(&mut website, &mut raw_lines_cache, user_agent_of(&seed)).await;
            assert!(
                !rules.allows(&format!("{start}named-group-only")),
                "the named group governing this identity was not applied"
            );
            assert!(
                rules.allows(&format!("{start}general-group-only")),
                "the * group governed instead of the named group the seed asked for"
            );
        });

        let seed = Seed::new(&start);
        let mut website = Website::new(&start);
        configure_for_seed(&mut website, &start, &seed, empty_depths());
        assert_eq!(
            website
                .configuration
                .user_agent
                .as_deref()
                .map(AsRef::as_ref),
            Some(DEFAULT_USER_AGENT),
            "omitting the flag did not send the compiled default byte for byte"
        );
    }

    /// Answers every request on this connection with a fixed `robots.txt` body, which is all
    /// a test of the parser reuse below needs from a server: what matters is how many times
    /// it is asked, never what else it is asked for.
    fn answer_robots_txt(mut stream: std::net::TcpStream, body: &str) -> std::io::Result<()> {
        use std::io::{BufRead, BufReader, Write};

        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        line.clear();
        while reader.read_line(&mut line)? > 2 {
            line.clear();
        }
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes())?;
        stream.write_all(body.as_bytes())?;
        stream.flush()
    }

    /// The regression the first attempt at this reuse left behind. `configure_robots_parser`
    /// reads `Crawl-delay` into the website's own delay only the first time it runs for a
    /// given parser (it guards on the parser's `mtime`), so a `Website` carrying an already
    /// read parser skips that assignment on every later call, exactly the call
    /// `configure_for_seed` makes before it, resetting the delay to the seed's own, on every
    /// sub-crawl `SpiderEngine::crawl` hands the cached `Website` back for.
    #[test]
    fn a_reused_sub_crawl_still_honours_the_site_s_crawl_delay() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let addr = listener.local_addr().expect("the bound address");
        let requests = Arc::new(Mutex::new(0usize));
        let served = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                *served
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) += 1;
                let _ = answer_robots_txt(stream, "User-agent: *\nAllow: /\nCrawl-delay: 3\n");
            }
        });
        let start = format!("http://{addr}/");

        let runtime = Runtime::new().expect("a runtime for the test");
        runtime.block_on(async {
            let mut seed = Seed::new(&start);
            seed.delay = Duration::from_millis(0);
            let mut website = Website::new(&start);
            let mut raw_lines_cache = None;

            configure_for_seed(&mut website, &start, &seed, empty_depths());
            robots_rules(&mut website, &mut raw_lines_cache, user_agent_of(&seed)).await;
            assert_eq!(
                website.configuration.delay, 3000,
                "the site's own crawl delay was not read on the first, fetching call"
            );

            // The second seed on this origin, exactly as `SpiderEngine::crawl` hands the
            // cached `Website` back for it: the same reset-then-reread sequence, on a parser
            // that is already read and therefore skips the fetch this time.
            configure_for_seed(&mut website, &start, &seed, empty_depths());
            robots_rules(&mut website, &mut raw_lines_cache, user_agent_of(&seed)).await;
            assert_eq!(
                website.configuration.delay, 3000,
                "a reused sub-crawl fell back to the seed's own delay instead of the site's"
            );
        });

        assert_eq!(
            *requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            1,
            "the parser reuse this regression depends on was not exercised"
        );
    }

    fn answer_two_trivial_pages(mut stream: std::net::TcpStream) -> std::io::Result<()> {
        use std::io::{BufRead, BufReader, Write};

        let mut reader = BufReader::new(stream.try_clone()?);
        let mut request_line = String::new();
        reader.read_line(&mut request_line)?;
        let mut header = String::new();
        while reader.read_line(&mut header)? > 2 {
            header.clear();
        }
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_owned();
        let (status, body): (&str, &str) = match path.as_str() {
            "/robots.txt" => ("404 Not Found", ""),
            _ => ("200 OK", "<html><body>a page</body></html>"),
        };
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes())?;
        stream.write_all(body.as_bytes())?;
        stream.flush()
    }

    /// `outcome.stopped` is `crawl_seed`'s to keep honest, not `recover_lost_links`'s alone:
    /// a caller that stops on a page recovery just handed it is a real `CallerStopped`, the
    /// same answer the crawl phase itself gives when its own caller does the same thing, and
    /// the fix is `crawl_seed` reading this return value rather than assuming the crawl
    /// phase's own `Exhausted` still holds once recovery has run.
    ///
    /// Two candidates, `/a` and `/b`, both ordinary pages: `on_page` breaks on the very
    /// first one handed to it, which is `/a` since `links_discovered_but_never_fetched`
    /// already sorts its candidates. `/a` still counts as recovered, since the break comes
    /// after it was already fetched and delivered; `/b` was never attempted at all, and is
    /// exactly as unfetched as it already was.
    #[test]
    fn a_caller_that_stops_mid_recovery_is_reported_as_the_reason_recovery_stopped() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let addr = listener.local_addr().expect("the bound address");
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = answer_two_trivial_pages(stream);
            }
        });
        let seed = Seed::new(format!("http://{addr}/"));
        let website = Website::new(&seed.url);
        let selectors = website.setup_selectors();
        let robots = RobotRules::everything_allowed();
        let depths = empty_depths();

        let candidates = vec![
            (format!("http://{addr}/a"), 1),
            (format!("http://{addr}/b"), 1),
        ];
        let seed_host = addr.ip().to_string();
        let (still_missing, links_recovered, stop) = recover_lost_links(
            candidates,
            &seed,
            0,
            Instant::now(),
            &selectors,
            Some(seed_host.as_str()),
            "http",
            Duration::ZERO,
            &depths,
            &robots,
            &mut |_event| ControlFlow::Break(()),
        );

        assert_eq!(stop, RecoveryStop::CallerStopped);
        assert_eq!(
            links_recovered, 1,
            "the page fetched before the break was not counted"
        );
        assert_eq!(
            still_missing,
            vec![format!("http://{addr}/b")],
            "the untouched candidate was not left exactly as unfetched as it already was"
        );
    }
}
