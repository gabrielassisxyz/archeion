//! The line between the archival core and whatever crawls the web for it.
//!
//! Everything above this file depends on the types here and never on an engine. All an
//! engine may say is `PageEvent`, all it is asked for is a `Seed`, and both are written in
//! the archive's terms rather than the engine's, so swapping the engine is a new adapter
//! and not a rewrite of the code that stores what it produced.

use std::collections::HashMap;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::ops::ControlFlow;
use std::time::{Duration, Instant, SystemTime};

use jiff::Timestamp;
use url::{Host, Origin, Url};

use crate::storage::Header;

/// A subscription the run was given, so that a page a reader has paid for is archived as the
/// page rather than as an invitation to subscribe.
///
/// It holds the value of a `Cookie` header taken from an authenticated request, and the origin
/// that credential belongs to. **It is sent to that origin and to nothing else.** A run follows
/// redirects and acquires subresources from wherever a page names them, so a cookie attached to
/// every request the run happens to make is a credential handed to whatever host a page points
/// at, which is the failure here that costs the most and shows the least.
///
/// The origin is captured when this is built rather than read from `Seed::url` when a request
/// is made, because the sitemap phase clones the seed once per listed URL and replaces that
/// field: a binding derived on the fly would follow the clone onto whatever address the listing
/// named. What the sitemap phase can hand over is narrower than it looks, since `read_sitemap`
/// already refuses a listed URL whose host is not the seed's, so what remains is a listed URL
/// sharing the host under another scheme or port. The requests that genuinely aim elsewhere are
/// a subresource on a content network and a redirect the run followed off the host.
///
/// An origin and not a hostname, which is what "its host" means once a scheme and a port are in
/// play: the same name on another scheme is another site's session as far as this is concerned,
/// and the comparison costs nothing.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionCookie {
    /// Where this credential may be sent. An address that does not parse gets an opaque origin,
    /// which equals nothing, so the cookie is sent nowhere rather than everywhere.
    origin: Origin,
    /// The whole `Cookie` header value, as an authenticated request sent it.
    value: String,
}

impl SessionCookie {
    /// Binds a `Cookie` header value to the origin of the URL the run was given.
    pub fn bound_to(url: &str, value: String) -> Self {
        let origin = Url::parse(url)
            .map(|parsed| parsed.origin())
            .unwrap_or_else(|_unreadable| Origin::new_opaque());
        Self { origin, value }
    }

    /// Where this credential may be sent, for a report that has to name it. The origin and never
    /// the value: an opaque origin serializes as `null`, which is the honest answer for a seed
    /// address nothing could read.
    pub fn origin(&self) -> String {
        self.origin.ascii_serialization()
    }

    /// The header value to send with a request aimed at this URL, and `None` for every other
    /// address.
    ///
    /// How often it is asked is the caller's business and it differs between the two: a single
    /// fetch builds a client per address, so the question is asked per request, while a crawl
    /// builds one client for the whole traversal and the answer rides every page it fetches.
    /// That is sound because a crawl is one host by construction. What is unmeasured is whether
    /// the engine's frontier will queue a same-host link on another port, which is the one shape
    /// that would make a crawl's single answer too generous; it is the dependency's rule and
    /// nobody here has driven it.
    pub(crate) fn value_for(&self, url: &str) -> Option<&str> {
        let parsed = Url::parse(url).ok()?;
        (parsed.origin() == self.origin).then_some(self.value.as_str())
    }
}

/// Prints where the credential goes and never what it is. `Seed` derives `Debug`, so anything
/// that ever writes a seed into a message, a log line or an error would otherwise publish a
/// session token.
impl fmt::Debug for SessionCookie {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("SessionCookie")
            .field("origin", &self.origin.ascii_serialization())
            .field("value", &"(not shown)")
            .finish()
    }
}

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
    /// Whether the seed may name an address that exists only inside a network: loopback, a
    /// private range, link-local, or one of the names a cloud metadata service answers on.
    /// It is off, so a URL cannot talk the archive into reading the machine it runs on or
    /// the network around it.
    ///
    /// Turning it on is how a locally served site is archived at all, and it is also the
    /// only way the fetch path itself is ever exercised, since every check of it points at
    /// a server on localhost.
    pub allow_private_addresses: bool,
    /// A subscription the run was given, sent only to the origin it is bound to. Absent by
    /// default, which is a run that archives what an anonymous reader is served.
    pub session_cookie: Option<SessionCookie>,
    /// The identity this run announces to servers and matches `robots.txt` groups against.
    /// `None` keeps the engine's own compiled default, read fresh by whichever adapter runs
    /// the seed rather than copied in here: a boundary type has no default of its own to
    /// give, since the string is a fact about one adapter and this crosses to any of them.
    ///
    /// Every request the seed causes, a crawl's, a single fetch's and a subresource's alike,
    /// is decided by this same field, and so is the robots group the run is judged against:
    /// an override that reached the client but not the matcher would obey rules nobody wrote
    /// for the identity actually asking.
    pub user_agent: Option<String>,
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
            // This number multiplies the one above: a URL that keeps timing out is paid for
            // once per attempt, so the ceiling for a single dead URL is ninety seconds and
            // the backoff between them, not thirty. Two is what keeps that under a third of
            // the deadline while still giving a 429 somewhere to land.
            max_retries: 2,
            // A seed arrives from outside and the ranges below are the ones an outside URL
            // has no business naming, so the default is the safe half of the choice and
            // reaching a local server is the part that has to be asked for.
            allow_private_addresses: false,
            // Carrying a credential is never the default. A run says so, out loud, and says
            // where the credential came from.
            session_cookie: None,
            // The engine's own compiled default, until a caller asks for another identity.
            user_agent: None,
        }
    }
}

