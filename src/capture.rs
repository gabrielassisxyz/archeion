//! Running a seed into the archive: the one place a crawl engine and the store meet.
//!
//! Nothing here knows which engine is underneath, and the engine knows nothing about the
//! archive. What connects them is a page event turning into a capture, which is where
//! canonicalization decides the address the page is filed under.

use std::collections::{BTreeMap, HashSet};
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use crate::assets::{AssetCapture, CapturedAssets};
use crate::canonical_url::{CanonicalUrl, InvalidCanonicalUrl};
use crate::crawl::{
    CrawlEngine, CrawlError, CrawlStop, FetchFailure, PageEvent, PageResponse, Seed,
    points_inside_a_network,
};
use crate::metadata::{self, PageMetadata, PageSource, ReferencedAsset, UnreadablePage};
use crate::readability::{self, Extraction, SiteRules, UnreadableArticle};
use crate::storage::{
    Archive, Header, NewCapture, OwedAddress, OwedReason, PolicyDeparture, StorageError,
};

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error(transparent)]
    Crawl(#[from] CrawlError),
    /// The run is carried out with the error because it is the moment the report matters
    /// most: the archive holds whatever was written before the disk refused, and a caller
    /// that only learns the write failed has to go looking for the rest.
    #[error("{source}")]
    Storage {
        #[source]
        source: StorageError,
        /// Behind a pointer because the report grows with every kind of loss a run can
        /// have, and carrying it inline would make every `Result` on this path as wide as
        /// the widest report rather than as wide as an error.
        run: Box<CaptureRun>,
    },
}

/// A page the crawl fetched and the archive has no address for. It is reported rather than
/// counted, because the address is the whole reason it was refused and a number leaves
/// nothing to look at.
#[derive(Debug, PartialEq, Eq)]
pub struct UnaddressablePage {
    pub url: String,
    pub reason: InvalidCanonicalUrl,
}

/// The address of a response the host refused, and the header that would say when to come
/// back, if it sent one. `responses_refused` below stays a count by status because a
/// consumer outside this repo already reads that shape; this is the per-address detail the
/// count cannot carry, and it is what the owed record in the archive is built from.
///
/// `url` is the canonical spelling, the same one an item would be filed under had the
/// response succeeded, and not the final URL as the response actually spelled it. That is
/// the deliberate choice, not an oversight: it is what lets `Archive::write_owed` tell a
/// newly archived address apart from one still owed by comparing it against
/// `CaptureRun::archived_urls`, which is canonical for the same reason. A later run resuming
/// from this record re-resolves the canonical address rather than replaying a literal
/// string, which it would have to do regardless, since a resume is free to reach the same
/// page by a different route than the one that was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedResponse {
    pub url: String,
    pub status: u16,
    pub retry_after: Option<String>,
}

/// What one seed left behind. Every URL the engine reported is in exactly one of these,
/// so a run that archived less than expected says where the rest went.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CaptureRun {
    pub captures_written: usize,
    /// How many of the captures above landed on an item that already had one, which is what
    /// separates a page a run is seeing for the first time from a page it is seeing again.
    /// Appending is correct either way, so nothing here refuses or skips anything; this is
    /// the count that lets a run into an archive it has already filled say so, since the
    /// storage layer itself gives no other sign that it happened.
    pub items_appended: usize,
    /// How many times this run charged a sitemap phase's own page ceiling: a capture written
    /// to disk, or a response the host refused. It exists because `captures_written` cannot
    /// be that count any more: a refusal stopped being a capture, and `docs/cli.md` still
    /// promises `captures_written` means captures written to disk. `RunSoFar` reads this
    /// field, and only the sitemap phase reads `RunSoFar`; the ordinary crawl's own ceiling
    /// is the engine's to enforce and never reads a field of this struct at all.
    pub pages_spent: usize,
    /// How many of those captures the run's subscription actually reached, which is zero for
    /// every run that carries none.
    ///
    /// A credential can apply to nothing at all and cost nothing but the archive: a seed spelled
    /// with a trailing dot, an `http` seed whose host redirects to `https`, a credential bound to
    /// an origin the run never asks for. Each of those archives the paid half of a publication as
    /// teasers, exits zero, and looks exactly like a run nobody gave a session to. This count is
    /// the one thing that separates the two, so the report can say the session did nothing.
    pub captures_with_a_session: usize,
    /// How many responses the host answered with an error, by status. Neither the response
    /// nor its address is kept here: this field's name and shape are depended on outside
    /// this repo, so it stays a count exactly as before, and `refused_responses` below is
    /// where the address and the reason actually live now.
    ///
    /// Counted by status rather than listed by URL, because the interesting question is
    /// whether the run was refused at all and one line can say so. Which URLs is a question
    /// `refused_responses` answers, and a hundred and sixty of them on stderr is a wall
    /// nobody reads.
    ///
    /// Four hundred and up, so a redirect the archive stored rather than followed is not
    /// counted here. Under the strict redirect policy every hop off the host is archived as
    /// the record of what the host said, which is a decision this archive made and not a
    /// refusal: counting those would print `250 answered 301` for a site that moved and
    /// answered every request, which is the false alarm that would teach a reader to ignore
    /// this row.
    pub responses_refused: BTreeMap<u16, usize>,
    /// Every response counted above, named by address, with its `Retry-After` header when
    /// the host sent one. A refused response is not stored as a capture: this is what
    /// survives it, and what the archive's owed record is built from.
    pub refused_responses: Vec<RefusedResponse>,
    /// The canonical addresses this run has already been refused on, so a listed URL the
    /// sitemap phase reaches after the ordinary crawl already asked for it and was turned
    /// away is not asked for a second time. `archived_urls` cannot serve this: it names what
    /// this run actually filed, and the owed record's merge in `Archive::write_owed` reads
    /// it to mean exactly that.
    pub refused_urls: HashSet<String>,
    pub unaddressable_pages: Vec<UnaddressablePage>,
    /// Pages that ended on an address existing only inside a network, which a run that did
    /// not ask for those addresses had no business reaching. They are reported rather than
    /// counted: the address is the whole reason the page was refused, and it is also the
    /// only evidence that something redirected the crawl there.
    pub pages_inside_a_network: Vec<String>,
    /// URLs no server answered. They are reported and not stored: there is no response to
    /// archive, and inventing one would put a status in the record nothing ever sent.
    pub failed_fetches: Vec<FetchFailure>,
    /// Pages that were stored whole and whose markup the extractor could not read. Only the
    /// derived reading is missing, so the run goes on: the response is in the archive, and
    /// a later pass can read it again without fetching anything.
    pub unreadable_pages: Vec<UnreadablePage>,
    /// Pages whose prose was extracted. It is far fewer than the captures, and that is
    /// correct: most of the web is navigation, and a page that is not an article is not a
    /// failure of anything.
    pub articles_extracted: usize,
    /// Pages that produced prose the extractor then refused to call an article. Counted here
    /// and written beside the capture: the count says whether the rule is firing at all, and
    /// only the files it leaves can say whether it fired on something it should have kept.
    pub extractions_refused: usize,
    /// Pages whose prose was refused because reading it would have cost too much. Reported
    /// rather than counted, for the same reason as the pages above: the response is stored
    /// whole and the URL is what someone needs in order to go and look at it.
    pub unreadable_articles: Vec<UnreadableArticle>,
    /// Pages the engine fetched that never reached the archive, straight from the engine.
    pub pages_dropped: usize,
    /// Links the crawl discovered, inside the seed's own limits, that the engine never
    /// fetched at all even though the run reports nothing was left to do. Straight from
    /// the engine, and reported rather than counted for the reason every other URL here is:
    /// each one is a page the archive is missing that a person can go and look at.
    pub links_never_followed: Vec<String>,
    /// Links the frontier lost and this run fetched directly instead, straight from the
    /// engine. Counted rather than listed: a recovered link is an ordinary capture the
    /// moment it lands, so `list` already names it.
    pub links_recovered: usize,
    /// Subresources stored beside the captures of this run.
    pub assets_stored: usize,
    /// Subresources a page referenced and its capture does not hold. Each one is in the
    /// capture record with the reason, which is where it belongs: this count is for the run,
    /// and the run is over by the time anyone asks why a page looks wrong.
    pub assets_missed: usize,
    /// Requests the subresource pass made. The gap between this and the two counts above is
    /// the run recognising a file shared by many pages instead of asking for it once per page.
    pub asset_fetches: usize,
    /// Why the run ended. A run that stopped at its deadline archived a prefix of a site
    /// rather than the site, and the difference is not visible in any of the counts above.
    pub stopped: CrawlStop,
    /// The canonical addresses this run filed, which is how a second phase of it knows not to
    /// buy the same page twice. Bounded by the page ceiling, like the run itself.
    pub archived_urls: HashSet<String>,
}

impl CaptureRun {
    /// Folds another run's results into this one.
    ///
    /// `--from-sitemap` is additive to the ordinary crawl from the seed rather than a second
    /// report about it, so what it found is one more phase of the same run: every count is
    /// summed, every list is extended, and the phase that ended last is the one whose stop
    /// reason survives, since that is the one that actually decided when the run was done.
    pub fn merge(&mut self, other: CaptureRun) {
        self.captures_written += other.captures_written;
        self.items_appended += other.items_appended;
        self.pages_spent += other.pages_spent;
        self.captures_with_a_session += other.captures_with_a_session;
        for (status, count) in other.responses_refused {
            *self.responses_refused.entry(status).or_default() += count;
        }
        self.refused_responses.extend(other.refused_responses);
        self.refused_urls.extend(other.refused_urls);
        self.unaddressable_pages.extend(other.unaddressable_pages);
        self.pages_inside_a_network
            .extend(other.pages_inside_a_network);
        self.failed_fetches.extend(other.failed_fetches);
        self.unreadable_pages.extend(other.unreadable_pages);
        self.articles_extracted += other.articles_extracted;
        self.extractions_refused += other.extractions_refused;
        self.unreadable_articles.extend(other.unreadable_articles);
        self.pages_dropped += other.pages_dropped;
        self.links_recovered += other.links_recovered;
        self.assets_stored += other.assets_stored;
        self.assets_missed += other.assets_missed;
        self.asset_fetches += other.asset_fetches;
        self.stopped = other.stopped;
        self.archived_urls.extend(other.archived_urls);
    }
}

/// Crawls a seed and stores every page it produces, for as long as the seed's budget lasts.
///
/// The deadline is the engine's to enforce, because a host that accepts a connection and
/// then says nothing produces no page, and a callback that is never called cannot end
/// anything. What lives here is the backstop for the opposite failure, an engine that
/// ignores the field, and it deliberately fires late rather than on the instant: see
/// `engine_overran_its_deadline`.
pub fn capture_seed(
    engine: &dyn CrawlEngine,
    archive: &Archive,
    seed: &Seed,
    rules: &SiteRules,
) -> Result<CaptureRun, CaptureError> {
    let mut run = CaptureRun::default();
    let mut write_failure: Option<StorageError> = None;
    let started = Instant::now();
    let deadline = seed.deadline;
    let mut engine_overran = false;
    // The pass outlives every page because what it learned about one page's subresources is
    // the answer for the next page that references them.
    let mut assets = AssetCapture::new(engine, archive, seed, started);

    let outcome = engine.crawl(seed, &mut |event| {
        let answer = capture_page(
            event,
            archive,
            seed,
            rules,
            &mut assets,
            &mut run,
            &mut write_failure,
        );
        if answer.is_break() {
            return ControlFlow::Break(());
        }
        // Read after the page is filed rather than before: it arrived already fetched, and
        // refusing to write what is in hand spends the bytes without keeping anything.
        if engine_overran_its_deadline(deadline, started.elapsed()) {
            engine_overran = true;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    })?;

    run.pages_dropped = outcome.pages_dropped;
    run.links_never_followed = outcome.links_never_followed;
    run.links_recovered = outcome.links_recovered;
    run.asset_fetches = assets.fetches();
    // The engine reports that its caller stopped it. This is that caller, and it knows why.
    run.stopped = if engine_overran {
        CrawlStop::DeadlineReached
    } else {
        outcome.stopped
    };
    if let Some(source) = write_failure {
        return Err(CaptureError::Storage {
            source,
            run: Box::new(run),
        });
    }
    Ok(run)
}

/// What earlier phases of the same run already did, which is what a later phase has to know to
/// keep the run's bounds the run's rather than its own.
///
/// `--max-pages` and `--deadline` bound a run, not one phase of it, and a run has two phases
/// whenever `--max-depth` is given beside `--from-sitemap`. Each phase measuring from its own
/// zero is how a run given fifty pages archived a hundred and a run given ten minutes took
/// twenty, while reporting the bound it was asked for.
///
/// The addresses travel here beside the counts because they answer the same question about the
/// same earlier phases, and because a phase that had to be handed them separately is a phase
/// somebody can hand one of the two and forget the other.
#[derive(Debug, Clone, Copy)]
pub struct RunSoFar<'a> {
    /// When the run began, which is not when the phase reading this began.
    pub started: Instant,
    /// What earlier phases of this run have already charged against the page ceiling: a
    /// capture written to disk or a response the host refused, either one a page the run
    /// spent regardless of what came back.
    pub pages_written: usize,
    /// The canonical addresses already filed or refused, so a listed URL the crawl reached
    /// is not bought a second time and a host that already refused one is not asked again.
    pub archived: &'a HashSet<String>,
}

