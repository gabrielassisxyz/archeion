//! The line between the archival core and whatever crawls the web for it.
//!
//! Everything above this file depends on the types here and never on an engine. All an
//! engine may say is `PageEvent`, all it is asked for is a `Seed`, and both are written in
//! the archive's terms rather than the engine's, so swapping the engine is a new adapter
//! and not a rewrite of the code that stores what it produced.

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::ops::ControlFlow;
use std::time::Duration;

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
    pub links_never_followed: Vec<String>,
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
}