/// Whether a URL names an address that exists only inside a network.
///
/// It answers for both ends of a fetch, which is why it lives on the boundary rather than
/// in an adapter: a seed is refused before anything is dialled, and a page that ended on
/// one of these addresses is refused before it is stored. The second half is not the first
/// one repeated. A redirect is screened inside the engine, but the archive still keeps
/// this predicate at the storage boundary because the engine is replaceable and a stored
/// response is the durable part of the harm.
///
/// A URL that does not parse, or that names no host, is not an address this can judge.
/// Both answer false and are refused further along for what they actually are.
pub(crate) fn points_inside_a_network(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    parsed.host().is_some_and(|host| is_internal_host(&host))
}

/// Whether a host exists only inside a network. It is the archive's half of the guard the
/// engine applies to every redirect hop, kept here so the boundary owns the archive policy
/// rather than inheriting it from one adapter.
///
/// Neither half resolves the name. A domain answering with a private address passes both,
/// and closing that gap means resolving before the connect and pinning the answer at connect
/// time, since a name is free to answer differently the second time it is asked. That is a
/// resolving connector rather than a string check, and it does not exist here yet.
pub(super) fn is_internal_host(host: &Host<&str>) -> bool {
    match host {
        Host::Domain(name) => is_internal_name(name),
        Host::Ipv4(address) => is_internal_ipv4(*address),
        Host::Ipv6(address) => is_internal_ipv6(*address),
    }
}

fn is_internal_name(name: &str) -> bool {
    // A trailing dot is the same name spelled as a fully qualified one, and a guard that
    // matched on the string alone would be walked past by typing it.
    let name = name.trim_end_matches('.');
    name == "localhost"
        || name.ends_with(".localhost")
        // The cloud metadata services answer on these as well as on 169.254.169.254, and
        // that address is the credential store of whatever machine the archive runs on.
        || name == "metadata.google.internal"
        || name == "metadata.goog"
}

fn is_internal_ipv4(address: Ipv4Addr) -> bool {
    // Link-local covers 169.254.169.254, so the metadata address needs no line of its own.
    address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_unspecified()
        || address.is_broadcast()
        // 0.0.0.0/8 is "this network" in RFC 1122, and the whole block is a way of naming
        // the local host: is_unspecified() only recognises 0.0.0.0 itself, so without this
        // the other sixteen million addresses in the range walk past the guard.
        || address.octets()[0] == 0
}

fn is_internal_ipv6(address: Ipv6Addr) -> bool {
    address.is_loopback()
        || address.is_unspecified()
        // fc00::/7 is the private range, and one of the cloud metadata services answers
        // inside it, on fd00:ec2::254. That is the same credential store 169.254.169.254
        // is, reached by its other address.
        || address.is_unique_local()
        // fe80::/10, which is what 169.254.0.0/16 is on the other side.
        || address.is_unicast_link_local()
        // An address written as ::ffff:127.0.0.1 reaches the same machine as 127.0.0.1 does.
        || address.to_ipv4_mapped().is_some_and(is_internal_ipv4)
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
    /// The seed's page count was reached and the run stopped asking for more. It is a
    /// separate answer from the one above because the two send an operator somewhere
    /// different: a ceiling reached says a larger number takes the rest of the site, and a
    /// clock that ran out says the run needs longer or the host needs asking more slowly.
    PageCeilingReached,
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
    /// Whether the body above is less than the response promised. A transfer can end early
    /// for reasons that have nothing to do with the server changing its mind: a stream that
    /// errored, one that went idle, one that ran past a size limit. Archiving what arrived
    /// under a status that promises the whole page, with nothing saying which of the two it
    /// is, makes the archive quietly wrong instead of visibly short.
    pub body_truncated: bool,
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
    /// Links the crawl found in scope, through a page it fetched, and never fetched
    /// themselves even though the run reports there was nothing left to do. Nothing was
    /// spent on these, unlike the pages above: the loss is that a page reachable from the
    /// seed is simply missing, with no failed fetch and no warning to say so.
    ///
    /// What survives here is only a link recovery genuinely could not reach: refused past
    /// its own retry budget, or still queued when the seed's own page count or deadline
    /// ran out. A link recovery did reach is counted in `links_recovered` instead, never
    /// listed twice.
    pub links_never_followed: Vec<String>,
    /// Links the frontier discovered and never asked the site for, fetched directly instead
    /// once the crawl claimed to be done, because they were in scope and this project's own
    /// robots decision would have let them through. Counted rather than listed: once one is
    /// archived it is an ordinary capture like any other, and `list` is what names it.
    pub links_recovered: usize,
    /// Pages a host answered 429 that were archived anyway, once waiting for it stopped being
    /// refused. Counted rather than listed, on the same reasoning as `links_recovered` above:
    /// once one is archived it is an ordinary capture like any other, and it is not owed.
    pub pages_recovered_from_rate_limit: usize,
    pub stopped: CrawlStop,
}