impl<'a> RunSoFar<'a> {
    /// A phase that is the run's first, so the run begins with it and nothing is spent yet.
    pub fn nothing_yet(archived: &'a HashSet<String>) -> Self {
        Self {
            started: Instant::now(),
            pages_written: 0,
            archived,
        }
    }

    /// Everything this run has charged against its page ceiling so far, the phase now
    /// running included: a capture written to disk or a response the host refused, either
    /// one a page the run spent regardless of what came back.
    fn pages_written_including(&self, run: &CaptureRun) -> usize {
        self.pages_written + run.pages_spent
    }

    /// Whether the run can still archive anything, asked before a phase starts.
    ///
    /// Once the budget is shared, a phase can begin with none of it left, and the work a phase
    /// does before its first budget check is a request of its own: reading a sitemap costs a
    /// `robots.txt` and a listing from a host that cannot answer into this run any more. Asked
    /// through the same function the phase itself stops on, because two opinions about one
    /// bound is how a run came to report the wrong one.
    pub fn nothing_left_to_spend(&self, seed: &Seed) -> bool {
        bound_reached(seed, self.pages_written, self.started).is_some()
    }
}

/// Fetches every URL a sitemap listed, additionally to whatever the ordinary crawl from the
/// seed already found.
///
/// A sitemap answers a question a crawl cannot: what a site has, for pages nothing links to.
/// Each URL enters at depth zero, exactly like a seed, and by default nothing is followed
/// out of it, because a depth bound has no meaning for a page nobody linked to: `follow_links`
/// false fetches the URL on its own, the same way a subresource is acquired, and never asks
/// the engine to queue anything past it. An operator who gave `--max-depth` explicitly asked
/// for more than that, so `follow_links` true takes each URL as a seed of its own and crawls
/// from it up to that depth, sharing what is left of the run's own page count and deadline
/// rather than starting each one with a fresh budget, which is what keeps the sitemap's
/// bounds the same bounds the rest of the run already answers to.
///
/// What this deliberately does not share with the ordinary crawl is the subresource cache: a
/// stylesheet referenced by both an ordinarily reached page and a sitemap-listed one is asked
/// for once per phase rather than once for the whole run. That costs the host one extra
/// request for a handful of shared files, not a correctness problem, and sharing the cache
/// across two separately built passes was not worth the extra plumbing for what the two sites
/// this was measured against would ever trigger. The budget is the opposite case and is shared:
/// see `RunSoFar`.
pub fn capture_sitemap(
    engine: &dyn CrawlEngine,
    archive: &Archive,
    seed: &Seed,
    rules: &SiteRules,
    urls: &[String],
    follow_links: bool,
    so_far: RunSoFar<'_>,
) -> Result<CaptureRun, CaptureError> {
    let mut run = CaptureRun::default();
    let mut write_failure: Option<StorageError> = None;
    // The run's own instant rather than this phase's, because a deadline bounds the run. Taken
    // here it would restart the clock, and the subresource pass below reads the same field for
    // the same reason.
    //
    // Two of the three places it lands are pinned by tests. The third is `AssetCapture`, and no
    // test sees it: the loop guard below reads the same instant, so any clock that would refuse
    // a subresource has already refused the page holding it, and the two only come apart while
    // a deadline expires inside one page's own subresource pass. See the hurdle table.
    let started = so_far.started;
    let mut assets = AssetCapture::new(engine, archive, seed, started);

    let mut asked_the_host_for_something = false;

    for url in urls {
        if let Some(bound) = bound_reached(seed, so_far.pages_written_including(&run), started) {
            run.stopped = bound;
            break;
        }
        if already_filed(url, so_far.archived, &run) {
            continue;
        }
        // The wait belongs here because this is the only phase that asks a host for page
        // after page with no traversal between them, and `--delay` does not otherwise reach
        // it: the flag is documented as the wait between requests and it reaches the engine
        // through `with_delay`, which the engine applies to a crawl. What had been standing
        // in for it was the subresource pass being serial, which is an accident of how much
        // work a run happened to do rather than a policy, and it stopped being enough the
        // moment a page's pictures stopped costing eight requests each.
        //
        // Paid per request rather than per URL: an address this run already filed is skipped
        // without asking anybody anything, and the first request has nothing to wait behind.
        //
        // Paid on both branches, including the one where each URL is crawled rather than
        // fetched, and that is not the redundancy it looks like. The engine does sleep for a
        // non-zero delay, but only around the links a crawl discovers for itself: before a
        // batch it already has and before each link it queues. A sitemap sub-crawl has neither,
        // because a sitemap exists precisely for a site whose pages do not link one another, so
        // its seed is fetched with no wait at all. Measured against a loopback site at a one
        // second delay: the sub-crawls ran back to back, gaps of 0.05 seconds, while a crawl of
        // two linked pages at two seconds took four. Skipping the wait here on the strength of
        // the engine's own would leave `--max-depth` runs exactly as unpaced as before.
        //
        // The budget is read again on the far side of the wait, and that is not the same
        // check twice. A wait long enough to matter is a wait the deadline can expire inside,
        // and a phase that checked only before it would sleep past the run's own end and then
        // ask for one more page. What a deadline means is that nothing more is asked for, so
        // the moment it says so is the moment before asking rather than the top of the loop.
        if asked_the_host_for_something {
            pace(seed.delay);
            if let Some(bound) = bound_reached(seed, so_far.pages_written_including(&run), started)
            {
                run.stopped = bound;
                break;
            }
        }
        asked_the_host_for_something = true;
        if follow_links {
            let mut sub_seed = seed.clone();
            sub_seed.url = url.clone();
            sub_seed.max_pages = remaining_pages(seed, so_far.pages_written_including(&run));
            sub_seed.deadline = remaining_deadline(seed, started);
            // A seed the engine will not dial costs this one URL, not the rest of the list:
            // the same host answered for every other entry, so one address the engine
            // refuses is worth reporting and moving past rather than ending the sitemap
            // phase over.
            let outcome = match engine.crawl(&sub_seed, &mut |event| {
                capture_page(
                    event,
                    archive,
                    seed,
                    rules,
                    &mut assets,
                    &mut run,
                    &mut write_failure,
                )
            }) {
                Ok(outcome) => outcome,
                Err(error) => {
                    run.failed_fetches.push(FetchFailure {
                        url: url.clone(),
                        reason: error.to_string(),
                    });
                    continue;
                }
            };
            run.pages_dropped += outcome.pages_dropped;
            if write_failure.is_some() {
                break;
            }
            if outcome.stopped == CrawlStop::DeadlineReached {
                run.stopped = CrawlStop::DeadlineReached;
                break;
            }
        } else {
            let event = engine.fetch(url, seed);
            let answer = capture_page(
                event,
                archive,
                seed,
                rules,
                &mut assets,
                &mut run,
                &mut write_failure,
            );
            if answer.is_break() {
                break;
            }
        }
    }

    run.asset_fetches = assets.fetches();
    if let Some(source) = write_failure {
        return Err(CaptureError::Storage {
            source,
            run: Box::new(run),
        });
    }
    Ok(run)
}

/// Whether an address this run has already settled, filed or refused, is being asked for
/// again.
///
/// A sitemap normally lists the seed, and it can list one page twice, so without this a run
/// buys the same response more than once and the archive grows a second capture of an item
/// nothing about the site changed. A host that already refused the address is the other half:
/// asking it again spends a second request on a host that just said it was being asked too
/// often, for an answer this run already has. The comparison is against the canonical
/// spelling, which is what an item is filed under: what cannot be caught here is a listed URL
/// that redirects onto a page already filed or refused, since only the fetch itself can say
/// where it lands.
fn already_filed(url: &str, already_archived: &HashSet<String>, run: &CaptureRun) -> bool {
    let Ok(canonical) = CanonicalUrl::parse(url) else {
        return false;
    };
    let canonical = canonical.to_string();
    already_archived.contains(&canonical)
        || run.archived_urls.contains(&canonical)
        || run.refused_urls.contains(&canonical)
}

/// Waits the run's politeness delay, if it has one.
///
/// The zero case is a guard and not an optimisation: zero is documented as the one span that
/// means what it says, no wait at all, and handing it to the clock would still cost a trip
/// through the scheduler on every URL of a list that can run to hundreds.
///
/// The wait is counted against the run's deadline, which is measured on the same wall clock,
/// so a delay large enough to matter shortens the run rather than extending it. That is the
/// honest arithmetic: a host that wants to be asked slowly is a host fewer of whose pages fit
/// in a given hour.
fn pace(delay: Duration) {
    if !delay.is_zero() {
        std::thread::sleep(delay);
    }
}

/// Which of the run's bounds is spent, if either: everything the run has written against the
/// seed's own ceiling, or the seed's wall clock against how long the run has gone on. Both are
/// the run's and not the phase's, which is why the count arrives as a total and the instant
/// arrives from the caller. Read plainly rather than with the margin the engine gets, because
/// what is left over here is a prefix of a URL list rather than pages already in flight that a
/// late guard has to hand over.
///
/// It answers which one rather than that one of them was, because the caller writes the answer
/// into the run's stop reason and the two lead somewhere different. A single boolean here had
/// both bounds reporting the deadline, so a run given `--deadline none` printed that its
/// deadline ran out.
fn bound_reached(seed: &Seed, pages_written: usize, started: Instant) -> Option<CrawlStop> {
    if pages_written >= seed.max_pages as usize {
        return Some(CrawlStop::PageCeilingReached);
    }
    seed.deadline
        .is_some_and(|budget| started.elapsed() >= budget)
        .then_some(CrawlStop::DeadlineReached)
}

/// What is left of the run's page count, for a sub-crawl started from a sitemap URL. Floored
/// at one rather than let a fully spent budget reach the engine as a zero, which it would read
/// as no limit at all instead of none left.
fn remaining_pages(seed: &Seed, pages_written: usize) -> u32 {
    let written = u32::try_from(pages_written).unwrap_or(u32::MAX);
    seed.max_pages.saturating_sub(written).max(1)
}

/// What is left of the run's wall clock, for a sub-crawl started from a sitemap URL.
fn remaining_deadline(seed: &Seed, started: Instant) -> Option<Duration> {
    seed.deadline
        .map(|budget| budget.saturating_sub(started.elapsed()))
}

/// Whether the engine is still handing over pages long after the budget it was given.
///
/// The margin is the whole point. An engine that honors the deadline cancels its fetching
/// when the budget expires and then hands over the pages it had already paid for, and a
/// guard that fired on the same instant would break out on the first of them and count the
/// rest as lost. That handover is local writes, so a tenth of the budget is far more room
/// than it needs, while still bounding an engine that ignores the field.
fn engine_overran_its_deadline(deadline: Option<Duration>, elapsed: Duration) -> bool {
    deadline.is_some_and(|budget| elapsed >= budget.saturating_add(budget / 10))
}

/// Files one page under the address the archive knows it by.
///
/// Three shapes are skipped and reported rather than stored, and none of them ends the run:
/// a page the archive cannot address, since one URL the canonical rules refuse says nothing
/// about the other two hundred; a page that ended inside a network the run was not pointed
/// at; and a response the host refused, which is not an item and whose body is not kept, on
/// the reasoning `docs/storage-model.md` records. A failed write is the opposite, and stops
/// the run: the disk that rejected this capture will reject the next one, and a crawl that
/// keeps fetching after that spends a site's bandwidth on nothing.
fn capture_page(
    event: PageEvent,
    archive: &Archive,
    seed: &Seed,
    rules: &SiteRules,
    assets: &mut AssetCapture<'_>,
    run: &mut CaptureRun,
    write_failure: &mut Option<StorageError>,
) -> ControlFlow<()> {
    let page = match event {
        PageEvent::Response(page) => page,
        PageEvent::NoResponse(failure) => {
            run.failed_fetches.push(failure);
            return ControlFlow::Continue(());
        }
    };

    // A seed is screened before anything is dialled, so a page that ends on one of these
    // addresses got there by redirect, through a guard inside the engine. This boundary
    // keeps the same predicate because the engine is replaceable and storing the response
    // is what turns a blind fetch into a durable copy of whatever answered on the machine
    // the archive runs on. The run that asked for local addresses gets them, which is the
    // only way a locally served site is archived at all.
    if !seed.allow_private_addresses && points_inside_a_network(&page.final_url) {
        run.pages_inside_a_network.push(page.final_url);
        return ControlFlow::Continue(());
    }

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

    // A refused response is not an item and its body is not stored: the decision in
    // `arch-9j5` and the reasoning behind it are in `docs/storage-model.md`. Checked here,
    // before a byte of the body is read, so nothing below spends effort extracting or
    // fetching subresources for seventeen bytes of `Too Many Requests`.
    if page.status >= 400 {
        // Charged against the sitemap phase's own page ceiling exactly as a written capture
        // is, a few lines down: before this bead a refusal was a capture and paid into that
        // budget, and it still has to, or a host that refuses everything drains the sitemap
        // listing down to its deadline instead of stopping at `--max-pages`.
        run.pages_spent += 1;
        let canonical_url = canonical.to_string();
        run.refused_urls.insert(canonical_url.clone());
        *run.responses_refused.entry(page.status).or_default() += 1;
        run.refused_responses.push(RefusedResponse {
            retry_after: retry_after_of(&page.headers),
            url: canonical_url,
            status: page.status,
        });
        return ControlFlow::Continue(());
    }

    // Read before the capture is written, because the bytes are still in hand, and stored
    // after, because the response is what cannot be recovered: a run cut short then leaves
    // a capture with no reading of it, which a later pass can produce on its own.
    let extracted = read_page(&page, run);
    let prose = read_prose(&page, extracted.as_ref(), rules, run);
    // The subresources are acquired before the capture is written because the record has to
    // name them, and their bytes are stored before the page's own for the reason every body
    // is stored before every record: a blob nobody references costs disk space, and a record
    // naming bytes that are absent is broken. What it trades is a page lost whole if the
    // process dies mid-pass, rather than a capture on disk claiming subresources it never got.
    let captured = match assets.of_page(referenced_assets(extracted.as_ref())) {
        Ok(captured) => captured,
        Err(error) => {
            *write_failure = Some(error);
            return ControlFlow::Break(());
        }
    };
    let (stored, missed) = (captured.stored.len(), captured.missed.len());
    // Read before the write below can create this item, since that is the only moment left
    // to tell a further capture apart from the item's first.
    let item_already_had_a_capture = archive.has_captures(&canonical);
    let capture = match archive.write_capture(new_capture(canonical.clone(), page, captured, seed))
    {
        Ok(capture) => capture,
        Err(error) => {
            *write_failure = Some(error);
            return ControlFlow::Break(());
        }
    };
    run.captures_written += 1;
    if item_already_had_a_capture {
        run.items_appended += 1;
    }
    run.pages_spent += 1;
    // Counted off the record that landed rather than off the seed, so what the report says a
    // session reached is what the archive says too.
    if capture
        .policy_departures
        .contains(&PolicyDeparture::Session)
    {
        run.captures_with_a_session += 1;
    }
    // Kept so a later phase of the same run does not archive an address this one already has.
    // It is what this run wrote rather than what the archive holds, because an archive holds
    // the history of everything ever captured and the question here is about one run.
    run.archived_urls.insert(canonical.to_string());
    // Counted with the capture rather than with the pass. A subresource whose capture never
    // reached the disk is a blob nothing references, and reporting it beside a capture that
    // does not exist would describe an archive nobody has.
    run.assets_stored += stored;
    run.assets_missed += missed;

    if let Some(metadata) = &extracted
        && let Err(error) = archive.write_metadata(&canonical, &capture.id, metadata)
    {
        *write_failure = Some(error);
        return ControlFlow::Break(());
    }

    let stored_prose = match &prose {
        Extraction::Article(article) => archive.write_article(&canonical, &capture.id, article),
        Extraction::Refused(refused) => {
            archive.write_refused_extraction(&canonical, &capture.id, refused)
        }
        Extraction::NotArticle(non_article) => {
            archive.write_non_article(&canonical, &capture.id, non_article)
        }
        Extraction::Nothing => Ok(()),
    };
    if let Err(error) = stored_prose {
        *write_failure = Some(error);
        return ControlFlow::Break(());
    }
    ControlFlow::Continue(())
}