/// The smallest a wait for a rate-limited host is ever asked to be, and what it doubles from
/// on each successive 429 that host has answered. A whole second rather than something
/// sub-second: a host answering 429 at all is already saying the current pace is too fast, and
/// asking again inside the same second is the pace that produced the refusal.
const RATE_LIMIT_BASE_BACKOFF: Duration = Duration::from_secs(1);

/// The most a single wait may grow to before the next attempt asks again, independent of the
/// seed's own deadline. Doubling has no ceiling of its own, and a host that keeps refusing
/// would otherwise grow one sitting's wait past anything worth sleeping through at once.
///
/// It is not this number that decides whether the run gives up on the address. The two
/// checks beside every wait are, and the tighter of them is ordinarily the share rather
/// than the deadline: a wait has to fit both what is left of the run and
/// `RATE_LIMIT_SHARE_OF_THE_BUDGET` of the whole of it. At the default three hundred second
/// deadline that puts the largest wait ever taken at seventy five seconds, so a
/// `Retry-After` asking for more is recorded as owed at once however much of the run is
/// still unspent.
const RATE_LIMIT_MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// What fraction of the run's whole budget one refusing address may spend being waited out,
/// as the divisor: a quarter.
///
/// The deadline alone bounds the run and not the address inside it, and those are different
/// bounds. Every caller here sleeps on the thread that is also draining the crawl, so the
/// wait is not one address pausing while the rest of the run continues, it is the run
/// stopping. Bounded only by the deadline, a single path that answers 429 forever, a search
/// endpoint or an address behind a firewall being the shapes seen, sleeps through one second,
/// then two, and at the default five minute budget has spent two hundred and fifty five of
/// them before the next wait stops fitting. The archive that comes back is nearly empty and
/// the report blames the deadline, over one address that the run before this policy existed
/// would have recorded as owed and walked past.
///
/// A quarter is a share rather than a number of seconds because the question it answers is
/// how much of this run one address is worth, which is a different answer for a five minute
/// run and an all night one. Past it the address is recorded as owed exactly as it was before
/// any of this, and the run keeps three quarters of what it was given for everything else.
const RATE_LIMIT_SHARE_OF_THE_BUDGET: u32 = 4;

/// How long to wait before asking a rate-limited address again.
///
/// Doubles with every successive 429 the same host has already answered in this run, since a
/// wait that stays flat is a wait tuned for the first refusal and never adjusted for a host
/// still refusing after it. Never shorter than `floor`, the pace this run is already honouring
/// on every other request it makes: a backoff under that would be a faster request to the
/// exact host that just said the pace was too fast, which is not politeness with a different
/// name. A `Retry-After` the host sent wins whenever it asks for longer than either.
fn rate_limit_backoff(
    consecutive_refusals: u32,
    floor: Duration,
    retry_after: Option<Duration>,
) -> Duration {
    // Ten doublings already exceeds `RATE_LIMIT_MAX_BACKOFF`, so capping the exponent here is
    // what keeps the multiplication below from ever needing to be checked.
    let doublings = consecutive_refusals.min(10);
    let grown = (RATE_LIMIT_BASE_BACKOFF * 2u32.pow(doublings)).min(RATE_LIMIT_MAX_BACKOFF);
    let floor = grown.max(floor);
    retry_after.map_or(floor, |asked| floor.max(asked))
}

/// The wait a `Retry-After` header asked for, read as either form the header may take: a count
/// of seconds, or an HTTP-date naming when to come back. `None` covers a header that is
/// absent, unparseable, or already in the past, since a header naming a moment already gone
/// asks for nothing more than what backoff already provides on its own.
fn retry_after_wait(headers: &[Header]) -> Option<Duration> {
    let value = headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("retry-after"))
        .map(|header| header.value.trim())?;
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let target = httpdate::parse_http_date(value).ok()?;
    target.duration_since(SystemTime::now()).ok()
}

/// What a backoff decision is tracked against, read off the address itself rather than off
/// whatever the caller already knows about it: an address this fails to parse still gets its
/// own entry, keyed on its own spelling, so backoff degrades to per-address rather than
/// disappearing.
///
/// The origin rather than the host alone, because a rate limit belongs to the server that
/// imposed it and a scheme, a host and a port together are what name one server. It is also
/// the unit a resume already works in: `capture_owed` splits the debt it is paying down into
/// origin groups and hands the engine one group at a time, so a decision taken about one
/// group is now a decision about exactly what that group is.
fn rate_limit_key(url: &str) -> String {
    Url::parse(url)
        .map(|parsed| parsed.origin().ascii_serialization())
        .unwrap_or_else(|_| url.to_owned())
}

/// What a run has learned about the servers that answered it 429: how many times in a row
/// each one has refused, and which ones it has stopped waiting for altogether.
///
/// It belongs to the run rather than to one crawl, which is the whole of what makes the
/// second decision worth recording. A resume hands every owed address to the engine as a
/// sub-crawl of its own, and a memory that began again with each of them would answer a
/// server's twentieth refusal at the pace its first one earned, and would pay the share of
/// the budget one address may spend being waited out twenty times over. A hundred addresses
/// owed on a server that is still refusing would then leave nothing at all for the server
/// listed behind them, which is the case this exists to prevent.
#[derive(Debug, Default)]
pub(crate) struct RateLimitMemory {
    consecutive_refusals: HashMap<String, u32>,
    /// Servers this run has already spent an address's whole share of the budget on without
    /// getting an answer. Nothing clears an entry: within one run, a server that would not
    /// be waited out once is not worth waiting out again, and the addresses behind it are
    /// recorded as owed at the speed of a single request each rather than of a wait each.
    given_up_on: std::collections::HashSet<String>,
}

impl RateLimitMemory {
    fn consecutive_refusals(&self, key: &str) -> u32 {
        self.consecutive_refusals.get(key).copied().unwrap_or(0)
    }

    fn refused_again(&mut self, key: &str, consecutive: u32) {
        self.consecutive_refusals
            .insert(key.to_owned(), consecutive);
    }

    fn answered(&mut self, key: &str) {
        self.consecutive_refusals.insert(key.to_owned(), 0);
    }

    fn give_up_on(&mut self, key: &str) {
        self.given_up_on.insert(key.to_owned());
    }

    fn given_up_on(&self, key: &str) -> bool {
        self.given_up_on.contains(key)
    }
}