/// Separates the page's prose from the furniture around it, or reports that it could not.
///
/// Like the metadata above, a page this fails on is not a failed capture: the response is
/// stored whole, and the only thing missing is a reading of it that a later pass can redo
/// without fetching anything.
///
/// The title comes from the metadata rather than from this page's markup, so that the
/// precedence rules that live there decide it once instead of this forming a second opinion.
/// `accessible_for_free` is resolved the same way, from the JSON-LD metadata extraction already
/// parsed, rather than this function reading raw blocks itself.
fn read_prose(
    page: &PageResponse,
    metadata: Option<&PageMetadata>,
    rules: &SiteRules,
    run: &mut CaptureRun,
) -> Extraction {
    let title = metadata
        .and_then(|metadata| metadata.title.as_ref())
        .map(|title| title.value.as_str());
    let accessible_for_free =
        metadata.and_then(|metadata| readability::declared_accessible_for_free(&metadata.json_ld));
    match readability::extract(
        PageSource {
            body: &page.body,
            content_type: content_type_of(&page.headers),
            final_url: &page.final_url,
        },
        title,
        accessible_for_free,
        rules,
    ) {
        Ok(Extraction::Article(article)) => {
            run.articles_extracted += 1;
            Extraction::Article(article)
        }
        Ok(Extraction::Refused(refused)) => {
            run.extractions_refused += 1;
            Extraction::Refused(refused)
        }
        Ok(Extraction::NotArticle(non_article)) => Extraction::NotArticle(non_article),
        Ok(Extraction::Nothing) => Extraction::Nothing,
        Err(refused) => {
            run.unreadable_articles.push(refused);
            Extraction::Nothing
        }
    }
}

/// Reads what the page says about itself, or reports that it could not be read.
///
/// A page the parser gives up on is not a failed capture. The response was fetched and is
/// about to be stored whole, and the only thing missing is a reading of it that costs
/// nothing to redo later, so the run keeps going and says which page it was.
fn read_page(page: &PageResponse, run: &mut CaptureRun) -> Option<PageMetadata> {
    match metadata::extract(PageSource {
        body: &page.body,
        content_type: content_type_of(&page.headers),
        final_url: &page.final_url,
    }) {
        Ok(extracted) => extracted,
        Err(unreadable) => {
            run.unreadable_pages.push(unreadable);
            None
        }
    }
}

/// What a page referenced, or nothing at all when there was no reading of it. A capture that
/// is not a page has no subresources to acquire, and neither has one whose markup the
/// extractor could not read: the references are in the part it failed on.
fn referenced_assets(extracted: Option<&PageMetadata>) -> &[ReferencedAsset] {
    extracted.map_or(&[], |metadata| metadata.assets.as_slice())
}

fn new_capture(
    canonical_url: CanonicalUrl,
    page: PageResponse,
    captured: CapturedAssets,
    seed: &Seed,
) -> NewCapture {
    let media_type = media_type_of(&page.headers);
    let policy_departures = policy_departures_of(seed, &page.final_url);
    NewCapture {
        canonical_url,
        requested_url: page.requested_url,
        final_url: page.final_url,
        status: page.status,
        media_type,
        response_headers: page.headers,
        body: page.body,
        body_truncated: page.body_truncated,
        fetched_at: page.fetched_at,
        assets: captured.stored,
        assets_missed: captured.missed,
        policy_departures,
    }
}

/// How the response this capture holds departed from what an ordinary run receives.
///
/// It is asked of one address rather than of the run as a whole, because a session belongs to
/// one origin: a run holding one for a publication asks another host's pages without it, and
/// recording those as authenticated would describe an observation nobody made.
///
/// The final URL and not the requested one. The field says what the stored response is, and a
/// redirect that leaves the credential's origin makes the HTTP client drop the header before the
/// hop, so the body that arrived is the one an anonymous reader gets: an apex redirecting to its
/// `www` form, or an `http` seed reaching its `https` one, would otherwise store a teaser under
/// a record claiming a paying reader was served. What this still cannot see is a chain that
/// leaves the origin and comes back, since the header is gone for the rest of the chain while
/// the final address matches again.
fn policy_departures_of(seed: &Seed, final_url: &str) -> Vec<PolicyDeparture> {
    let mut departures = Vec::new();
    if let Some(cookie) = &seed.session_cookie
        && cookie.value_for(final_url).is_some()
    {
        departures.push(PolicyDeparture::Session);
    }
    departures
}

/// The media type without its parameters: `text/html` out of `text/html; charset=utf-8`.
/// Nothing is lost by narrowing it, since the header survives verbatim in the record, and
/// the field then holds what its name promises instead of a string every reader re-parses.
pub(crate) fn media_type_of(headers: &[Header]) -> Option<String> {
    content_type_of(headers)
        .map(|content_type| {
            let (media_type, _parameters) =
                content_type.split_once(';').unwrap_or((content_type, ""));
            media_type.trim().to_ascii_lowercase()
        })
        .filter(|media_type| !media_type.is_empty())
}

fn content_type_of(headers: &[Header]) -> Option<&str> {
    headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("content-type"))
        .map(|header| header.value.as_str())
}

/// The header a refused response uses to say when asking again might work. Read off the
/// response and not stored on it: the body of a refusal is discarded, and this is the one
/// part of it the owed record keeps.
fn retry_after_of(headers: &[Header]) -> Option<String> {
    headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("retry-after"))
        .map(|header| header.value.clone())
}

/// Every address this run did not archive, with why, folded into the shape the archive
/// keeps outside its collection.
///
/// The destructure below is exhaustive and binds every field this function does not read to
/// `_`, on purpose: `CaptureRun`'s own comment says every URL the engine reported is in
/// exactly one of its fields, and a loss field added there without a matching arm here used
/// to compile silently and leave that loss unrecorded in the archive. Adding a field now
/// fails the build here instead.
///
/// The final pass answers the case a reason above cannot: the same address reaching two of
/// these fields, or the same field twice, most plainly when a run's crawl phase is refused on
/// an address its sitemap phase also lists, and both push into `refused_responses`.
/// The first entry for an address is the one kept.
pub fn owed_addresses(run: &CaptureRun) -> Vec<OwedAddress> {
    let CaptureRun {
        captures_written: _,
        items_appended: _,
        pages_spent: _,
        captures_with_a_session: _,
        responses_refused: _,
        refused_responses,
        refused_urls: _,
        unaddressable_pages,
        pages_inside_a_network,
        failed_fetches,
        unreadable_pages: _,
        articles_extracted: _,
        extractions_refused: _,
        unreadable_articles: _,
        pages_dropped: _,
        links_never_followed,
        assets_stored: _,
        assets_missed: _,
        asset_fetches: _,
        stopped: _,
        archived_urls: _,
    } = run;

    let mut owed = Vec::new();
    owed.extend(refused_responses.iter().map(|refused| OwedAddress {
        url: refused.url.clone(),
        reason: OwedReason::Refused {
            status: refused.status,
            retry_after: refused.retry_after.clone(),
        },
    }));
    owed.extend(failed_fetches.iter().map(|failure| OwedAddress {
        url: failure.url.clone(),
        reason: OwedReason::NoResponse {
            detail: failure.reason.clone(),
        },
    }));
    owed.extend(unaddressable_pages.iter().map(|page| OwedAddress {
        url: page.url.clone(),
        reason: OwedReason::Unaddressable {
            detail: page.reason.to_string(),
        },
    }));
    owed.extend(
        pages_inside_a_network
            .iter()
            .cloned()
            .map(|url| OwedAddress {
                url,
                reason: OwedReason::InsideANetwork,
            }),
    );
    owed.extend(links_never_followed.iter().cloned().map(|url| OwedAddress {
        url,
        reason: OwedReason::NeverFollowed,
    }));

    let mut seen = HashSet::new();
    owed.retain(|address| seen.insert(address.url.clone()));
    owed
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;
    use crate::crawl::{CrawlOutcome, SessionCookie};
    use crate::storage::AssetMiss;

    /// A run over a host nothing has been told about, which is what every test here but the one
    /// below is about. Where a rule reaches the extraction is `readability`'s subject, and this
    /// module's is what a run does with what came back.
    fn capture_with_no_rules(
        engine: &dyn CrawlEngine,
        archive: &Archive,
        seed: &Seed,
    ) -> Result<CaptureRun, CaptureError> {
        capture_seed(engine, archive, seed, &SiteRules::default())
    }

    /// A crawl engine that replays a written-down list of page events instead of fetching
    /// anything. It is the whole reason the boundary exists: the pipeline above it is
    /// testable without a network, and what it does with a 404 or with a page it cannot
    /// address is decided by the test rather than by whatever the web answered today.
    struct ScriptedCrawlEngine {
        pages: Vec<PageEvent>,
        /// What each URL fetched on its own answers with. A URL that is not written down
        /// answered nothing, which is the only honest default for a fake with no network.
        subresources: HashMap<String, PageEvent>,
        outcome: CrawlOutcome,
        /// What the pipeline answered for each page, so a test can prove the crawl stopped
        /// rather than infer it from a count.
        answers: RefCell<Vec<ControlFlow<()>>>,
        /// Every URL fetched on its own, in order. A file shared by two pages should cost one
        /// request, and a count is the only way a test can say so.
        fetched: RefCell<Vec<String>>,
        /// Every seed this engine was asked to crawl. A sub-crawl started from a sitemap URL
        /// carries what is left of the run's budget, and the numbers on that seed are the only
        /// place a test can see the sharing happen: nothing the engine does with them is
        /// visible to a fake that has no clock and no page limit of its own.
        crawled: RefCell<Vec<Seed>>,
    }

    impl ScriptedCrawlEngine {
        fn new(pages: Vec<PageEvent>) -> Self {
            Self {
                pages,
                subresources: HashMap::new(),
                outcome: CrawlOutcome::default(),
                answers: RefCell::new(Vec::new()),
                fetched: RefCell::new(Vec::new()),
                crawled: RefCell::new(Vec::new()),
            }
        }

        /// The subresources this engine will answer for, keyed by the URL they are asked for.
        fn serving(mut self, subresources: Vec<PageEvent>) -> Self {
            for event in subresources {
                let url = match &event {
                    PageEvent::Response(response) => response.requested_url.clone(),
                    PageEvent::NoResponse(failure) => failure.url.clone(),
                };
                self.subresources.insert(url, event);
            }
            self
        }

        fn pages_offered(&self) -> usize {
            self.answers.borrow().len()
        }

        fn urls_fetched(&self) -> Vec<String> {
            self.fetched.borrow().clone()
        }

        /// The seeds this engine was handed, which is where a sub-crawl's share of the run's
        /// budget is readable.
        fn seeds_crawled(&self) -> Vec<Seed> {
            self.crawled.borrow().clone()
        }
    }

    impl CrawlEngine for ScriptedCrawlEngine {
        /// This engine dials nothing, so there is no seed it has a reason to refuse. What a
        /// real one refuses is checked where that rule lives, in `spider_engine`.
        fn check_seed(&self, _seed: &Seed) -> Result<(), CrawlError> {
            Ok(())
        }

        fn crawl(
            &self,
            seed: &Seed,
            on_page: &mut dyn FnMut(PageEvent) -> ControlFlow<()>,
        ) -> Result<CrawlOutcome, CrawlError> {
            self.crawled.borrow_mut().push(seed.clone());
            for page in &self.pages {
                let answer = on_page(page.clone());
                self.answers.borrow_mut().push(answer);
                if answer.is_break() {
                    break;
                }
            }
            Ok(self.outcome.clone())
        }

        fn fetch(&self, url: &str, _seed: &Seed) -> PageEvent {
            self.fetched.borrow_mut().push(url.to_owned());
            self.subresources.get(url).cloned().unwrap_or_else(|| {
                PageEvent::NoResponse(FetchFailure {
                    url: url.to_owned(),
                    reason: "this fake was given nothing to answer with".to_owned(),
                })
            })
        }
    }

    fn page(url: &str, status: u16, body: &str) -> PageEvent {
        PageEvent::Response(PageResponse {
            requested_url: url.to_owned(),
            final_url: url.to_owned(),
            status,
            headers: vec![Header {
                name: "content-type".to_owned(),
                value: "text/html; charset=utf-8".to_owned(),
            }],
            body: body.as_bytes().to_vec(),
            body_truncated: false,
            fetched_at: "2026-07-25T14:03:22Z".parse().expect("valid timestamp"),
        })
    }

    fn response_of(event: &mut PageEvent) -> &mut PageResponse {
        match event {
            PageEvent::Response(response) => response,
            PageEvent::NoResponse(failure) => panic!("expected a response, got {failure:?}"),
        }
    }

    fn archive_in(dir: &TempDir) -> Archive {
        Archive::open(dir.path()).expect("archive opens in an empty directory")
    }

    #[test]
    fn a_page_is_archived_with_a_reading_of_it_beside_the_capture() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![page(
            "https://example.com/a",
            200,
            r#"<html><head><title>A page</title></head>
               <body><a href="/b">b</a></body></html>"#,
        )]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert!(run.unreadable_pages.is_empty());
        let url = CanonicalUrl::parse("https://example.com/a").expect("valid url");
        let captures = archive.list_captures(&url).expect("captures are listed");
        let metadata = archive
            .read_metadata(&url, &captures[0])
            .expect("the reading is stored")
            .expect("a page has a reading");

        assert_eq!(metadata.title.expect("a title").value, "A page");
        assert_eq!(metadata.links[0].url, "https://example.com/b");
    }

    #[test]
    fn an_article_is_archived_as_a_markdown_document_beside_the_capture() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![page(
            "https://example.com/a",
            200,
            &format!(
                r#"<html><head><title>A page for the tab</title>
                   <meta property="og:title" content="How to bake bread"></head>
                   <body><nav><a href="/b">b</a></nav><article>{}</article>
                   <footer>Subscribe to our newsletter</footer></body></html>"#,
                "<p>Bread is mostly patience, and the dough will tell you when it is ready.</p>"
                    .repeat(8)
            ),
        )]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.articles_extracted, 1);
        assert!(run.unreadable_articles.is_empty());
        let url = CanonicalUrl::parse("https://example.com/a").expect("valid url");
        let captures = archive.list_captures(&url).expect("captures are listed");
        let article = archive
            .read_article(&url, &captures[0])
            .expect("the prose is stored")
            .expect("an article has prose");

        // The heading is the title the metadata rules resolved, not the raw `<title>`.
        assert!(
            article.markdown.starts_with("# How to bake bread\n"),
            "{}",
            article.markdown
        );
        assert!(article.markdown.contains("Bread is mostly patience"));
        assert!(!article.markdown.contains("Subscribe to our newsletter"));
        assert!(article.record.word_count > 0);
    }

    /// The rules reach the stored article, which is the only part of them this module owns. A
    /// run that read a rule file and then handed the extractor nothing would still archive the
    /// page, still report an article, and be wrong only in the file a person opens.
    #[test]
    fn what_a_host_was_told_reaches_the_article_that_is_stored() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![page(
            "https://example.com/a",
            200,
            &format!(
                r#"<html><head><title>How to bake bread</title></head>
                   <body><article>{}
                   <aside class="house-appeal"><p>Every article here is written by a person who
                   was paid to write it, and a subscription is what pays them. There is a free
                   trial month if you would rather read a few more before deciding.</p>
                   </aside></article></body></html>"#,
                "<p>Bread is mostly patience, and the dough will tell you when it is ready.</p>"
                    .repeat(8)
            ),
        )]);
        let (rules, unused) = SiteRules::parse(
            r#"{"hosts": {"example.com": {"strip": ["aside.house-appeal"]}}}"#,
            "a test",
        );
        assert!(unused.is_empty(), "{unused:?}");

        let run = capture_seed(
            &engine,
            &archive,
            &Seed::new("https://example.com/"),
            &rules,
        )
        .expect("the run completes");

        assert_eq!(run.articles_extracted, 1);
        let url = CanonicalUrl::parse("https://example.com/a").expect("valid url");
        let captures = archive.list_captures(&url).expect("captures are listed");
        let article = archive
            .read_article(&url, &captures[0])
            .expect("the prose is stored")
            .expect("an article has prose");

        assert!(article.markdown.contains("Bread is mostly patience"));
        assert!(
            !article
                .markdown
                .contains("a subscription is what pays them"),
            "{}",
            article.markdown
        );
        assert_eq!(
            article.record.rules,
            crate::readability::ExtractionRules::Site("example.com".to_owned())
        );
    }

    /// A page refused for being a sliver of itself leaves a record and no article, which is
    /// the difference between the two ways of not being one: the capture below is passed over
    /// in silence, and this one goes into a queue somebody is expected to read.
    #[test]
    fn a_front_page_refused_as_an_article_is_recorded_beside_its_capture() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![page(
            "https://example.com/",
            200,
            &format!(
                r#"<html><head><title>The Slow Kitchen</title></head>
                   <body><header><h1>The Slow Kitchen</h1><p>Notes on bread, patience and the
                   things that take longer than the recipe says they will.</p></header><main>
                   <p>This is where I write down what I have learned about baking at home, one
                   loaf at a time. Everything here is written slowly and revised often, so
                   nothing is ever quite finished, and most of it is wrong in some way I have
                   not noticed yet. If you came here for a recipe you can follow in an
                   afternoon, the archive below is not going to help you very much, and I would
                   rather say so at the top than have you find it out four paragraphs down.</p>
                   <ul>{}</ul></main><footer><p>Written by hand, published from a laptop on a
                   kitchen table. There is no newsletter, no tracking and no comment section,
                   which suits everyone involved rather well.</p></footer></body></html>"#,
                r#"<li><a href="/p">Keeping a sourdough starter alive through a cold winter</a></li>"#
                    .repeat(12)
            ),
        )]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.articles_extracted, 0);
        assert_eq!(run.extractions_refused, 1);
        let url = CanonicalUrl::parse("https://example.com/").expect("valid url");
        let captures = archive.list_captures(&url).expect("captures are listed");
        let refused = archive
            .read_refused_extraction(&url, &captures[0])
            .expect("the refusal is stored")
            .expect("a refused page has a record");

        // The record describes the prose that was refused, which is what makes the file worth
        // keeping. Repeating the comparison the rule already made to reach this arm would
        // assert nothing.
        assert!(
            refused
                .excerpt
                .as_deref()
                .is_some_and(|excerpt| excerpt.contains("Written by hand")),
            "{refused:?}"
        );
        assert_eq!(
            archive
                .read_article(&url, &captures[0])
                .expect("no prose is not an error"),
            None
        );
    }

    /// Most of what a crawl answers is navigation. A capture with no prose in it is the
    /// ordinary case, and writing an empty article for each would bury the ones worth having.
    #[test]
    fn a_capture_with_no_prose_in_it_gets_no_article_and_no_complaint() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![page(
            "https://example.com/index",
            200,
            r#"<html><head><title>Index</title></head><body><ul>
               <li><a href="/a">one</a></li><li><a href="/b">two</a></li>
               <li><a href="/c">three</a></li></ul></body></html>"#,
        )]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert_eq!(run.articles_extracted, 0);
        assert!(run.unreadable_articles.is_empty());
        let url = CanonicalUrl::parse("https://example.com/index").expect("valid url");
        let captures = archive.list_captures(&url).expect("captures are listed");
        assert_eq!(
            archive
                .read_article(&url, &captures[0])
                .expect("no prose is not an error"),
            None
        );
    }

    /// The archive keeps whatever answered, and most of what answers a crawl is not a page.
    /// A capture with nothing to read is the ordinary case, not a failure to record.
    #[test]
    fn a_capture_that_is_not_a_page_gets_no_reading_and_no_complaint() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut event = page(
            "https://example.com/logo.png",
            200,
            "<html>not markup</html>",
        );
        response_of(&mut event).headers = vec![Header {
            name: "content-type".to_owned(),
            value: "image/png".to_owned(),
        }];
        let engine = ScriptedCrawlEngine::new(vec![event]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert!(run.unreadable_pages.is_empty());
        let url = CanonicalUrl::parse("https://example.com/logo.png").expect("valid url");
        let captures = archive.list_captures(&url).expect("captures are listed");
        assert_eq!(
            archive
                .read_metadata(&url, &captures[0])
                .expect("no reading is not an error"),
            None
        );
    }

    /// The response is the part that cannot be fetched again, so a page whose encoding the
    /// extractor has to work out is still archived byte for byte, and the reading of it is
    /// what has to cope.
    #[test]
    fn a_page_in_a_legacy_encoding_is_stored_verbatim_and_read_correctly() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let body = b"<html><head><title>caf\xe9</title></head></html>".to_vec();
        let mut event = page("https://example.com/a", 200, "");
        let response = response_of(&mut event);
        response.body = body.clone();
        response.headers = vec![Header {
            name: "content-type".to_owned(),
            value: "text/html; charset=windows-1252".to_owned(),
        }];
        let engine = ScriptedCrawlEngine::new(vec![event]);

        capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        let url = CanonicalUrl::parse("https://example.com/a").expect("valid url");
        let captures = archive.list_captures(&url).expect("captures are listed");
        let capture = archive
            .read_capture(&url, &captures[0])
            .expect("the capture reads back");
        let metadata = archive
            .read_metadata(&url, &captures[0])
            .expect("the reading is stored")
            .expect("a page has a reading");

        assert_eq!(archive.read_body(&capture.body.sha256).expect("body"), body);
        assert_eq!(metadata.title.expect("a title").value, "café");
    }

    #[test]
    fn every_page_the_crawl_produced_becomes_a_capture() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![
            page("https://example.com/a", 200, "<html>a</html>"),
            page("https://example.com/b", 200, "<html>b</html>"),
        ]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
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

    /// A response the host refused is not an item: it is not written by `write_capture`, and
    /// it creates no directory under `items/`. What survives it is the address, in
    /// `refused_responses`, which is what the archive's owed record is built from.
    #[test]
    fn a_page_the_host_refused_is_not_archived_as_an_item() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![page("https://example.com/gone", 404, "")]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 0);
        assert_eq!(run.responses_refused, BTreeMap::from([(404, 1)]));
        assert_eq!(
            run.refused_responses,
            vec![RefusedResponse {
                url: "https://example.com/gone".to_owned(),
                status: 404,
                retry_after: None,
            }]
        );
        let url = CanonicalUrl::parse("https://example.com/gone").expect("valid url");
        assert_eq!(
            archive.list_captures(&url).expect("captures are listed"),
            Vec::new(),
            "a refused response leaves no capture behind"
        );
    }

    /// The header a refused response carries is read off it and kept, and its absence is
    /// kept just as deliberately: a caller deciding whether to wait needs to tell "the host
    /// said nothing" from "the host said now".
    #[test]
    fn a_retry_after_header_on_a_refused_response_reaches_the_record() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut refused = page("https://example.com/slow-down", 429, "Too Many Requests");
        response_of(&mut refused).headers.push(Header {
            name: "Retry-After".to_owned(),
            value: "120".to_owned(),
        });
        let engine = ScriptedCrawlEngine::new(vec![refused]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(
            run.refused_responses,
            vec![RefusedResponse {
                url: "https://example.com/slow-down".to_owned(),
                status: 429,
                retry_after: Some("120".to_owned()),
            }]
        );
    }

    /// Every reason `CaptureRun` can leave an address owed for, folded into the shape the
    /// archive keeps outside its collection and written and read back once.
    /// `owed_addresses`'s own destructure is exhaustive and would refuse to compile if a
    /// loss field were left out of it; this test is the belt beside that brace, proving the
    /// six variants that exist today actually reach the record rather than merely that a
    /// seventh could not be forgotten silently.
    #[test]
    fn a_run_s_owed_addresses_round_trip_through_the_archive_with_every_reason() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut run = CaptureRun::default();
        run.refused_responses.push(RefusedResponse {
            url: "https://example.com/refused".to_owned(),
            status: 429,
            retry_after: Some("120".to_owned()),
        });
        run.refused_responses.push(RefusedResponse {
            url: "https://example.com/refused-quietly".to_owned(),
            status: 404,
            retry_after: None,
        });
        run.failed_fetches.push(FetchFailure {
            url: "https://example.com/timed-out".to_owned(),
            reason: "error sending request: operation timed out".to_owned(),
        });
        run.unaddressable_pages.push(UnaddressablePage {
            url: "ftp://example.com/file".to_owned(),
            reason: InvalidCanonicalUrl::UnsupportedScheme {
                url: "ftp://example.com/file".to_owned(),
                scheme: "ftp".to_owned(),
            },
        });
        run.pages_inside_a_network
            .push("http://169.254.169.254/".to_owned());
        run.links_never_followed
            .push("https://example.com/never-fetched".to_owned());

        let addresses = owed_addresses(&run);
        assert_eq!(addresses.len(), 6, "every reason variant is present");

        archive
            .write_owed(&run.archived_urls, &addresses)
            .expect("the owed record is written");
        assert_eq!(
            archive.read_owed().expect("the owed record reads back"),
            addresses
        );
    }

    /// A redirect collision, two different addresses this run asked for that both landed on
    /// the same final URL and were both refused, is one shape that reaches `owed_addresses`
    /// with the same URL twice; a run whose crawl and sitemap phases both met an address
    /// `already_filed` failed to catch is another. Either way the record should not claim
    /// the archive is owed the same page once for every path that led to it.
    #[test]
    fn the_same_address_owed_twice_is_recorded_once() {
        let mut run = CaptureRun::default();
        run.refused_responses.push(RefusedResponse {
            url: "https://example.com/limited".to_owned(),
            status: 429,
            retry_after: Some("30".to_owned()),
        });
        run.refused_responses.push(RefusedResponse {
            url: "https://example.com/limited".to_owned(),
            status: 429,
            retry_after: None,
        });

        let addresses = owed_addresses(&run);

        assert_eq!(
            addresses,
            vec![OwedAddress {
                url: "https://example.com/limited".to_owned(),
                reason: OwedReason::Refused {
                    status: 429,
                    retry_after: Some("30".to_owned()),
                },
            }],
            "the first entry for an address is the one kept"
        );
    }

    #[test]
    fn two_spellings_of_one_page_land_on_one_item() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![
            page("https://www.example.com/a", 200, "<html>a</html>"),
            page("https://example.com/a?utm_source=x", 200, "<html>a</html>"),
        ]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
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
        // The first write created the item; the second landed on the one it just made, which
        // is a further capture by the same definition as one landing on a run that ran
        // yesterday.
        assert_eq!(run.items_appended, 1);
    }

    /// Two different pages captured in the same run are two new items, neither of which is a
    /// further capture of anything.
    #[test]
    fn distinct_items_captured_in_one_run_are_not_counted_as_appended() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![
            page("https://example.com/a", 200, "<html>a</html>"),
            page("https://example.com/b", 200, "<html>b</html>"),
        ]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.items_appended, 0);
    }

    /// The case the bead exists for: a run into an archive a previous run already filled.
    /// Nothing here refuses or skips the second capture; the run just has to say it happened.
    #[test]
    fn a_run_into_an_archive_that_already_holds_the_item_counts_it_as_appended() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let seed = Seed::new("https://example.com/");

        let first_engine =
            ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, "<html>a</html>")]);
        let first =
            capture_with_no_rules(&first_engine, &archive, &seed).expect("the first run completes");
        assert_eq!(
            first.items_appended, 0,
            "a brand new item is not a further capture"
        );

        let second_engine = ScriptedCrawlEngine::new(vec![page(
            "https://example.com/a",
            200,
            "<html>a, again</html>",
        )]);
        let second = capture_with_no_rules(&second_engine, &archive, &seed)
            .expect("the second run completes");

        assert_eq!(second.captures_written, 1);
        assert_eq!(
            second.items_appended, 1,
            "the second run found the item the first one left behind"
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

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert_eq!(run.unaddressable_pages.len(), 1);
        assert_eq!(run.unaddressable_pages[0].url, "ftp://example.com/a");
        assert_eq!(engine.pages_offered(), 2);
    }

    /// The page arrives with two URLs because that is the only way it can exist: the seed
    /// guard refused this address before the crawl started, so a page wearing it got there
    /// by a redirect the engine followed.
    fn page_redirected_inside_a_network(final_url: &str) -> PageEvent {
        let mut event = page("https://example.com/a", 200, "<html>internal</html>");
        response_of(&mut event).final_url = final_url.to_owned();
        event
    }

    #[test]
    fn a_page_that_ended_inside_a_network_is_refused_and_the_crawl_goes_on() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![
            page_redirected_inside_a_network("http://0.0.0.1/"),
            page("https://example.com/b", 200, "<html>b</html>"),
        ]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert_eq!(run.pages_inside_a_network, vec!["http://0.0.0.1/"]);
        assert_eq!(engine.pages_offered(), 2);
        let url = CanonicalUrl::parse("http://0.0.0.1/").expect("valid url");
        assert_eq!(
            archive
                .list_captures(&url)
                .expect("captures are listed")
                .len(),
            0,
            "the response from inside the network reached the archive"
        );
    }

    /// The refusal above is a guard against an address the run never asked for, so a run
    /// that did ask has to keep working. Archiving a locally served site is the whole
    /// purpose of the flag, and it is also the only way the real fetch path is exercised.
    #[test]
    fn a_run_that_asked_for_local_addresses_still_archives_them() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine =
            ScriptedCrawlEngine::new(vec![page_redirected_inside_a_network("http://127.0.0.1/")]);
        let mut seed = Seed::new("http://127.0.0.1/");
        seed.allow_private_addresses = true;

        let run = capture_with_no_rules(&engine, &archive, &seed).expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert!(run.pages_inside_a_network.is_empty());
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

        let error = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect_err("the write fails");

        assert!(matches!(error, CaptureError::Storage { .. }));
        assert_eq!(engine.pages_offered(), 1);
    }

    #[test]
    fn a_run_cut_short_by_a_failed_write_still_reports_what_it_did() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![
            page("ftp://example.com/a", 200, "<html>a</html>"),
            page("https://example.com/b", 200, "<html>b</html>"),
        ]);
        // Blocked only after the archive exists, so the first page is refused for its
        // address and the second one for the disk.
        std::fs::write(dir.path().join("items"), b"not a directory")
            .expect("the write target is blocked");

        let error = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect_err("the write fails");

        match error {
            CaptureError::Storage { run, .. } => {
                assert_eq!(run.unaddressable_pages.len(), 1);
                assert_eq!(run.captures_written, 0);
            }
            other => panic!("expected a storage failure, got {other:?}"),
        }
    }

    #[test]
    fn a_url_no_server_answered_is_reported_and_never_archived() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![
            PageEvent::NoResponse(FetchFailure {
                url: "https://example.com/unreachable".to_owned(),
                reason: "error sending request: dns error".to_owned(),
            }),
            page("https://example.com/b", 200, "<html>b</html>"),
        ]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert_eq!(run.failed_fetches.len(), 1);
        assert_eq!(run.failed_fetches[0].url, "https://example.com/unreachable");
        let url = CanonicalUrl::parse("https://example.com/unreachable").expect("valid url");
        assert!(
            archive
                .list_captures(&url)
                .expect("captures are listed")
                .is_empty(),
            "a fetch that reached no server left a record behind"
        );
    }

    #[test]
    fn pages_the_engine_lost_are_carried_into_the_report() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut engine =
            ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, "<html>a</html>")]);
        engine.outcome.pages_dropped = 3;

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert_eq!(run.pages_dropped, 3);
    }

    /// The engine's frontier can lose a link before ever fetching it, which is a different
    /// shape from the one above: nothing was spent on it, and the only trace of it is the
    /// engine saying so through the outcome.
    #[test]
    fn links_the_engine_never_followed_are_carried_into_the_report() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut engine =
            ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, "<html>a</html>")]);
        engine.outcome.links_never_followed = vec!["https://example.com/b".to_owned()];

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert_eq!(
            run.links_never_followed,
            vec!["https://example.com/b".to_owned()]
        );
    }

    /// The engine here replays a list and knows nothing about a deadline, which is exactly
    /// the case this guard is for: an engine that ignores the field is stopped from above.
    /// A budget of zero has a margin of zero, so the guard is armed on the first page.
    #[test]
    fn a_seed_out_of_budget_stops_after_the_page_it_is_holding() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![
            page("https://example.com/a", 200, "<html>a</html>"),
            page("https://example.com/b", 200, "<html>b</html>"),
            page("https://example.com/c", 200, "<html>c</html>"),
        ]);
        let mut seed = Seed::new("https://example.com/");
        seed.deadline = Some(Duration::ZERO);

        let run = capture_with_no_rules(&engine, &archive, &seed).expect("the run completes");

        assert_eq!(engine.pages_offered(), 1);
        assert_eq!(
            run.captures_written, 1,
            "the page in hand was fetched already and should not be thrown away"
        );
        assert_eq!(run.stopped, CrawlStop::DeadlineReached);
    }

    #[test]
    fn a_run_inside_its_budget_says_it_ran_out_of_pages_and_not_of_time() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine =
            ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, "<html>a</html>")]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert_eq!(run.stopped, CrawlStop::Exhausted);
    }

    /// An engine that honors the deadline is mid-handover when the budget expires, passing
    /// up the pages it had already fetched. Cutting it on the instant would lose exactly the
    /// pages the deadline was careful to keep, so the guard above it has to arrive later.
    #[test]
    fn an_engine_still_handing_over_at_its_deadline_is_left_to_finish() {
        let budget = Duration::from_secs(300);

        assert!(!engine_overran_its_deadline(Some(budget), budget));
        assert!(!engine_overran_its_deadline(
            Some(budget),
            budget + Duration::from_secs(29)
        ));
    }

    #[test]
    fn an_engine_fetching_well_past_the_budget_is_cut_from_above() {
        let budget = Duration::from_secs(300);

        assert!(engine_overran_its_deadline(
            Some(budget),
            budget + Duration::from_secs(31)
        ));
    }

    #[test]
    fn a_seed_that_asked_for_no_deadline_is_never_cut_from_above() {
        assert!(!engine_overran_its_deadline(
            None,
            Duration::from_secs(86_400)
        ));
    }

    /// The engine has its own reach on the deadline: it can end a crawl that produced no
    /// page at all, which is a stop nothing above it would ever see happen.
    #[test]
    fn a_crawl_the_engine_cut_short_says_so_in_the_run() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut engine =
            ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, "<html>a</html>")]);
        engine.outcome.stopped = CrawlStop::DeadlineReached;

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.stopped, CrawlStop::DeadlineReached);
    }

    /// The reason the flag exists rather than being inferred: a body cut short still parses
    /// and still arrives under a status that promises the whole page, so the only place the
    /// shortfall can be seen is a record that says so.
    #[test]
    fn a_page_that_arrived_short_is_archived_saying_so() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut cut_short = page("https://example.com/a", 200, "<html>a");
        response_of(&mut cut_short).body_truncated = true;
        let engine = ScriptedCrawlEngine::new(vec![cut_short]);

        capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        let url = CanonicalUrl::parse("https://example.com/a").expect("valid url");
        let captures = archive.list_captures(&url).expect("captures are listed");
        let capture = archive
            .read_capture(&url, &captures[0])
            .expect("the capture reads back");
        assert!(capture.body_truncated);
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
        response_of(&mut redirected).requested_url = "https://example.com/short-link".to_owned();
        let engine = ScriptedCrawlEngine::new(vec![redirected]);

        capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        let url = CanonicalUrl::parse("https://example.com/final").expect("valid url");
        let captures = archive.list_captures(&url).expect("captures are listed");
        let capture = archive
            .read_capture(&url, &captures[0])
            .expect("the capture reads back");
        assert_eq!(capture.requested_url, "https://example.com/short-link");
        assert_eq!(capture.final_url, "https://example.com/final");
    }

    const STYLED_PAGE: &str = r#"<html><head><link rel="stylesheet" href="/style.css"></head>
           <body><img src="/logo.png"></body></html>"#;

    fn subresource(url: &str, media_type: &str, body: &[u8]) -> PageEvent {
        PageEvent::Response(PageResponse {
            requested_url: url.to_owned(),
            final_url: url.to_owned(),
            status: 200,
            headers: vec![Header {
                name: "content-type".to_owned(),
                value: media_type.to_owned(),
            }],
            body: body.to_vec(),
            body_truncated: false,
            fetched_at: "2026-07-25T14:03:22Z".parse().expect("valid timestamp"),
        })
    }

    fn only_capture_of(archive: &Archive, url: &str) -> crate::storage::Capture {
        let url = CanonicalUrl::parse(url).expect("valid url");
        let captures = archive.list_captures(&url).expect("captures are listed");
        archive
            .read_capture(&url, &captures[0])
            .expect("the capture reads back")
    }

    #[test]
    fn a_page_is_archived_with_the_files_it_needs_to_still_render() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine =
            ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, STYLED_PAGE)])
                .serving(vec![
                    subresource("https://example.com/style.css", "text/CSS", b"body{}"),
                    subresource("https://example.com/logo.png", "image/png", b"\x89PNG"),
                ]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.assets_stored, 2);
        assert_eq!(run.assets_missed, 0);
        assert_eq!(run.asset_fetches, 2);
        let capture = only_capture_of(&archive, "https://example.com/a");
        assert!(capture.assets_missed.is_empty());
        let stored: Vec<(&str, Option<&str>)> = capture
            .assets
            .iter()
            .map(|asset| (asset.final_url.as_str(), asset.media_type.as_deref()))
            .collect();
        assert_eq!(
            stored,
            [
                ("https://example.com/style.css", Some("text/css")),
                ("https://example.com/logo.png", Some("image/png")),
            ]
        );
        assert_eq!(
            archive
                .read_body(&capture.assets[0].body.sha256)
                .expect("the stylesheet reads back"),
            b"body{}"
        );
    }

    /// The reason the pass remembers anything. A stylesheet belongs to every page of a site
    /// that links it, and asking the server for it once per page is two hundred requests for
    /// one file, which is not something an archive gets to do to somebody else's host.
    #[test]
    fn a_file_two_pages_share_is_asked_for_once_and_stored_once() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![
            page(
                "https://example.com/a",
                200,
                r#"<html><link rel="stylesheet" href="/style.css"></html>"#,
            ),
            page(
                "https://example.com/b",
                200,
                r#"<html><link rel="stylesheet" href="/style.css"></html>"#,
            ),
        ])
        .serving(vec![subresource(
            "https://example.com/style.css",
            "text/css",
            b"body{}",
        )]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 2);
        assert_eq!(run.assets_stored, 2, "both captures reference the file");
        assert_eq!(
            engine.urls_fetched(),
            ["https://example.com/style.css"],
            "the shared file was asked for more than once"
        );
        assert_eq!(run.asset_fetches, 1);
        let first = only_capture_of(&archive, "https://example.com/a");
        let second = only_capture_of(&archive, "https://example.com/b");
        assert_eq!(first.assets[0].body.sha256, second.assets[0].body.sha256);
    }

    /// A subresource that is gone is gone for every page of the site that references it, so
    /// the answer is remembered exactly like a successful one. A run that asked again per page
    /// would spend a request each time to be told the same thing.
    #[test]
    fn a_subresource_no_server_answered_is_recorded_and_asked_for_only_once() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let referencing = r#"<html><link rel="stylesheet" href="/gone.css"></html>"#;
        let engine = ScriptedCrawlEngine::new(vec![
            page("https://example.com/a", 200, referencing),
            page("https://example.com/b", 200, referencing),
        ]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 2, "the pages are archived regardless");
        assert_eq!(run.assets_stored, 0);
        assert_eq!(run.assets_missed, 2);
        assert_eq!(run.asset_fetches, 1);
        let capture = only_capture_of(&archive, "https://example.com/a");
        assert_eq!(capture.assets_missed.len(), 1);
        assert_eq!(capture.assets_missed[0].url, "https://example.com/gone.css");
        assert!(matches!(
            capture.assets_missed[0].reason,
            AssetMiss::NoResponse { .. }
        ));
    }

    /// Half a stylesheet is not a stylesheet, and a subresource record has nowhere to say the
    /// bytes are partial, so it is refused rather than stored as though it were whole. A page
    /// in the same state is kept, because a page cut short is still the page.
    #[test]
    fn a_subresource_that_arrived_short_is_not_stored_as_if_it_were_whole() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut cut_short = subresource("https://example.com/style.css", "text/css", b"body{col");
        response_of(&mut cut_short).body_truncated = true;
        let engine =
            ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, STYLED_PAGE)])
                .serving(vec![cut_short]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.assets_stored, 0, "the logo was never served either");
        let capture = only_capture_of(&archive, "https://example.com/a");
        let short = capture
            .assets_missed
            .iter()
            .find(|missed| missed.url == "https://example.com/style.css")
            .expect("the stylesheet is reported");
        assert_eq!(short.reason, AssetMiss::ArrivedShort { byte_len: 8 });
    }

    /// The same guard the pages get, one hop further in. A public page whose stylesheet
    /// redirects into the machine the archive runs on is the same request as a seed pointed
    /// there, and the durable half of the harm is the bytes that would be written.
    #[test]
    fn a_subresource_that_ended_inside_a_network_is_refused() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut redirected = subresource("https://example.com/style.css", "text/css", b"body{}");
        response_of(&mut redirected).final_url = "http://169.254.169.254/latest/".to_owned();
        let engine =
            ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, STYLED_PAGE)])
                .serving(vec![redirected]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert_eq!(run.assets_stored, 0);
        let capture = only_capture_of(&archive, "https://example.com/a");
        let refused = capture
            .assets_missed
            .iter()
            .find(|missed| missed.url == "https://example.com/style.css")
            .expect("the stylesheet is reported");
        assert_eq!(refused.reason, AssetMiss::InsideANetwork);
    }

    /// The other side of that guard: archiving a site served locally is the whole reason the
    /// flag exists, and a local site's stylesheet is on the same local address.
    #[test]
    fn a_run_that_asked_for_local_addresses_still_gets_their_subresources() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut local = page("http://127.0.0.1:8000/a", 200, STYLED_PAGE);
        response_of(&mut local).final_url = "http://127.0.0.1:8000/a".to_owned();
        let engine = ScriptedCrawlEngine::new(vec![local]).serving(vec![subresource(
            "http://127.0.0.1:8000/style.css",
            "text/css",
            b"body{}",
        )]);
        let mut seed = Seed::new("http://127.0.0.1:8000/");
        seed.allow_private_addresses = true;

        let run = capture_with_no_rules(&engine, &archive, &seed).expect("the run completes");

        assert_eq!(run.assets_stored, 1);
        let capture = only_capture_of(&archive, "http://127.0.0.1:8000/a");
        assert_eq!(
            capture.assets[0].final_url,
            "http://127.0.0.1:8000/style.css"
        );
    }

    /// A run out of time stops asking. The page is archived either way, and a subresource is
    /// the cheapest thing in a run to give up on, so the budget is read plainly here rather
    /// than with the margin the backstop over the engine allows itself.
    #[test]
    fn a_run_out_of_time_stops_asking_for_subresources_and_says_so() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine =
            ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, STYLED_PAGE)])
                .serving(vec![subresource(
                    "https://example.com/style.css",
                    "text/css",
                    b"body{}",
                )]);
        let mut seed = Seed::new("https://example.com/");
        seed.deadline = Some(Duration::ZERO);

        let run = capture_with_no_rules(&engine, &archive, &seed).expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert_eq!(
            run.asset_fetches, 0,
            "the run asked for a file it had no time for"
        );
        let capture = only_capture_of(&archive, "https://example.com/a");
        assert_eq!(capture.assets_missed.len(), 2);
        assert!(
            capture
                .assets_missed
                .iter()
                .all(|missed| missed.reason == AssetMiss::DeadlineReached),
            "{:#?}",
            capture.assets_missed
        );
    }

    /// A capture that is not a page references nothing, and neither does one whose markup the
    /// extractor could not read: the references live in the part it failed on.
    #[test]
    fn a_capture_with_no_reading_of_it_asks_for_nothing() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut image = page("https://example.com/logo.png", 200, "not markup");
        response_of(&mut image).headers = vec![Header {
            name: "content-type".to_owned(),
            value: "image/png".to_owned(),
        }];
        let engine = ScriptedCrawlEngine::new(vec![image]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert_eq!(run.asset_fetches, 0);
        assert_eq!(run.assets_missed, 0);
    }

    /// A page that references more files than a capture holds is a page the ceiling was
    /// written for, and the tail of it is recorded rather than dropped quietly. Nothing past
    /// the ceiling is asked for: refusing after fetching would spend the requests anyway.
    #[test]
    fn a_page_referencing_more_files_than_a_capture_holds_records_the_tail() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let references = 130;
        let mut markup = String::from("<html><body>");
        let mut served = Vec::new();
        for index in 0..references {
            markup.push_str(&format!(r#"<img src="/{index}.png">"#));
            served.push(subresource(
                &format!("https://example.com/{index}.png"),
                "image/png",
                format!("{index}").as_bytes(),
            ));
        }
        markup.push_str("</body></html>");
        let engine = ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, &markup)])
            .serving(served);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        let capture = only_capture_of(&archive, "https://example.com/a");
        assert_eq!(capture.assets.len(), 128);
        assert_eq!(capture.assets_missed.len(), references - 128);
        assert_eq!(run.asset_fetches, 128, "a refused file was fetched anyway");
        assert!(
            capture
                .assets_missed
                .iter()
                .all(|missed| missed.reason == AssetMiss::CountCeilingReached)
        );
    }

    /// A page referencing a host that accepts connections and says nothing is what holds the
    /// whole pipeline: the pass runs inside the callback the crawl hands pages through, so every
    /// request it waits out is time the engine spends fetching pages nothing is reading. After
    /// enough silence the capture stops asking, and what it did not ask for says so.
    #[test]
    fn a_capture_stops_asking_once_nothing_is_answering() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut markup = String::from("<html><body>");
        for index in 0..6 {
            markup.push_str(&format!(r#"<img src="/{index}.png">"#));
        }
        markup.push_str("</body></html>");
        // The last reference would have answered, and the point is that it is never asked.
        let engine = ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, &markup)])
            .serving(vec![subresource(
                "https://example.com/5.png",
                "image/png",
                b"\x89PNG",
            )]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.captures_written, 1);
        assert_eq!(run.assets_stored, 0);
        assert_eq!(
            run.asset_fetches, 3,
            "the capture kept waiting on a host that was not answering"
        );
        let capture = only_capture_of(&archive, "https://example.com/a");
        let reasons: Vec<&AssetMiss> = capture
            .assets_missed
            .iter()
            .map(|missed| &missed.reason)
            .collect();
        assert!(
            matches!(
                reasons.as_slice(),
                [
                    AssetMiss::NoResponse { .. },
                    AssetMiss::NoResponse { .. },
                    AssetMiss::NoResponse { .. },
                    AssetMiss::NothingWasAnswering,
                    AssetMiss::NothingWasAnswering,
                    AssetMiss::NothingWasAnswering,
                ]
            ),
            "{reasons:#?}"
        );
    }

    /// Silence has to be consecutive to mean anything. A host that answers between two failures
    /// is answering, and a page that references a couple of files that are gone is an ordinary
    /// page rather than a reason to stop capturing it.
    #[test]
    fn a_host_that_answers_between_failures_is_not_taken_for_a_dead_one() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![page(
            "https://example.com/a",
            200,
            r#"<html><body><img src="/gone-1.png"><img src="/gone-2.png"><img src="/here.png">
               <img src="/gone-3.png"><img src="/gone-4.png"><img src="/also-here.png"></body></html>"#,
        )])
        .serving(vec![
            subresource("https://example.com/here.png", "image/png", b"\x89PNG1"),
            subresource("https://example.com/also-here.png", "image/png", b"\x89PNG2"),
        ]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.assets_stored, 2);
        assert_eq!(run.asset_fetches, 6, "a reference was never asked for");
        let capture = only_capture_of(&archive, "https://example.com/a");
        assert!(
            capture
                .assets_missed
                .iter()
                .all(|missed| matches!(missed.reason, AssetMiss::NoResponse { .. })),
            "{:#?}",
            capture.assets_missed
        );
    }

    /// A refusal that never became a request cost no wait, so it is not silence. Counting it
    /// would let a page full of addresses this archive will not dial stop it from capturing the
    /// files that are perfectly reachable further down the same page.
    #[test]
    fn a_reference_refused_before_being_dialled_is_not_counted_as_silence() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![page(
            "https://example.com/a",
            200,
            r#"<html><body><img src="http://127.0.0.1/1.png"><img src="http://10.0.0.1/2.png">
               <img src="http://[::1]/3.png"><img src="http://169.254.169.254/4.png">
               <img src="/here.png"></body></html>"#,
        )])
        .serving(vec![subresource(
            "https://example.com/here.png",
            "image/png",
            b"\x89PNG",
        )]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.assets_stored, 1);
        assert_eq!(run.asset_fetches, 1);
        assert_eq!(run.assets_missed, 4);
    }

    /// The ceiling on bytes counts what came over the wire, not what was kept, and this is the
    /// page that tells the two apart: every file on it is just over the size one subresource
    /// may spend, so nothing is ever stored. A ceiling that counted stored bytes would sit at
    /// zero forever while the run transferred a gigabyte per page.
    #[test]
    fn a_page_of_oversized_files_stops_costing_transfers_at_the_ceiling() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let oversized = vec![b'x'; 8 * 1024 * 1024 + 1];
        let mut markup = String::from("<html><body>");
        let mut served = Vec::new();
        for index in 0..5 {
            markup.push_str(&format!(r#"<img src="/{index}.png">"#));
            served.push(subresource(
                &format!("https://example.com/{index}.png"),
                "image/png",
                &oversized,
            ));
        }
        markup.push_str("</body></html>");
        let engine = ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, &markup)])
            .serving(served);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.assets_stored, 0);
        // Four transfers of eight megabytes crosses the thirty-two the capture may spend, and
        // the overshoot is one file rather than every remaining reference.
        assert_eq!(
            run.asset_fetches, 4,
            "a refused transfer was not charged to the capture"
        );
        let capture = only_capture_of(&archive, "https://example.com/a");
        let reasons: Vec<&AssetMiss> = capture
            .assets_missed
            .iter()
            .map(|missed| &missed.reason)
            .collect();
        assert_eq!(
            reasons,
            [
                &AssetMiss::TooLarge {
                    byte_len: 8 * 1024 * 1024 + 1
                },
                &AssetMiss::TooLarge {
                    byte_len: 8 * 1024 * 1024 + 1
                },
                &AssetMiss::TooLarge {
                    byte_len: 8 * 1024 * 1024 + 1
                },
                &AssetMiss::TooLarge {
                    byte_len: 8 * 1024 * 1024 + 1
                },
                &AssetMiss::ByteCeilingReached,
            ]
        );
    }

    /// An address the archive can judge on its own is refused without a request. Asking the
    /// engine would spend a call that never leaves the machine and come back as a reason to be
    /// read out of an error string.
    #[test]
    fn a_reference_pointing_inside_a_network_is_refused_without_being_asked_for() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![page(
            "https://example.com/a",
            200,
            r#"<html><link rel="stylesheet" href="http://169.254.169.254/latest/"></html>"#,
        )]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.asset_fetches, 0);
        assert!(engine.urls_fetched().is_empty());
        let capture = only_capture_of(&archive, "https://example.com/a");
        assert_eq!(capture.assets_missed[0].reason, AssetMiss::InsideANetwork);
    }

    /// The shape an archiver is aimed at something with: one page, a thousand addresses on a
    /// host somebody else picked, none of them answering. It is also where all three bounds meet,
    /// so what each one refused is asserted rather than only the total.
    #[test]
    fn a_page_referencing_a_thousand_dead_addresses_costs_three_requests() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut markup = String::from("<html><body>");
        for index in 0..1_000 {
            markup.push_str(&format!(
                r#"<img src="https://victim.example/{index}.png">"#
            ));
        }
        markup.push_str("</body></html>");
        let engine = ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, &markup)]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert_eq!(run.assets_stored, 0);
        assert_eq!(
            run.asset_fetches, 3,
            "the run kept asking a host that had answered nothing three times over"
        );
        let capture = only_capture_of(&archive, "https://example.com/a");
        let counted = |wanted: &AssetMiss| {
            capture
                .assets_missed
                .iter()
                .filter(|missed| {
                    std::mem::discriminant(&missed.reason) == std::mem::discriminant(wanted)
                })
                .count()
        };
        // Three requests, then the capture stops asking, and the count ceiling takes over from
        // there: it is what keeps the record itself from growing with a page's ambitions.
        assert_eq!(
            counted(&AssetMiss::NoResponse {
                detail: String::new()
            }),
            3
        );
        assert_eq!(counted(&AssetMiss::NothingWasAnswering), 125);
        assert_eq!(counted(&AssetMiss::CountCeilingReached), 872);
        assert_eq!(capture.assets_missed.len(), 1_000);
    }
    /// The sitemap phase asks for page after page with nothing between the requests, and it is
    /// the phase that does so hundreds of times: it exists for a site whose pages do not link
    /// one another. `--delay` reached the engine through the crawl, which this phase does not
    /// run, so the flag was documented as the wait between requests and did nothing here.
    ///
    /// Measured against a real publication before this: 250 pages in 114 seconds, and the host
    /// refused 160 of them. The same capture before the subresource pass got cheaper took 22.7
    /// minutes and was refused none, because the serial subresource pass was standing in for a
    /// politeness delay nobody had asked for.
    #[test]
    fn the_sitemap_phase_waits_between_the_pages_it_asks_for() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut seed = Seed::new("https://example.com/");
        seed.delay = Duration::from_millis(60);
        let urls: Vec<String> = (0..3)
            .map(|i| format!("https://example.com/p/{i}"))
            .collect();
        let engine = ScriptedCrawlEngine::new(Vec::new()).serving(
            urls.iter()
                .map(|url| page(url, 200, "<html><body><p>prose</p></body></html>"))
                .collect(),
        );

        let started = Instant::now();
        let run = capture_sitemap(
            &engine,
            &archive,
            &seed,
            &SiteRules::default(),
            &urls,
            false,
            RunSoFar::nothing_yet(&HashSet::new()),
        )
        .expect("a fake engine and a fresh archive do not fail a write");

        assert_eq!(run.captures_written, 3);
        assert_eq!(engine.urls_fetched().len(), 3);
        // Two gaps between three requests. A lower bound, which is the only kind a sleep can
        // be held to: it never returns early, and asserting an upper bound would be asserting
        // that this machine was not busy.
        assert!(
            started.elapsed() >= Duration::from_millis(120),
            "three pages at a 60ms delay wait twice, took {:?}",
            started.elapsed()
        );
    }

    /// The wait is paid for a request and not for a loop iteration. A URL the run already
    /// filed is skipped without asking the host anything, so pacing it would spend the run's
    /// wall clock on politeness towards a request nobody made.
    ///
    /// This one passes with the change reverted, and it is a guard rather than a regression
    /// test: reverting removes all waiting, so what it catches is a future `pace` moved above
    /// the skip, which is the plausible wrong version of this and costs thirty seconds a URL.
    #[test]
    fn a_url_the_run_already_filed_is_skipped_without_waiting_for_it() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut seed = Seed::new("https://example.com/");
        seed.delay = Duration::from_secs(30);
        let urls: Vec<String> = (0..3)
            .map(|i| format!("https://example.com/p/{i}"))
            .collect();
        let engine = ScriptedCrawlEngine::new(Vec::new()).serving(vec![page(
            &urls[2],
            200,
            "<html><body><p>prose</p></body></html>",
        )]);
        let already: HashSet<String> = urls[..2].iter().cloned().collect();

        let started = Instant::now();
        let run = capture_sitemap(
            &engine,
            &archive,
            &seed,
            &SiteRules::default(),
            &urls,
            false,
            RunSoFar::nothing_yet(&already),
        )
        .expect("a fake engine and a fresh archive do not fail a write");

        assert_eq!(run.captures_written, 1);
        assert_eq!(engine.urls_fetched().len(), 1);
        // One request, so no gap to pace. At a thirty second delay, paying for the two skips
        // or for the first request is the difference between this and a run nobody waits for.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a single request waits for nothing, took {:?}",
            started.elapsed()
        );
    }

    /// An address the ordinary crawl phase was refused on lives in a different `CaptureRun`
    /// from the sitemap phase's own, so `run.refused_urls` alone cannot stop the sitemap
    /// phase from asking a host that just refused it for the same page again: the address has
    /// to travel through `RunSoFar.archived`, the same way an already filed one already did.
    /// Two wrongs follow if it does not: a second request against a host already rate
    /// limiting this run, and the same address recorded twice in the owed list, once per
    /// phase that met it.
    #[test]
    fn a_url_the_crawl_phase_was_refused_on_is_not_asked_for_again_by_the_sitemap_phase() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let seed = Seed::new("https://example.com/refused");
        let crawl_engine = ScriptedCrawlEngine::new(vec![page(
            "https://example.com/refused",
            429,
            "Too Many Requests",
        )]);

        let crawl_run = capture_with_no_rules(&crawl_engine, &archive, &seed)
            .expect("a fake engine and a fresh archive do not fail a write");
        assert_eq!(crawl_run.captures_written, 0, "the seed page was refused");

        let urls = vec![
            "https://example.com/refused".to_owned(),
            "https://example.com/other".to_owned(),
        ];
        let sitemap_engine = ScriptedCrawlEngine::new(Vec::new()).serving(vec![page(
            "https://example.com/other",
            200,
            "<html><body><p>ok</p></body></html>",
        )]);
        // What `src/cli/capture.rs` builds for the sitemap phase: the union of what the
        // crawl phase filed and what it was refused on.
        let archived: HashSet<String> = crawl_run
            .archived_urls
            .iter()
            .chain(crawl_run.refused_urls.iter())
            .cloned()
            .collect();

        let sitemap_run = capture_sitemap(
            &sitemap_engine,
            &archive,
            &seed,
            &SiteRules::default(),
            &urls,
            false,
            RunSoFar {
                started: Instant::now(),
                pages_written: crawl_run.pages_spent,
                archived: &archived,
            },
        )
        .expect("a fake engine and a fresh archive do not fail a write");

        assert_eq!(
            sitemap_engine.urls_fetched(),
            vec!["https://example.com/other".to_owned()],
            "the already-refused address is not asked for a second time"
        );

        let mut merged = CaptureRun::default();
        merged.merge(crawl_run);
        merged.merge(sitemap_run);

        let owed = owed_addresses(&merged);
        assert_eq!(
            owed.iter()
                .filter(|address| address.url == "https://example.com/refused")
                .count(),
            1,
            "the refused address is recorded once, not once per phase that met it"
        );
    }

    /// A response that is not a success does not become a capture, because an item is a page
    /// that was served. What it must not do is disappear from the run without a trace:
    /// `responses_refused` keeps the count a rate limited crawl needs, and the address
    /// itself survives in `refused_responses`, which is what used to be silently indistinguishable
    /// from a captures_written count that read as a collection larger than the archive held.
    ///
    /// Measured: 160 of 250 pages answered 429 with a seventeen byte body, and the run said
    /// `250 captures, stopped: nothing was left to fetch` and nothing else.
    #[test]
    fn a_capture_the_host_refused_is_counted_apart_from_the_ones_it_served() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![
            page(
                "https://example.com/a",
                200,
                "<html><body><p>a</p></body></html>",
            ),
            page("https://example.com/b", 429, "Too Many Requests"),
            page("https://example.com/c", 429, "Too Many Requests"),
            page("https://example.com/d", 503, "unavailable"),
        ]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("a fake engine and a fresh archive do not fail a write");

        assert_eq!(
            run.captures_written, 1,
            "only the served page becomes an item"
        );
        assert_eq!(
            run.responses_refused,
            BTreeMap::from([(429, 2), (503, 1)]),
            "a run has to be able to say the host refused it"
        );
        assert_eq!(
            run.refused_responses
                .iter()
                .map(|refused| (refused.url.as_str(), refused.status))
                .collect::<Vec<_>>(),
            vec![
                ("https://example.com/b", 429),
                ("https://example.com/c", 429),
                ("https://example.com/d", 503),
            ],
            "each refusal keeps the address the count alone cannot carry"
        );
    }
    /// The sitemap phase is where a host refusing a run was actually seen, and its counts reach
    /// the report through `merge` rather than directly. A count that survived the crawl path
    /// and was dropped on the way out of this one would report the same silence the row exists
    /// to break.
    #[test]
    fn a_refusal_during_the_sitemap_phase_survives_into_the_run_it_is_merged_into() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let seed = Seed::new("https://example.com/");
        let urls: Vec<String> = (0..3)
            .map(|i| format!("https://example.com/p/{i}"))
            .collect();
        let engine = ScriptedCrawlEngine::new(Vec::new()).serving(vec![
            page(&urls[0], 200, "<html><body><p>served</p></body></html>"),
            page(&urls[1], 429, "Too Many Requests"),
            page(&urls[2], 429, "Too Many Requests"),
        ]);

        let sitemap_run = capture_sitemap(
            &engine,
            &archive,
            &seed,
            &SiteRules::default(),
            &urls,
            false,
            RunSoFar::nothing_yet(&HashSet::new()),
        )
        .expect("a fake engine and a fresh archive do not fail a write");

        let mut whole_run = CaptureRun::default();
        whole_run.merge(sitemap_run);

        assert_eq!(whole_run.captures_written, 1);
        assert_eq!(whole_run.responses_refused, BTreeMap::from([(429, 2)]));
        assert_eq!(
            whole_run
                .refused_responses
                .iter()
                .map(|refused| refused.url.as_str())
                .collect::<Vec<_>>(),
            vec![urls[1].as_str(), urls[2].as_str()],
            "a phase's refused addresses survive the merge along with its count"
        );
    }

    /// The two bounds this phase answers to are different facts and send an operator to
    /// different flags: a ceiling reached says a larger number takes the rest of the site,
    /// while a clock that ran out says the run needs longer or the host needs asking more
    /// slowly. One boolean stood for both, so a run given no deadline at all still reported
    /// that its deadline ran out.
    #[test]
    fn a_sitemap_phase_stopped_by_the_page_ceiling_does_not_blame_the_deadline() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut seed = Seed::new("https://example.com/");
        seed.max_pages = 2;
        seed.deadline = None;
        let urls: Vec<String> = (0..4)
            .map(|i| format!("https://example.com/p/{i}"))
            .collect();
        let engine = ScriptedCrawlEngine::new(Vec::new()).serving(
            urls.iter()
                .map(|url| page(url, 200, "<html><body><p>prose</p></body></html>"))
                .collect(),
        );

        let run = capture_sitemap(
            &engine,
            &archive,
            &seed,
            &SiteRules::default(),
            &urls,
            false,
            RunSoFar::nothing_yet(&HashSet::new()),
        )
        .expect("a fake engine and a fresh archive do not fail a write");

        assert_eq!(run.captures_written, 2);
        assert_eq!(run.stopped, CrawlStop::PageCeilingReached);
    }

    /// Before this bead a refused response was still a written capture and paid into this
    /// phase's own page ceiling; now that it is not an item at all, the phase has to charge
    /// the ceiling some other way, or a host that refuses everything drains the whole
    /// sitemap listing down to the deadline while `--max-pages` sits there unread.
    #[test]
    fn a_sitemap_phase_over_a_refusing_host_still_stops_at_the_page_ceiling() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut seed = Seed::new("https://example.com/");
        seed.max_pages = 2;
        seed.deadline = None;
        let urls: Vec<String> = (0..4)
            .map(|i| format!("https://example.com/p/{i}"))
            .collect();
        let engine = ScriptedCrawlEngine::new(Vec::new()).serving(
            urls.iter()
                .map(|url| page(url, 429, "Too Many Requests"))
                .collect(),
        );

        let run = capture_sitemap(
            &engine,
            &archive,
            &seed,
            &SiteRules::default(),
            &urls,
            false,
            RunSoFar::nothing_yet(&HashSet::new()),
        )
        .expect("a fake engine and a fresh archive do not fail a write");

        assert_eq!(
            engine.urls_fetched().len(),
            2,
            "the ceiling has to stop the phase from asking for more than it was given"
        );
        assert_eq!(run.captures_written, 0, "every response was refused");
        assert_eq!(run.pages_spent, 2);
        assert_eq!(run.stopped, CrawlStop::PageCeilingReached);
    }

    /// The other half of the same distinction: the phase that really did run out of clock
    /// still says so. Without this, naming the ceiling everywhere would pass the test above
    /// while telling an operator to raise a number that was never the bound.
    #[test]
    fn a_sitemap_phase_stopped_by_its_deadline_still_names_the_clock() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut seed = Seed::new("https://example.com/");
        seed.deadline = Some(Duration::ZERO);
        let urls: Vec<String> = (0..4)
            .map(|i| format!("https://example.com/p/{i}"))
            .collect();
        let engine = ScriptedCrawlEngine::new(Vec::new()).serving(
            urls.iter()
                .map(|url| page(url, 200, "<html><body><p>prose</p></body></html>"))
                .collect(),
        );

        let run = capture_sitemap(
            &engine,
            &archive,
            &seed,
            &SiteRules::default(),
            &urls,
            false,
            RunSoFar::nothing_yet(&HashSet::new()),
        )
        .expect("a fake engine and a fresh archive do not fail a write");

        assert_eq!(
            engine.urls_fetched().len(),
            0,
            "a budget already spent asks the host for nothing"
        );
        assert_eq!(run.stopped, CrawlStop::DeadlineReached);
    }

    /// A ceiling bounds the run, and a run is both phases when `--max-depth` is given beside
    /// `--from-sitemap`. The sitemap phase counted only what it had filed itself, so a run told
    /// to archive fifty pages archived fifty in the crawl and fifty more from the listing, and
    /// reported the ceiling it had been asked for while doubling it.
    #[test]
    fn the_sitemap_phase_spends_only_the_pages_the_crawl_left() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut seed = Seed::new("https://example.com/");
        seed.max_pages = 3;
        seed.deadline = None;
        let urls: Vec<String> = (0..4)
            .map(|i| format!("https://example.com/p/{i}"))
            .collect();
        let engine = ScriptedCrawlEngine::new(Vec::new()).serving(
            urls.iter()
                .map(|url| page(url, 200, "<html><body><p>prose</p></body></html>"))
                .collect(),
        );

        let run = capture_sitemap(
            &engine,
            &archive,
            &seed,
            &SiteRules::default(),
            &urls,
            false,
            RunSoFar {
                started: Instant::now(),
                pages_written: 2,
                archived: &HashSet::new(),
            },
        )
        .expect("a fake engine and a fresh archive do not fail a write");

        assert_eq!(
            run.captures_written, 1,
            "two of the three pages were spent before this phase began"
        );
        assert_eq!(engine.urls_fetched().len(), 1);
        assert_eq!(run.stopped, CrawlStop::PageCeilingReached);
    }

    /// The clock half of the same rule. A phase reading `Instant::now()` for itself starts the
    /// deadline over, so a run given ten minutes spent ten crawling and then ten more on the
    /// listing. What makes this the run's clock and not the phase's is that the instant arrives
    /// from the caller.
    #[test]
    fn the_sitemap_phase_measures_the_deadline_from_when_the_run_began() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut seed = Seed::new("https://example.com/");
        seed.deadline = Some(Duration::from_millis(50));
        let urls: Vec<String> = (0..4)
            .map(|i| format!("https://example.com/p/{i}"))
            .collect();
        let engine = ScriptedCrawlEngine::new(Vec::new()).serving(
            urls.iter()
                .map(|url| page(url, 200, "<html><body><p>prose</p></body></html>"))
                .collect(),
        );

        // A run whose earlier phase already outlasted the deadline. Subtracted rather than
        // slept, so the test costs nothing and cannot flake on a busy machine.
        let began = Instant::now() - Duration::from_millis(200);
        let run = capture_sitemap(
            &engine,
            &archive,
            &seed,
            &SiteRules::default(),
            &urls,
            false,
            RunSoFar {
                started: began,
                pages_written: 0,
                archived: &HashSet::new(),
            },
        )
        .expect("a fake engine and a fresh archive do not fail a write");

        assert_eq!(
            engine.urls_fetched().len(),
            0,
            "the run's deadline had already passed before this phase started"
        );
        assert_eq!(run.stopped, CrawlStop::DeadlineReached);
    }

    /// The two tests above stop at the loop that decides whether to ask for the next URL. This
    /// is the other place the shared budget lands: `--max-depth` makes each listed URL a seed of
    /// its own, and what bounds that sub-crawl is the seed handed to the engine. Reading the
    /// phase's own count there gave every sub-crawl the whole ceiling, so a listed URL that
    /// linked three more archived four pages of a run that had one left.
    #[test]
    fn a_sub_crawl_is_handed_only_the_pages_the_run_has_left() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut seed = Seed::new("https://example.com/");
        seed.max_pages = 3;
        seed.deadline = None;
        let engine = ScriptedCrawlEngine::new(Vec::new());

        capture_sitemap(
            &engine,
            &archive,
            &seed,
            &SiteRules::default(),
            &["https://example.com/listed".to_owned()],
            true,
            RunSoFar {
                started: Instant::now(),
                pages_written: 2,
                archived: &HashSet::new(),
            },
        )
        .expect("a fake engine and a fresh archive do not fail a write");

        let crawled = engine.seeds_crawled();
        assert_eq!(crawled.len(), 1, "the listed URL was crawled once");
        assert_eq!(
            crawled[0].max_pages, 1,
            "two of the run's three pages were spent before this phase began"
        );
    }

    /// The clock half of the seed handed to a sub-crawl. A phase reading `Instant::now()` here
    /// hands each listed URL the whole deadline again, so a run given ten minutes that spent
    /// nine crawling gives its first sub-crawl another ten.
    #[test]
    fn a_sub_crawl_is_handed_only_the_clock_the_run_has_left() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut seed = Seed::new("https://example.com/");
        seed.deadline = Some(Duration::from_secs(10));
        let engine = ScriptedCrawlEngine::new(Vec::new());

        // Nine of the ten seconds are already gone. Subtracted rather than slept: the gap
        // between what is left and what a fresh clock would hand over is a full order of
        // magnitude, so no machine is slow enough to blur the two.
        let began = Instant::now() - Duration::from_secs(9);
        capture_sitemap(
            &engine,
            &archive,
            &seed,
            &SiteRules::default(),
            &["https://example.com/listed".to_owned()],
            true,
            RunSoFar {
                started: began,
                pages_written: 0,
                archived: &HashSet::new(),
            },
        )
        .expect("a fake engine and a fresh archive do not fail a write");

        let crawled = engine.seeds_crawled();
        assert_eq!(crawled.len(), 1, "the listed URL was crawled once");
        let handed = crawled[0].deadline.expect("the run had a deadline");
        assert!(
            handed < Duration::from_secs(2),
            "a sub-crawl of a run with one second left was handed {handed:?}"
        );
    }

    /// A redirect the archive stored rather than followed is not a refusal, and counting it as
    /// one would print `answered 301` for a site that moved and served every request. The
    /// strict redirect policy archives that hop on purpose, so this is the archive's own
    /// decision showing up in the record rather than anything a host did to the run.
    #[test]
    fn a_redirect_the_archive_stored_is_not_counted_as_a_refusal() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![
            page("https://example.com/moved", 301, ""),
            page("https://example.com/gone", 404, "not found"),
        ]);

        let run = capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("a fake engine and a fresh archive do not fail a write");

        assert_eq!(
            run.captures_written, 1,
            "the redirect is stored, the refused response is not"
        );
        assert_eq!(run.responses_refused, BTreeMap::from([(404, 1)]));
    }
    /// The wait is paid when each listed URL is crawled too, not only when it is fetched, and
    /// this is the test that says why. A review read the engine's own throttle and concluded
    /// this branch was already paced, which would have made the wait here a redundant third
    /// delay; the engine sleeps only around the links a crawl finds for itself, and a sitemap
    /// sub-crawl has none, because a sitemap exists for a site whose pages do not link one
    /// another. Measured with the binary before this was written down: sub-crawls ran 0.05
    /// seconds apart under a one second delay.
    #[test]
    fn a_listed_url_that_is_crawled_rather_than_fetched_is_paced_the_same() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut seed = Seed::new("https://example.com/");
        seed.delay = Duration::from_millis(60);
        let urls: Vec<String> = (0..3)
            .map(|i| format!("https://example.com/p/{i}"))
            .collect();
        // Answering with no page at all, so what is measured is the gap between one listed URL
        // and the next rather than anything a crawl did inside one.
        let engine = ScriptedCrawlEngine::new(Vec::new());

        let started = Instant::now();
        capture_sitemap(
            &engine,
            &archive,
            &seed,
            &SiteRules::default(),
            &urls,
            true,
            RunSoFar::nothing_yet(&HashSet::new()),
        )
        .expect("a fake engine and a fresh archive do not fail a write");

        assert!(
            started.elapsed() >= Duration::from_millis(120),
            "three crawled URLs wait twice, took {:?}",
            started.elapsed()
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

    /// Two captures of one URL, one anonymous and one with a subscription, are different
    /// observations of the page, and nothing else in the record distinguishes them: the body is
    /// simply longer. So the record says which kind of run produced it, and never what the
    /// credential was.
    #[test]
    fn a_capture_made_with_a_session_says_so_in_the_record() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![page(
            "https://parknotes.substack.com/p/a-paid-post",
            200,
            "<html><head><title>A paid post</title></head><body><p>All of it.</p></body></html>",
        )]);

        capture_with_no_rules(
            &engine,
            &archive,
            &seed_with_a_session("https://parknotes.substack.com/archive"),
        )
        .expect("the run completes");

        let capture = only_capture_of(&archive, "https://parknotes.substack.com/p/a-paid-post");
        assert_eq!(capture.policy_departures, vec![PolicyDeparture::Session]);
        assert!(
            !format!("{capture:?}").contains("secret"),
            "the credential reached the record"
        );
    }

    /// The failure that is not silence but a false claim inside the archive. An apex redirecting
    /// to its `www` form and an `http` seed reaching its `https` one are both ordinary, and on
    /// either the HTTP client drops the header before the hop: what comes back is the teaser an
    /// anonymous reader is served. A record marking that capture as paid for would assert the
    /// archive holds a page it does not, which is worse than holding the teaser.
    #[test]
    fn a_capture_a_redirect_took_off_the_credential_s_origin_claims_no_session() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let mut redirected = page(
            "https://parknotes.substack.com/p/a-paid-post",
            200,
            "<html><head><title>A teaser</title></head><body><p>Subscribe.</p></body></html>",
        );
        response_of(&mut redirected).final_url =
            "https://www.parknotes.substack.com/p/a-paid-post".to_owned();
        let engine = ScriptedCrawlEngine::new(vec![redirected]);

        let run = capture_with_no_rules(
            &engine,
            &archive,
            &seed_with_a_session("https://parknotes.substack.com/archive"),
        )
        .expect("the run completes");

        assert!(
            only_capture_of(&archive, "https://www.parknotes.substack.com/p/a-paid-post")
                .policy_departures
                .is_empty(),
            "a capture the credential never reached was recorded as authenticated"
        );
        assert_eq!(run.captures_with_a_session, 0);
    }

    /// A credential can apply to nothing at all: bound to an origin the run never asks for, or to
    /// a seed whose host redirects everything away. The run then archives teasers and exits zero,
    /// so the count is what lets the report say the session did nothing.
    #[test]
    fn a_run_counts_the_captures_its_session_reached() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![
            page(
                "https://parknotes.substack.com/p/one",
                200,
                "<html>a</html>",
            ),
            page(
                "https://parknotes.substack.com/p/two",
                200,
                "<html>b</html>",
            ),
            page("https://substack.com/discover", 200, "<html>c</html>"),
        ]);

        let run = capture_with_no_rules(
            &engine,
            &archive,
            &seed_with_a_session("https://parknotes.substack.com/archive"),
        )
        .expect("the run completes");

        assert_eq!(run.captures_written, 3);
        assert_eq!(run.captures_with_a_session, 2);
    }

    /// A page on another host, which reaches the archive through a redirect the run followed off
    /// the seed. It is asked for without the session, so a record saying it was captured with one
    /// would describe an observation nobody made.
    #[test]
    fn a_capture_of_another_host_says_nothing_about_a_session() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine = ScriptedCrawlEngine::new(vec![page(
            "https://parkersfiction.substack.com/p/a-story",
            200,
            "<html><head><title>A story</title></head><body><p>Free.</p></body></html>",
        )]);

        capture_with_no_rules(
            &engine,
            &archive,
            &seed_with_a_session("https://parknotes.substack.com/archive"),
        )
        .expect("the run completes");

        assert!(
            only_capture_of(&archive, "https://parkersfiction.substack.com/p/a-story")
                .policy_departures
                .is_empty()
        );
    }

    /// The ordinary run, which is every run before this existed: no departure to record, and a
    /// record shaped exactly as it was.
    #[test]
    fn a_capture_from_an_ordinary_run_records_no_departure() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let engine =
            ScriptedCrawlEngine::new(vec![page("https://example.com/a", 200, "<html></html>")]);

        capture_with_no_rules(&engine, &archive, &Seed::new("https://example.com/"))
            .expect("the run completes");

        assert!(
            only_capture_of(&archive, "https://example.com/a")
                .policy_departures
                .is_empty()
        );
    }
}