/// Waits out a 429 and asks for the same address again, growing the wait across successive
/// refusals from the same host until the host answers something other than 429 or waiting
/// again would spend the run's own deadline rather than a fetch. Every other status a caller
/// hands this is returned untouched: only a 429 says the address is fine and the pace is the
/// problem, and every other retryable status already had its say, inside the engine or inside
/// recovery, before either one ever calls this.
///
/// `fetch_again` is how this crosses the same line every fetch here does: a caller supplies
/// it rather than this function reaching for an engine of its own, which is what lets both a
/// live crawl, where a retry has to bypass the frontier that already gave up on the address,
/// and a plain fetch, where there was never a frontier to bypass, share the one policy.
///
/// A page recovered this way is filed as the response it is, and whatever else its own links
/// have to become is the caller's: `fetch_again` is the one place a recovered body is ever
/// seen, so a caller that has to fold the page's links into a crawl's own bookkeeping does
/// it from inside the fetch it supplies rather than from here.
///
/// The deadline check happens before every sleep rather than only once at the top, and
/// compares the wait about to be taken rather than only the clock already spent: a caller
/// blocking this loop synchronously, as every caller here does, can starve whatever else
/// would otherwise have noticed the deadline expire, so this has to notice it on its own
/// before a wait large enough to matter is ever taken, not only after one already was.
///
/// A seed with no deadline never waits here at all, and this is the one case a caller cannot
/// override by choosing a shorter floor: nothing bounds how long a host may keep refusing,
/// and "bounded by the deadline" has nothing to be bounded by when there is none. A run asked
/// to run forever keeps doing so, on whatever other addresses are left, rather than this
/// function choosing a ceiling of its own for a run that deliberately chose to have none.
///
/// Giving up on an address because a further wait would not fit the remaining budget ends
/// this call and nothing else. The refusal is handed back exactly as it arrived, so the
/// caller records the address as owed the way it records any other refusal, and the run
/// carries on with whatever else it had to ask for. It is not a deadline that ended the
/// run: the budget is still open, and a run that goes on to finish its remaining addresses
/// inside it finished inside it. Blaming the deadline here would name a bound that decided
/// one address rather than the run, and on the sitemap and resume paths it would also cost
/// every address still unasked behind it.
pub(crate) fn wait_out_rate_limit(
    event: PageEvent,
    deadline: Option<Duration>,
    started: Instant,
    floor: Duration,
    state: &mut RateLimitMemory,
    recovered: &mut usize,
    mut fetch_again: impl FnMut(&str) -> PageEvent,
) -> PageEvent {
    let PageEvent::Response(first) = &event else {
        return event;
    };
    if first.status != 429 {
        return event;
    }
    let url = first.requested_url.clone();
    let host = rate_limit_key(&url);
    if state.given_up_on(&host) {
        return event;
    }
    let mut event = event;
    // Read apart from `started`, which is the whole run's clock: this one measures what this
    // one address has cost, which is what `RATE_LIMIT_SHARE_OF_THE_BUDGET` bounds.
    let this_address_started = Instant::now();
    let mut asked_again = false;
    while let PageEvent::Response(page) = &event {
        if page.status != 429 {
            break;
        }
        let consecutive = state.consecutive_refusals(&host);
        let wait = rate_limit_backoff(consecutive, floor, retry_after_wait(&page.headers));
        // Saturating rather than plain addition: `Retry-After` is a number a host chose, and
        // a host that answers `Retry-After: 18446744073709551615` would otherwise overflow
        // this sum and panic the run rather than be refused by it.
        let fits = match deadline {
            Some(budget) => {
                started.elapsed().saturating_add(wait) < budget
                    && this_address_started.elapsed().saturating_add(wait)
                        <= budget / RATE_LIMIT_SHARE_OF_THE_BUDGET
            }
            None => false,
        };
        if !fits {
            // A wait this run will not take is a server it has stopped waiting for. What
            // the next address on it would do is take this same decision again, one wait
            // at a time and one share of the budget at a time, and arrive at the same
            // answer; recording it here is what keeps the hundredth address owed at the
            // cost of a request rather than of a whole share. A run with no deadline is
            // not a run that gave up on anything: `fits` is false there because nothing
            // bounds a wait, not because a wait was refused.
            if deadline.is_some() {
                state.give_up_on(&host);
            }
            break;
        }
        std::thread::sleep(wait);
        asked_again = true;
        state.refused_again(&host, consecutive + 1);
        event = fetch_again(&url);
    }
    // The pace this run owes the host applies to whatever it asks for next, and the request
    // after this one is not this function's to make: a crawl's own scheduler has been
    // counting its throttle down while this thread slept, so it dispatches the next page the
    // moment this returns. Paying the floor here is the gap that would otherwise be missing
    // between the last request this made and the first request the caller makes after it,
    // and it is owed only when this actually asked the host for something.
    if asked_again && !floor.is_zero() {
        let fits = deadline.is_some_and(|budget| started.elapsed().saturating_add(floor) < budget);
        if fits {
            std::thread::sleep(floor);
        }
    }
    if matches!(&event, PageEvent::Response(page) if page.status < 400) {
        state.answered(&host);
        *recovered += 1;
    }
    event
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

/// What the archival core asks of a crawl engine: a crawl, and a single fetch.
///
/// The crawl call blocks until the crawl ends, and every page reaches the caller through
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
    /// Whether this engine will dial the seed at all, asked before anything happens.
    ///
    /// A crawl screens its own seed and refuses the same ones, so this answers a question
    /// that would be answered anyway. What it is for is the caller with something to commit
    /// before the crawl starts: an archive brought into existence for a run that was never
    /// going to fetch anything is a directory nobody asked for, sitting on the exact path
    /// that was typed wrong.
    fn check_seed(&self, seed: &Seed) -> Result<(), CrawlError>;

    fn crawl(
        &self,
        seed: &Seed,
        on_page: &mut dyn FnMut(PageEvent) -> ControlFlow<()>,
    ) -> Result<CrawlOutcome, CrawlError>;

    /// Fetches one URL on its own, outside the crawl.
    ///
    /// This is what an asset pass is made of. A subresource a page referenced is not a page
    /// of the crawl: it has no depth, it contributes no links, and it belongs to the capture
    /// that referenced it rather than to a queue. Handing those URLs back to `crawl` would
    /// file each of them as an item of its own, which is the one thing an asset is not.
    ///
    /// What a fetch does share with the crawl around it is the policy, which is why the seed
    /// comes along. The request timeout, the redirect screening and the rule about addresses
    /// that exist only inside a network govern every request a run makes, not only the ones
    /// the engine chose to make. The address in the seed is not where this fetch goes.
    ///
    /// There is one answer and no error, because everything that can go wrong is a fetch
    /// that produced no response, which `PageEvent` already carries a shape for. A URL this
    /// engine refuses to dial at all is reported the same way, since to the caller a
    /// subresource it cannot have is a subresource it cannot have.
    ///
    /// A URL a sitemap lists is fetched the same way when nothing is to be followed out of
    /// it: it has no depth and contributes no links either, which is what it shares with a
    /// subresource. Where it differs is that it does become an item of its own once fetched,
    /// unlike a subresource, so the caller files it as a capture rather than folding it into
    /// the page that referenced it.
    fn fetch(&self, url: &str, seed: &Seed) -> PageEvent;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie_for(seed_url: &str) -> SessionCookie {
        SessionCookie::bound_to(seed_url, "substack.sid=secret".to_owned())
    }

    /// The whole point of carrying one: a page the subscription paid for is asked for with the
    /// subscription attached.
    #[test]
    fn a_cookie_reaches_a_request_to_the_origin_it_is_bound_to() {
        let cookie = cookie_for("https://parknotes.substack.com/archive");

        assert_eq!(
            cookie.value_for("https://parknotes.substack.com/p/a-paid-post"),
            Some("substack.sid=secret")
        );
        // The port a URL leaves out is the scheme's own, so writing it changes nothing.
        assert_eq!(
            cookie.value_for("https://parknotes.substack.com:443/p/a-paid-post"),
            Some("substack.sid=secret")
        );
    }

    /// The ordinary case rather than the exotic one: every picture on these pages lives on a
    /// content network, and a run may follow a redirect off the seed's host. A credential sent
    /// to a host that did not issue it is a credential given away.
    #[test]
    fn a_request_to_any_other_host_carries_no_cookie() {
        let cookie = cookie_for("https://parknotes.substack.com/archive");

        for elsewhere in [
            "https://parkersfiction.substack.com/p/a-story",
            "https://substackcdn.com/image/fetch/w_1456/a.jpeg",
            "https://substack.com/",
        ] {
            assert_eq!(
                cookie.value_for(elsewhere),
                None,
                "{elsewhere} was asked for with the session attached"
            );
        }
    }

    /// The binding is an origin, so the two ways an address can leave it are both refused: a
    /// different host, and the seed's own host under another scheme. It asserts the predicate
    /// and nothing about a redirect: the hop the engine makes inside a chain never reaches this
    /// project, `remove_sensitive_headers` in the HTTP client is what strips `Cookie` there, and
    /// no test here can drive it, because the engine refuses a redirect to a loopback target
    /// before any policy is consulted and a test may not leave the machine.
    #[test]
    fn the_binding_covers_neither_another_host_nor_the_same_host_on_another_scheme() {
        let cookie = cookie_for("https://parknotes.substack.com/archive");

        assert_eq!(cookie.value_for("https://elsewhere.example/landing"), None);
        // The same name under another scheme is another origin, which is the reading of "its
        // host" this binding takes.
        assert_eq!(cookie.value_for("http://parknotes.substack.com/p/a"), None);
    }

    /// A binding is captured when the cookie is built, so a phase that replaces the seed's own
    /// URL per address it works through cannot move the credential to whatever that address
    /// named. The sitemap phase already refuses a listed URL on another host, so the shape it
    /// can still hand over is the seed's own host on another port, which is another origin and
    /// gets nothing either.
    #[test]
    fn a_seed_cloned_for_another_url_keeps_the_binding_it_was_given() {
        let mut seed = Seed::new("https://parknotes.substack.com/archive");
        seed.session_cookie = Some(cookie_for(&seed.url));

        let mut listed = seed.clone();
        listed.url = "https://parknotes.substack.com:8443/p/a-story".to_owned();
        let cookie = listed.session_cookie.expect("the clone keeps the cookie");

        assert_eq!(cookie.value_for(&listed.url), None);
        assert_eq!(
            cookie.value_for("https://parknotes.substack.com/p/a-paid-post"),
            Some("substack.sid=secret")
        );
    }

    /// An address this cannot read is an address the credential is not sent to. The opposite
    /// default would send a session wherever a URL happened not to parse.
    #[test]
    fn an_address_that_is_not_a_url_receives_nothing() {
        let cookie = cookie_for("https://parknotes.substack.com/archive");

        for unusable in ["/p/a-paid-post", "parknotes.substack.com", ""] {
            assert_eq!(cookie.value_for(unusable), None, "{unusable} was trusted");
        }
        // A seed that does not parse binds the cookie to nothing rather than to everything.
        assert_eq!(
            cookie_for("not a url").value_for("https://example.com/"),
            None
        );
    }

    /// `Seed` derives `Debug`, so a message, a log line or an error that prints one must not
    /// publish the credential it carries.
    #[test]
    fn a_session_cookie_never_prints_the_credential() {
        let mut seed = Seed::new("https://parknotes.substack.com/archive");
        seed.session_cookie = Some(cookie_for(&seed.url));

        let printed = format!("{seed:?}");

        assert!(!printed.contains("secret"), "the cookie was printed");
        assert!(printed.contains("https://parknotes.substack.com"));
    }

    fn refused(status: u16, headers: Vec<Header>) -> PageEvent {
        refused_at("https://example.com/rate-limited", status, headers)
    }

    fn refused_at(url: &str, status: u16, headers: Vec<Header>) -> PageEvent {
        PageEvent::Response(PageResponse {
            requested_url: url.to_owned(),
            final_url: url.to_owned(),
            status,
            headers,
            body: Vec::new(),
            body_truncated: false,
            fetched_at: Timestamp::UNIX_EPOCH,
        })
    }

    fn ok() -> PageEvent {
        refused(200, Vec::new())
    }

    fn retry_after(value: &str) -> Header {
        Header {
            name: "Retry-After".to_owned(),
            value: value.to_owned(),
        }
    }

    /// The shape the acceptance criterion asks for directly: a wait that is not the same
    /// number every time a host refuses in a row.
    #[test]
    fn rate_limit_backoff_grows_with_each_successive_refusal() {
        let first = rate_limit_backoff(0, Duration::ZERO, None);
        let second = rate_limit_backoff(1, Duration::ZERO, None);
        let third = rate_limit_backoff(2, Duration::ZERO, None);

        assert!(second > first, "{second:?} did not grow past {first:?}");
        assert!(third > second, "{third:?} did not grow past {second:?}");
    }

    /// However small the growth is, it never undercuts the pace the run is already asking
    /// every other request to honour: `floor` stands in for `--delay` or the site's own
    /// `Crawl-delay`, whichever the caller passed as the larger of the two.
    #[test]
    fn rate_limit_backoff_never_undercuts_the_run_s_own_pace() {
        let floor = Duration::from_secs(90);

        assert_eq!(rate_limit_backoff(0, floor, None), floor);
    }

    /// A `Retry-After` longer than what backoff would have chosen on its own wins, in either
    /// direction: this is the growth alone, with the floor already covered above.
    #[test]
    fn rate_limit_backoff_honours_a_longer_retry_after() {
        let asked = Duration::from_secs(120);

        assert_eq!(
            rate_limit_backoff(0, Duration::ZERO, Some(asked)),
            asked,
            "a Retry-After longer than the grown wait did not win"
        );
        assert_eq!(
            rate_limit_backoff(0, Duration::ZERO, Some(Duration::from_millis(1))),
            RATE_LIMIT_BASE_BACKOFF,
            "a Retry-After shorter than the grown wait shortened it"
        );
    }

    /// Growth is capped independently of the deadline, which is what keeps a single sleep
    /// from costing more than a caller can reasonably block a thread for even before the
    /// deadline check ever runs.
    #[test]
    fn rate_limit_backoff_stops_growing_at_its_ceiling() {
        assert_eq!(
            rate_limit_backoff(30, Duration::ZERO, None),
            RATE_LIMIT_MAX_BACKOFF
        );
    }

    #[test]
    fn retry_after_wait_reads_the_seconds_form() {
        let headers = vec![retry_after("120")];

        assert_eq!(retry_after_wait(&headers), Some(Duration::from_secs(120)));
    }

    #[test]
    fn retry_after_wait_reads_the_http_date_form() {
        let target = SystemTime::now() + Duration::from_secs(60);
        let headers = vec![retry_after(&httpdate::fmt_http_date(target))];

        let read = retry_after_wait(&headers).expect("an HTTP-date in the future parses");
        // A lower and an upper bound rather than an exact value: `fmt_http_date` rounds to
        // the second and the parse reads the clock again on the way back, so the two ends of
        // this round trip are never quite the same instant.
        assert!(
            read >= Duration::from_secs(58) && read <= Duration::from_secs(61),
            "{read:?}"
        );
    }

    /// A moment already gone asks for nothing more than backoff already provides, rather
    /// than for a negative wait no `Duration` can hold.
    #[test]
    fn retry_after_wait_ignores_a_date_already_in_the_past() {
        let past = SystemTime::now() - Duration::from_secs(600);
        let headers = vec![retry_after(&httpdate::fmt_http_date(past))];

        assert_eq!(retry_after_wait(&headers), None);
    }

    #[test]
    fn retry_after_wait_is_absent_when_the_header_is_absent() {
        assert_eq!(retry_after_wait(&[]), None);
    }

    /// The one status this waits out at all. Every other status, retryable or not, is
    /// returned exactly as it arrived, and the fetch this was given is never called.
    #[test]
    fn wait_out_rate_limit_leaves_every_other_status_untouched() {
        let mut state = RateLimitMemory::default();
        let mut recovered = 0;
        let mut calls = 0;
        let event = refused(503, Vec::new());

        let result = wait_out_rate_limit(
            event.clone(),
            None,
            Instant::now(),
            Duration::ZERO,
            &mut state,
            &mut recovered,
            |_url| {
                calls += 1;
                ok()
            },
        );

        assert_eq!(result, event);
        assert_eq!(calls, 0, "a 503 was retried by the rate limit backoff");
        assert_eq!(recovered, 0);
    }

    /// The address is asked for again after waiting, and once it answers under 400 the
    /// address is counted as recovered rather than left to be recorded as owed.
    #[test]
    fn wait_out_rate_limit_asks_again_and_counts_a_success() {
        let mut state = RateLimitMemory::default();
        let mut recovered = 0;
        let mut calls = 0;

        let result = wait_out_rate_limit(
            refused(429, Vec::new()),
            Some(Duration::from_secs(10)),
            Instant::now(),
            Duration::ZERO,
            &mut state,
            &mut recovered,
            |_url| {
                calls += 1;
                ok()
            },
        );

        assert!(
            matches!(&result, PageEvent::Response(page) if page.status == 200),
            "{result:?}"
        );
        assert_eq!(calls, 1, "the address was not asked for again");
        assert_eq!(recovered, 1);
        assert_eq!(
            state.consecutive_refusals(&rate_limit_key("https://example.com/slow")),
            0,
            "a server that just answered under 400 was left with an elevated count"
        );
    }

    /// A wait large enough to cross the deadline is never taken: the address is handed back
    /// exactly as refused as it arrived, and the fetch this was given is never called. The
    /// deadline here is far shorter than `RATE_LIMIT_BASE_BACKOFF`, the smallest wait this
    /// could ever choose, so the bound is what stops this rather than the test's own timing.
    #[test]
    fn wait_out_rate_limit_gives_up_before_a_wait_would_cross_the_deadline() {
        let mut state = RateLimitMemory::default();
        let mut recovered = 0;
        let mut calls = 0;
        let event = refused(429, Vec::new());

        let result = wait_out_rate_limit(
            event.clone(),
            Some(Duration::from_millis(10)),
            Instant::now(),
            Duration::ZERO,
            &mut state,
            &mut recovered,
            |_url| {
                calls += 1;
                ok()
            },
        );

        assert_eq!(
            result, event,
            "the refusal was not handed back for the caller to record as owed"
        );
        assert_eq!(calls, 0, "a wait was taken past the run's own deadline");
        assert_eq!(recovered, 0);
    }

    /// A run with no deadline has nothing for a bound to be measured against, so it does not
    /// grow one of its own: the address is handed back exactly as refused as it arrived, and
    /// the fetch this was given is never called. A host that refuses forever in this run costs
    /// this one address, and the run goes on to whatever else is left rather than waiting on
    /// it for as long as the run itself has chosen to be willing to run.
    #[test]
    fn wait_out_rate_limit_never_waits_when_the_run_has_no_deadline() {
        let mut state = RateLimitMemory::default();
        let mut recovered = 0;
        let mut calls = 0;
        let event = refused(429, Vec::new());

        let result = wait_out_rate_limit(
            event.clone(),
            None,
            Instant::now(),
            Duration::ZERO,
            &mut state,
            &mut recovered,
            |_url| {
                calls += 1;
                ok()
            },
        );

        assert_eq!(result, event);
        assert_eq!(
            calls, 0,
            "a wait was taken on a run with no deadline at all"
        );
        assert_eq!(recovered, 0);
    }

    /// The deadline bounds the run and this bounds the address inside it, and both have to
    /// hold: every caller here sleeps on the thread that is also draining the crawl, so an
    /// address allowed to spend the whole budget is an address that stops the run.
    ///
    /// Eight seconds of budget is two seconds for one address. The first wait is a second
    /// and fits; the two second wait after it would put this address at three, so the
    /// address is given back refused. Bounded by the deadline alone this would have waited
    /// one, two and four seconds, asked three times, and spent seven of the eight.
    #[test]
    fn wait_out_rate_limit_spends_at_most_its_share_of_the_budget_on_one_address() {
        let mut state = RateLimitMemory::default();
        let mut recovered = 0;
        let mut calls = 0;

        let started = Instant::now();
        let result = wait_out_rate_limit(
            refused(429, Vec::new()),
            Some(Duration::from_secs(8)),
            started,
            Duration::ZERO,
            &mut state,
            &mut recovered,
            |_url| {
                calls += 1;
                refused(429, Vec::new())
            },
        );
        let elapsed = started.elapsed();

        assert!(
            matches!(&result, PageEvent::Response(page) if page.status == 429),
            "{result:?}"
        );
        assert_eq!(
            calls, 1,
            "one address spent more than its share of the run's budget"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "one address held the run for {elapsed:?} of an eight second budget"
        );
    }

    /// `Retry-After` is a number a host chose, and the largest one it can spell overflows
    /// the sum that decides whether the wait fits. Refused, rather than panicking the run
    /// that was polite enough to read the header.
    #[test]
    fn wait_out_rate_limit_refuses_a_retry_after_too_large_to_add_up() {
        let mut state = RateLimitMemory::default();
        let mut recovered = 0;
        let mut calls = 0;
        let event = refused(429, vec![retry_after(&u64::MAX.to_string())]);

        let result = wait_out_rate_limit(
            event.clone(),
            Some(Duration::from_secs(300)),
            Instant::now(),
            Duration::ZERO,
            &mut state,
            &mut recovered,
            |_url| {
                calls += 1;
                ok()
            },
        );

        assert_eq!(result, event);
        assert_eq!(calls, 0);
    }

    /// The run owes the host its own pace between requests, and the request after a waited
    /// out refusal is the caller's rather than this function's: a crawl's scheduler counts
    /// its own throttle down while this thread sleeps, so it dispatches the next page the
    /// instant this returns. The gap is paid here or it is not paid at all.
    #[test]
    fn wait_out_rate_limit_leaves_the_run_s_own_pace_behind_its_last_request() {
        let mut state = RateLimitMemory::default();
        let mut recovered = 0;
        let floor = Duration::from_millis(600);

        let started = Instant::now();
        let result = wait_out_rate_limit(
            refused(429, Vec::new()),
            Some(Duration::from_secs(30)),
            started,
            floor,
            &mut state,
            &mut recovered,
            |_url| ok(),
        );
        let elapsed = started.elapsed();

        assert!(
            matches!(&result, PageEvent::Response(page) if page.status == 200),
            "{result:?}"
        );
        // One second of backoff, which the floor does not reach, and the floor again after
        // the request it took: under one and a half means the second one was never paid.
        assert!(
            elapsed >= Duration::from_millis(1_500),
            "the pace owed after the recovered request was not waited, took {elapsed:?}"
        );
    }

    /// A server that has already been refused several times in a row does not lend that
    /// count to an address on another one: the wait `a.example`'s own count would produce is
    /// growing and long, and a fresh server answering 429 for the first time still gets the
    /// smallest wait backoff chooses, not the one `a.example` has earned. Sharing the count
    /// would cost the run addresses on servers that never refused anything.
    #[test]
    fn wait_out_rate_limit_keeps_a_separate_backoff_per_server() {
        let mut state = RateLimitMemory::default();
        // Keyed through the same `rate_limit_key` production code uses, rather than a
        // literal guess at its spelling, so a change to how a server is derived from a URL
        // is a change this test would notice too: seeding under a key nothing ever looks up
        // again would pass whether or not the two servers actually stayed apart.
        //
        // As if `a.example` had already been refused three times in a row; its own next wait
        // would be eight seconds, `RATE_LIMIT_BASE_BACKOFF * 2^3`.
        state.refused_again(&rate_limit_key("https://a.example/first"), 3);
        let mut recovered = 0;

        let started = Instant::now();
        let result = wait_out_rate_limit(
            refused_at("https://b.example/other", 429, Vec::new()),
            Some(Duration::from_secs(10)),
            started,
            Duration::ZERO,
            &mut state,
            &mut recovered,
            |_url| refused_at("https://b.example/other", 200, Vec::new()),
        );
        let elapsed = started.elapsed();

        assert!(
            matches!(&result, PageEvent::Response(page) if page.status == 200),
            "{result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(4),
            "a fresh server inherited another server's own backoff count, took {elapsed:?}"
        );
        assert_eq!(
            state.consecutive_refusals(&rate_limit_key("https://a.example/first")),
            3,
            "an unrelated server's own count changed when only b.example was asked for anything"
        );
    }
}
