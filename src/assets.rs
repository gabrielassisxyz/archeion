//! Fetching what a page needs to still make sense once the source is gone.
//!
//! The list of subresources is already in hand when this runs: extraction produced it from
//! the page's own markup, absolute and deduplicated. What is left is the part that costs
//! somebody else's bandwidth, so it is the part with the budgets on it. The numbers and the
//! reasoning behind them are in `docs/asset-capture.md`.
//!
//! A reference on a page is remote data deciding which address the next request goes to, so
//! nothing here treats one as safer than a seed. It is screened before it is followed and
//! again where it ended up, it is bounded in size, in count and in the time the run has left,
//! and whatever the pass refused is written into the record rather than merely skipped.

use std::collections::HashMap;
use std::time::Instant;

use crate::capture::media_type_of;
use crate::crawl::{CrawlEngine, PageEvent, PageResponse, Seed, points_inside_a_network};
use crate::metadata::{AssetKind, ReferencedAsset};
use crate::storage::{Archive, Asset, AssetMiss, MissedAsset, NewAsset, StorageError};

/// The most references one capture asks about.
///
/// A page carries tens of them and the extraction will hand over up to two thousand, which is
/// a count set to survive a hostile page rather than to be fetched. Beyond a hundred and
/// twenty-eight the marginal reference is a tracking pixel or a gallery thumbnail, and the
/// ceiling that matters for those is the one on bytes.
///
/// It counts references dealt with and not files kept, which is the difference between a
/// bound and the appearance of one: a page listing two thousand addresses that answer nothing
/// stores nothing, so a ceiling on what is stored would never be reached and the run would
/// spend two thousand requests learning that. Whoever wrote the page chooses the addresses,
/// and that is the shape an archiver is aimed at something with.
///
/// A reference the run has already answered is not one of those, and does not count here. It
/// is a request that will not be made, so charging it a slot bounded nothing and only made the
/// record report an absence over bytes the archive was holding. What the ceiling still bounds
/// is what it was drawn against: a page of fresh addresses, since extraction deduplicates
/// references by address and a page cannot spend the budget on repeats of one URL.
///
/// So this is no longer also the count of subresources a capture holds, and the difference is
/// where the record can grow: a page naming addresses a whole run has already learned
/// references every one of them. It stays bounded, by the two thousand references extraction
/// will hand over at all, and it costs no bytes in the store, since the bodies are already
/// there under their own hashes and a second record only points at them.
const MAX_ASSETS_PER_CAPTURE: usize = 128;

/// The most one subresource may spend.
///
/// Eight megabytes is above every stylesheet, script, font and photograph and below a video,
/// which is the line this number is drawn on: archiving a film because a page embedded it is
/// a decision to make on purpose, not one to reach by having no ceiling. What exceeds it is
/// recorded with its size, so the record says how far over the page was rather than that
/// something was dropped.
const MAX_ASSET_BYTES: u64 = 8 * 1024 * 1024;

/// The most one capture may transfer for its subresources.
///
/// It counts what came over the wire and not what was kept, which is the same distinction the
/// count ceiling above is drawn on. A body refused for its size was paid for in full before its
/// size was known, so a ceiling that counted only stored bytes would never be reached by a page
/// whose subresources are all just over the per-file limit, and the run would transfer a
/// hundred and twenty-eight of them per page. What is served out of what the run already
/// learned costs nothing and counts as nothing.
///
/// It is where the pass stops asking rather than a total it guarantees: the size of the next
/// subresource is unknown until it arrives, so the last one may cross the line, and the
/// overshoot is bounded by the response ceiling rather than by this number. Thirty-two
/// megabytes is many times the weight of a heavy page.
const MAX_ASSET_BYTES_PER_CAPTURE: u64 = 32 * 1024 * 1024;

/// What one capture ended up with, and what it did not.
#[derive(Debug, Default)]
pub(crate) struct CapturedAssets {
    pub stored: Vec<Asset>,
    pub missed: Vec<MissedAsset>,
}

/// How many requests in a row may produce no response at all before a capture stops asking.
///
/// This is the bound on how long one page can hold the pipeline. The pass runs inside the
/// callback the crawl hands pages through, so while it waits on a host that accepts
/// connections and then says nothing, the engine keeps fetching pages that nothing is reading:
/// they fill the queue between the two, and what overflows was paid for and thrown away. The
/// deadline alone does not cover that, since a page can spend the whole remaining budget one
/// request timeout at a time.
///
/// Three, because silence in a row is a fact about what is answering rather than about each
/// file: it survives a page referencing an unreachable host a couple of times, and it bounds
/// the wait at three request timeouts instead of at everything the run has left. What it costs
/// is a page whose first three references are on a dead host losing the rest, which is recorded
/// rather than silent, and recoverable later from the reference list without a crawl.
const MAX_CONSECUTIVE_SILENCES: usize = 3;

/// What one ask taught the run, and what it cost to find out.
struct Asked {
    learned: Learned,
    /// Bytes that came over the wire, kept or not. A body refused for its size was paid for
    /// before its size was known, and the ceiling that bounds a capture has to see that.
    transferred: u64,
    on_the_wire: OnTheWire,
}

/// What one ask did on the wire. A capture reads this to tell a file that is missing from a
/// host that has stopped answering, which are the same absence and a different problem.
#[derive(PartialEq, Eq)]
enum OnTheWire {
    /// No request was made, because the address was refused before it was dialled. It says
    /// nothing either way: it cost no wait, so it neither counts as silence nor clears it.
    NothingSent,
    /// A server answered, whatever it answered. A 404 is an answer.
    Answered,
    /// A request was made and produced no response at all.
    Silent,
}

impl Asked {
    /// An answer that cost no transfer and no request: a URL refused before it was dialled.
    fn for_nothing(reason: AssetMiss) -> Self {
        Self {
            learned: Learned::Missed(reason),
            transferred: 0,
            on_the_wire: OnTheWire::NothingSent,
        }
    }
}

/// What asking for one subresource taught the run.
#[derive(Debug, Clone)]
enum Learned {
    /// The bytes are in the archive and this record references them.
    Stored(Asset),
    /// There will be no bytes for this URL, and this is why. Only an answer about the URL
    /// itself is ever kept here: a ceiling one capture reached says nothing about the next
    /// one, and a run out of time is not a fact about a stylesheet.
    Missed(AssetMiss),
}

/// The subresource pass of one run.
///
/// It lives across the whole run because the answers do. One stylesheet belongs to every page
/// of a site that links it, and a run that asked the server for it once per page would spend
/// two hundred requests on one file: what is remembered is the answer, not the bytes, so a
/// second page referencing it gets the record that is already there without a request and
/// without a copy.
pub(crate) struct AssetCapture<'a> {
    engine: &'a dyn CrawlEngine,
    archive: &'a Archive,
    seed: &'a Seed,
    /// When the run began, which is the same instant the deadline is measured from above.
    started: Instant,
    known: HashMap<String, Learned>,
    fetches: usize,
}

impl<'a> AssetCapture<'a> {
    pub(crate) fn new(
        engine: &'a dyn CrawlEngine,
        archive: &'a Archive,
        seed: &'a Seed,
        started: Instant,
    ) -> Self {
        Self {
            engine,
            archive,
            seed,
            started,
            known: HashMap::new(),
            fetches: 0,
        }
    }

    /// Requests the run actually made. The gap between this and the subresources stored is
    /// what says a shared file was recognised instead of asked for once per page.
    pub(crate) fn fetches(&self) -> usize {
        self.fetches
    }

    /// Acquires what one page referenced.
    ///
    /// The budget is spent in `fetch_priority` order rather than the page's own, so a page
    /// that lists its script bundle before its photographs still has the photographs asked
    /// for first. What the record then lists is back in the order the page referenced them,
    /// because that ordering is a property of the page, not of the budget: a page that fits
    /// inside every ceiling reads exactly as it would have without this rule, and only a
    /// page that spills past a ceiling can tell the two orders apart.
    ///
    /// A page that spills past the count ceiling is also the one page whose stored set is not
    /// a property of the page alone. Every reference the run has already answered is served
    /// without spending a slot, so how many of the rest fit depends on what the pages before
    /// it taught, and a crawl does not promise an order. Two runs reaching one page after
    /// learning different things keep different sets of it, and since a capture id is built
    /// from the addresses it holds, they are two captures rather than one. What is guaranteed
    /// is narrower and is what the ceiling is for: the same page in the same run answered the
    /// same way is dealt with identically, no run asks for more than the ceiling per capture,
    /// and the set only ever grows with what the run knows. Charging a memo hit a slot bought
    /// that wider determinism by reporting an absence over bytes the archive was holding,
    /// which is the worse of the two.
    ///
    /// A failed write ends the run, like any other, and is the only error here: everything
    /// else that can go wrong is one subresource the capture will not have, which the record
    /// carries as a miss.
    pub(crate) fn of_page(
        &mut self,
        referenced: &[ReferencedAsset],
    ) -> Result<CapturedAssets, StorageError> {
        let mut fetch_order: Vec<usize> = (0..referenced.len()).collect();
        // Stable on purpose: two references of the same kind keep the order the page named
        // them in, so a page fetched twice, unchanged, spends its budget the same way both
        // times and stores the same set.
        fetch_order.sort_by_key(|&index| fetch_priority(referenced[index].kind));

        let mut outcomes: Vec<Option<Learned>> = vec![None; referenced.len()];
        // A rendition recovered by falling back beside the primary result rather than in
        // place of it: the widest candidate's own miss is still what a reader looks for that
        // address under, and this is the address nobody asked for that turned out to answer.
        let mut fallbacks_recovered: Vec<Asset> = Vec::new();
        let mut dealt_with = 0usize;
        let mut bytes_spent = 0u64;
        let mut silences = 0usize;

        for index in fetch_order {
            let reference = &referenced[index];
            // The memo is consulted before any ceiling, because every ceiling here bounds what
            // one capture may spend on somebody else's host and an answer the run already has
            // costs nothing to serve: no request, no transfer, no wait. Asking the count
            // ceiling first spent a slot on a reference whose bytes were already in the
            // archive and then recorded it as absent, and on a whole publication that was half
            // of every absence reported. It is served for the same reason when the run is out
            // of time or the capture has given up asking.
            let learned = if let Some(known) = self.known.get(&reference.url) {
                known.clone()
            } else if let Some(reason) = no_room_for_another(dealt_with, bytes_spent) {
                dealt_with += 1;
                Learned::Missed(reason)
            } else if silences >= MAX_CONSECUTIVE_SILENCES {
                dealt_with += 1;
                Learned::Missed(AssetMiss::NothingWasAnswering)
            } else if self.out_of_time() {
                dealt_with += 1;
                Learned::Missed(AssetMiss::DeadlineReached)
            } else {
                dealt_with += 1;
                let asked = self.ask_for(&reference.url)?;
                bytes_spent = bytes_spent.saturating_add(asked.transferred);
                match asked.on_the_wire {
                    OnTheWire::Silent => silences += 1,
                    OnTheWire::Answered => silences = 0,
                    OnTheWire::NothingSent => {}
                }
                self.known
                    .insert(reference.url.clone(), asked.learned.clone());
                asked.learned
            };
            if let Some(asset) =
                self.try_fallback(reference, &learned, &mut bytes_spent, &mut silences)?
            {
                fallbacks_recovered.push(asset);
            }
            outcomes[index] = Some(learned);
        }

        let mut captured = CapturedAssets::default();
        for (reference, outcome) in referenced.iter().zip(outcomes) {
            match outcome.expect("fetch_order visits every index exactly once") {
                Learned::Stored(asset) => captured.stored.push(asset),
                Learned::Missed(reason) => captured.missed.push(missed(&reference.url, reason)),
            }
        }
        captured.stored.extend(fallbacks_recovered);
        Ok(captured)
    }

    /// Tries the next-widest candidate a `srcset` offered, when the one just asked for
    /// answers false to `retryable_miss`: a 404, a body over the size ceiling, a short
    /// arrival, or an address the run refused to dial. Those are the misses no later pass
    /// changes by asking the same address again, which is exactly the state this project's
    /// own review found leaves a picture with no rendition in the archive at all.
    ///
    /// A retryable miss is left alone. The ceiling or the deadline it hit is a fact about
    /// this capture's own budget rather than about the file, and spending more of that
    /// budget on a rendition nobody asked for first would make the shortfall worse rather
    /// than better; a later pass over the same reference already retries the address that
    /// failed. What runs here for a page that never needs it costs nothing:
    /// `reference.fallback` is `None` for every asset that is not a `srcset`'s widest
    /// candidate.
    ///
    /// Only the one address is tried, never the rest of what the attribute offered, so a
    /// page whose widest rendition answers keeps costing exactly what it costs today and a
    /// page whose widest fails costs one request more rather than one per rendition.
    ///
    /// What this returns is additional to `learned`, not a replacement for it: the widest
    /// candidate's own outcome is reported exactly as it would be without this, address and
    /// reason both, since that is the address the note and `assets_by_url` still look for.
    /// A recovered rendition is a second fact about the same reference, not a correction of
    /// the first one.
    fn try_fallback(
        &mut self,
        reference: &ReferencedAsset,
        learned: &Learned,
        bytes_spent: &mut u64,
        silences: &mut usize,
    ) -> Result<Option<Asset>, StorageError> {
        let Learned::Missed(reason) = learned else {
            return Ok(None);
        };
        let Some(fallback_url) = reference.fallback.as_deref() else {
            return Ok(None);
        };
        if retryable_miss(reason) {
            return Ok(None);
        }
        // The same budget the primary address was judged against, since the address about
        // to be asked for spends the same host's bandwidth and the same run's clock.
        if *silences >= MAX_CONSECUTIVE_SILENCES
            || *bytes_spent >= MAX_ASSET_BYTES_PER_CAPTURE
            || self.out_of_time()
        {
            return Ok(None);
        }
        let candidate = if let Some(known) = self.known.get(fallback_url) {
            known.clone()
        } else {
            let asked = self.ask_for(fallback_url)?;
            *bytes_spent = bytes_spent.saturating_add(asked.transferred);
            match asked.on_the_wire {
                OnTheWire::Silent => *silences += 1,
                OnTheWire::Answered => *silences = 0,
                OnTheWire::NothingSent => {}
            }
            self.known
                .insert(fallback_url.to_owned(), asked.learned.clone());
            asked.learned
        };
        match candidate {
            // Marked here rather than trusted from the memo: the same address can be
            // somebody else's own widest candidate, stored and remembered as itself, and
            // what makes it a fallback is this reference's situation, not a fact about the
            // bytes.
            Learned::Stored(asset) => Ok(Some(Asset {
                is_fallback: true,
                ..asset
            })),
            // The fallback did not answer either. Nothing is added; the widest's own miss,
            // already in `learned`, is all this reference has to report.
            Learned::Missed(_) => Ok(None),
        }
    }

    fn ask_for(&mut self, url: &str) -> Result<Asked, StorageError> {
        // Screened before the engine is asked, and not only because the engine refuses these
        // addresses as well. Handing one over means a request that never leaves the machine and
        // a refusal this pass then has to read out of an error string, when the address is
        // something it can judge itself.
        if !self.seed.allow_private_addresses && points_inside_a_network(url) {
            return Ok(Asked::for_nothing(AssetMiss::InsideANetwork));
        }
        self.fetches += 1;
        let response = match self.engine.fetch(url, self.seed) {
            PageEvent::Response(response) => response,
            // A reference the engine refused to dial arrives here too, which is the honest
            // shape for it: to the capture, a subresource it cannot have is a subresource it
            // cannot have, and the reason it was refused travels with it.
            PageEvent::NoResponse(failure) => {
                return Ok(Asked {
                    learned: Learned::Missed(AssetMiss::NoResponse {
                        detail: failure.reason,
                    }),
                    transferred: 0,
                    // The request was made and nothing came back, which is the outcome that
                    // costs a whole request timeout and the only one worth counting.
                    on_the_wire: OnTheWire::Silent,
                });
            }
        };

        // The same guard where the request ended up, which is the half the check above cannot
        // cover: the engine screens every redirect hop, and the archive checks anyway, because
        // the engine is the replaceable part while the bytes about to be written are durable.
        let byte_len = response.body.len() as u64;
        let refused = |reason: AssetMiss| {
            Ok(Asked {
                learned: Learned::Missed(reason),
                transferred: byte_len,
                on_the_wire: OnTheWire::Answered,
            })
        };
        if !self.seed.allow_private_addresses && points_inside_a_network(&response.final_url) {
            return refused(AssetMiss::InsideANetwork);
        }
        if response.body_truncated {
            // The one place a subresource is treated differently from a page. A page that
            // arrived short is still the page and is kept, marked as short. A subresource
            // exists so the page it belongs to still works, and a stylesheet missing its end
            // does not: storing it would put bytes in the archive that read as the whole
            // file, since a subresource record has nowhere to say otherwise.
            return refused(AssetMiss::ArrivedShort { byte_len });
        }
        if byte_len > MAX_ASSET_BYTES {
            return refused(AssetMiss::TooLarge { byte_len });
        }

        Ok(Asked {
            learned: Learned::Stored(self.archive.store_asset(&new_asset(response))?),
            transferred: byte_len,
            on_the_wire: OnTheWire::Answered,
        })
    }

    /// Whether the run's wall-clock budget is gone.
    ///
    /// It reads the budget plainly, without the margin the backstop above the boundary
    /// allows itself. That margin exists so an engine handing over pages it already fetched
    /// is not cut off mid-handover; a subresource that has not been asked for yet is the
    /// opposite case, and it is also the cheapest thing in a run to give up on, since the
    /// page it belongs to is archived either way.
    fn out_of_time(&self) -> bool {
        self.seed
            .deadline
            .is_some_and(|budget| self.started.elapsed() >= budget)
    }
}

/// Whether a missed subresource is one a later pass may ask for again.
///
/// A response that never came is not retried blindly: this pass is not a crawl. It spends
/// bandwidth only when the archive itself stopped asking, hit a ceiling, or had run out of
/// budget before the URL was tried.
pub(crate) fn retryable_miss(reason: &AssetMiss) -> bool {
    match reason {
        AssetMiss::TooLarge { byte_len } => *byte_len <= MAX_ASSET_BYTES,
        AssetMiss::CountCeilingReached
        | AssetMiss::ByteCeilingReached
        | AssetMiss::DeadlineReached
        | AssetMiss::NothingWasAnswering => true,
        AssetMiss::NoResponse { .. }
        | AssetMiss::ArrivedShort { .. }
        | AssetMiss::InsideANetwork => false,
    }
}

/// Where one kind sits in the fetch order. Lower goes first, and the ranking is by what an
/// archive of prose loses if the kind never arrives, not by how a browser would render the
/// page.
///
/// An image is asked for first: it is content the prose itself points a reader at, and a
/// page's photographs are what an archive of writing is least willing to be without.
/// Embedded audio or video is asked for next, on the same reasoning; it is ranked below an
/// image only because a page carries far more of the one than the other, so spending the
/// budget on images first stores more of what is likely to still fit inside it. A stylesheet
/// comes next: not prose, but written for this one page rather than shared across a whole
/// site, so it is still evidence about the page it was fetched with. An icon follows a
/// stylesheet: a site's favicon, generic, reused on every page of the site, and telling a
/// reader almost nothing about the one page it happened to be seen on. A script is asked for
/// last, on purpose, because it is the kind this ordering exists for: a modern site's script
/// bundle is minified, named by a content hash and useless later without the page
/// environment this archive does not replay.
fn fetch_priority(kind: AssetKind) -> u8 {
    match kind {
        AssetKind::Image => 0,
        AssetKind::Media => 1,
        AssetKind::Stylesheet => 2,
        AssetKind::Icon => 3,
        AssetKind::Script => 4,
    }
}

/// Whether one more reference can be dealt with at all, given what this capture already did.
///
/// Both ceilings are checked before anything is asked for, which is what keeps a page that
/// references two thousand files from costing two thousand requests to then refuse most of
/// them. They are consulted only for a reference the run has no answer for, since both bound
/// what asking costs and a memo hit asks nothing.
fn no_room_for_another(dealt_with: usize, bytes_spent: u64) -> Option<AssetMiss> {
    if dealt_with >= MAX_ASSETS_PER_CAPTURE {
        return Some(AssetMiss::CountCeilingReached);
    }
    if bytes_spent >= MAX_ASSET_BYTES_PER_CAPTURE {
        return Some(AssetMiss::ByteCeilingReached);
    }
    None
}

fn missed(url: &str, reason: AssetMiss) -> MissedAsset {
    MissedAsset {
        url: url.to_owned(),
        reason,
    }
}

fn new_asset(response: PageResponse) -> NewAsset {
    NewAsset {
        media_type: media_type_of(&response.headers),
        requested_url: response.requested_url,
        final_url: response.final_url,
        status: response.status,
        body: response.body,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::ops::ControlFlow;

    use tempfile::TempDir;

    use super::*;
    use crate::crawl::{CrawlError, CrawlOutcome, FetchFailure};
    use crate::storage::Header;

    /// A crawl engine that answers a written-down set of subresource URLs and nothing else,
    /// so `of_page` is driven without a network. `of_page` reaches only `fetch`: `crawl` and
    /// `check_seed` are never called from inside an asset pass, and are implemented here only
    /// because the trait requires them.
    struct ScriptedEngine {
        subresources: HashMap<String, PageEvent>,
        fetched: RefCell<Vec<String>>,
    }

    impl ScriptedEngine {
        fn serving(subresources: Vec<PageEvent>) -> Self {
            let mut by_url = HashMap::new();
            for event in subresources {
                let url = match &event {
                    PageEvent::Response(response) => response.requested_url.clone(),
                    PageEvent::NoResponse(failure) => failure.url.clone(),
                };
                by_url.insert(url, event);
            }
            Self {
                subresources: by_url,
                fetched: RefCell::new(Vec::new()),
            }
        }

        /// The addresses actually dialled, in the order `fetch` was called for them.
        fn fetched(&self) -> Vec<String> {
            self.fetched.borrow().clone()
        }
    }

    impl CrawlEngine for ScriptedEngine {
        fn check_seed(&self, _seed: &Seed) -> Result<(), CrawlError> {
            Ok(())
        }

        fn crawl(
            &self,
            _seed: &Seed,
            _on_page: &mut dyn FnMut(PageEvent) -> ControlFlow<()>,
        ) -> Result<CrawlOutcome, CrawlError> {
            Ok(CrawlOutcome::default())
        }

        fn fetch(&self, url: &str, _seed: &Seed) -> PageEvent {
            self.fetched.borrow_mut().push(url.to_owned());
            self.subresources.get(url).cloned().unwrap_or_else(|| {
                PageEvent::NoResponse(FetchFailure {
                    url: url.to_owned(),
                    reason: "this fake has no response for the URL".to_owned(),
                })
            })
        }
    }

    fn archive_in(dir: &TempDir) -> Archive {
        Archive::open(dir.path()).expect("archive opens in an empty directory")
    }

    fn referenced(url: &str, kind: AssetKind) -> ReferencedAsset {
        ReferencedAsset {
            url: url.to_owned(),
            kind,
            fallback: None,
        }
    }

    fn referenced_with_fallback(url: &str, kind: AssetKind, fallback: &str) -> ReferencedAsset {
        ReferencedAsset {
            url: url.to_owned(),
            kind,
            fallback: Some(fallback.to_owned()),
        }
    }

    /// A stored 200 whose body is a given number of bytes, for the ceiling that counts them.
    fn response_of_size(url: &str, bytes: usize) -> PageEvent {
        let PageEvent::Response(mut response) = stored_response(url) else {
            unreachable!("stored_response answers with a response");
        };
        response.body = vec![b'x'; bytes];
        PageEvent::Response(response)
    }

    /// A stored 200 for one subresource URL. The body is the URL's own bytes, which is
    /// enough to tell two subresources apart without a body worth naming.
    fn stored_response(url: &str) -> PageEvent {
        PageEvent::Response(PageResponse {
            requested_url: url.to_owned(),
            final_url: url.to_owned(),
            status: 200,
            headers: vec![Header {
                name: "content-type".to_owned(),
                value: "application/octet-stream".to_owned(),
            }],
            body: url.as_bytes().to_vec(),
            body_truncated: false,
            fetched_at: "2026-07-28T00:00:00Z".parse().expect("valid timestamp"),
        })
    }

    /// The regression this ordering exists for: a page that lists its script bundles before
    /// its photographs, with more references than the ceiling holds, used to spend the whole
    /// budget on the scripts and leave every image to a later pass. Fetch priority puts the
    /// images first regardless of where the page put them, so they are what survives.
    #[test]
    fn scripts_listed_before_images_lose_the_budget_to_the_images() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let seed = Seed::new("https://example.com/");

        let scripts: Vec<String> = (0..MAX_ASSETS_PER_CAPTURE)
            .map(|i| format!("https://example.com/bundle-{i}.js"))
            .collect();
        let images = [
            "https://example.com/photo-0.jpg".to_owned(),
            "https://example.com/photo-1.jpg".to_owned(),
            "https://example.com/photo-2.jpg".to_owned(),
        ];

        let mut referenced_assets: Vec<ReferencedAsset> = scripts
            .iter()
            .map(|url| referenced(url, AssetKind::Script))
            .collect();
        referenced_assets.extend(images.iter().map(|url| referenced(url, AssetKind::Image)));

        let all_urls: Vec<&String> = scripts.iter().chain(images.iter()).collect();
        let engine =
            ScriptedEngine::serving(all_urls.iter().map(|url| stored_response(url)).collect());

        let mut assets = AssetCapture::new(&engine, &archive, &seed, Instant::now());
        let captured = assets
            .of_page(&referenced_assets)
            .expect("a fake engine and a fresh archive do not fail a write");

        assert_eq!(captured.stored.len(), MAX_ASSETS_PER_CAPTURE);
        for image in &images {
            assert!(
                captured
                    .stored
                    .iter()
                    .any(|asset| &asset.final_url == image),
                "{image} should have outranked a script for the budget"
            );
        }
        assert_eq!(captured.missed.len(), 3, "{:?}", captured.missed);
        for miss in &captured.missed {
            assert_eq!(miss.reason, AssetMiss::CountCeilingReached);
            assert!(
                miss.url.ends_with(".js"),
                "only a script should have missed the budget: {}",
                miss.url
            );
        }
        // The same hundred and twenty-eight references are dealt with either way; ordering
        // decides which ones, not how many.
        assert_eq!(assets.fetches(), MAX_ASSETS_PER_CAPTURE);
    }

    /// The set stored, and its order, are both derived from the page, the answers a host
    /// gave, and what the run had already learned when it arrived. Two runs that reach this
    /// page knowing the same things agree with themselves, which is what keeps a capture id,
    /// built from this order, meaning the same thing on a repeat capture that changed nothing.
    ///
    /// The third term is real and is not exercised here, deliberately: it can only be told
    /// apart by a page that spills past the count ceiling, and `of_page` says what that costs.
    #[test]
    fn a_page_captured_twice_produces_the_same_stored_set_in_the_same_order() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let seed = Seed::new("https://example.com/");

        let referenced_assets = vec![
            referenced("https://example.com/app.js", AssetKind::Script),
            referenced("https://example.com/style.css", AssetKind::Stylesheet),
            referenced("https://example.com/hero.jpg", AssetKind::Image),
            referenced("https://example.com/favicon.ico", AssetKind::Icon),
            referenced("https://example.com/clip.mp3", AssetKind::Media),
        ];
        let urls: Vec<&str> = referenced_assets.iter().map(|r| r.url.as_str()).collect();

        let first_engine =
            ScriptedEngine::serving(urls.iter().map(|url| stored_response(url)).collect());
        let mut first_pass = AssetCapture::new(&first_engine, &archive, &seed, Instant::now());
        let first = first_pass
            .of_page(&referenced_assets)
            .expect("the first capture");

        let second_engine =
            ScriptedEngine::serving(urls.iter().map(|url| stored_response(url)).collect());
        let mut second_pass = AssetCapture::new(&second_engine, &archive, &seed, Instant::now());
        let second = second_pass
            .of_page(&referenced_assets)
            .expect("the second capture");

        assert_eq!(first.stored, second.stored);
        assert_eq!(first.missed, second.missed);
    }

    /// A page that fits inside every ceiling is read exactly as it would have been before
    /// this ordering existed: everything is stored, and it is stored back in the order the
    /// page named it, not in fetch priority order. Only a page that spills past a ceiling can
    /// tell the two orders apart.
    #[test]
    fn a_page_under_the_ceiling_stores_everything_in_page_order() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let seed = Seed::new("https://example.com/");

        let referenced_assets = vec![
            referenced("https://example.com/app.js", AssetKind::Script),
            referenced("https://example.com/style.css", AssetKind::Stylesheet),
            referenced("https://example.com/hero.jpg", AssetKind::Image),
            referenced("https://example.com/favicon.ico", AssetKind::Icon),
            referenced("https://example.com/clip.mp3", AssetKind::Media),
        ];
        let urls: Vec<&str> = referenced_assets.iter().map(|r| r.url.as_str()).collect();
        let engine = ScriptedEngine::serving(urls.iter().map(|url| stored_response(url)).collect());

        let mut assets = AssetCapture::new(&engine, &archive, &seed, Instant::now());
        let captured = assets
            .of_page(&referenced_assets)
            .expect("nothing here should miss");

        assert!(captured.missed.is_empty(), "{:?}", captured.missed);
        let stored_urls: Vec<&str> = captured
            .stored
            .iter()
            .map(|asset| asset.final_url.as_str())
            .collect();
        assert_eq!(
            stored_urls, urls,
            "a capture under the ceiling reads exactly as it did before fetch order existed"
        );
        assert_eq!(assets.fetches(), referenced_assets.len());
    }

    /// The byte ceiling is a running sum, so unlike the count it is spent at a rate the
    /// references themselves set, and ordering therefore decides how many fit rather than only
    /// which ones. A page whose images alone exhaust the budget leaves nothing for the kinds
    /// below them, which is the cost of ranking by kind and is worth having a page to point at.
    ///
    /// What survives the reordering is the record: the outcomes are still listed in the order
    /// the page named its references, and a page over a ceiling still answers the same way
    /// twice.
    #[test]
    fn images_large_enough_to_spend_the_byte_budget_leave_none_of_it_for_the_rest() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let seed = Seed::new("https://example.com/");

        // As large as one subresource may be, so that the per asset ceiling admits each answer
        // and it takes a handful of them to reach the ceiling for the whole capture. Derived
        // rather than written out, so the arithmetic follows the ceilings if they move.
        let heavy = usize::try_from(MAX_ASSET_BYTES).expect("fits in usize");
        let images: Vec<String> = (0..MAX_ASSET_BYTES_PER_CAPTURE / MAX_ASSET_BYTES + 1)
            .map(|i| format!("https://example.com/photo-{i}.jpg"))
            .collect();

        let mut referenced_assets = vec![referenced(
            "https://example.com/style.css",
            AssetKind::Stylesheet,
        )];
        referenced_assets.extend(images.iter().map(|url| referenced(url, AssetKind::Image)));
        referenced_assets.push(referenced("https://example.com/app.js", AssetKind::Script));

        let mut answers = vec![stored_response("https://example.com/style.css")];
        answers.extend(images.iter().map(|url| response_of_size(url, heavy)));
        answers.push(stored_response("https://example.com/app.js"));
        let engine = ScriptedEngine::serving(answers);

        let mut assets = AssetCapture::new(&engine, &archive, &seed, Instant::now());
        let captured = assets
            .of_page(&referenced_assets)
            .expect("a fake engine and a fresh archive do not fail a write");

        let stored: Vec<&str> = captured
            .stored
            .iter()
            .map(|asset| asset.final_url.as_str())
            .collect();
        let expected: Vec<&str> = images
            .iter()
            .take(images.len() - 1)
            .map(String::as_str)
            .collect();
        assert_eq!(
            stored, expected,
            "the images outrank the rest until the budget is gone"
        );

        let missed: Vec<(&str, &AssetMiss)> = captured
            .missed
            .iter()
            .map(|miss| (miss.url.as_str(), &miss.reason))
            .collect();
        assert_eq!(
            missed,
            [
                (
                    "https://example.com/style.css",
                    &AssetMiss::ByteCeilingReached
                ),
                (
                    images.last().expect("images were built above").as_str(),
                    &AssetMiss::ByteCeilingReached
                ),
                ("https://example.com/app.js", &AssetMiss::ByteCeilingReached),
            ],
            "what the images spent is spent for everything below them, and the record still
             lists what happened in the order the page named it"
        );
    }

    /// The ceiling bounds requests, and a reference the run already answered is not one. This
    /// page names ten files nobody has asked for and then the whole of a previous page's set,
    /// more references than the ceiling holds, and every one of the latter is already in the
    /// archive: charging them a slot made the last ten of them be recorded as absent while
    /// their bytes sat in the archive under another capture. On a whole publication that shape
    /// was half of every absence reported.
    #[test]
    fn a_reference_the_run_already_answered_is_served_past_the_count_ceiling() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let seed = Seed::new("https://example.com/");

        let shared: Vec<String> = (0..MAX_ASSETS_PER_CAPTURE)
            .map(|i| format!("https://example.com/shared-{i}.png"))
            .collect();
        let fresh: Vec<String> = (0..10)
            .map(|i| format!("https://example.com/own-{i}.png"))
            .collect();

        let first_page: Vec<ReferencedAsset> = shared
            .iter()
            .map(|url| referenced(url, AssetKind::Image))
            .collect();
        let mut second_page: Vec<ReferencedAsset> = fresh
            .iter()
            .map(|url| referenced(url, AssetKind::Image))
            .collect();
        second_page.extend(
            first_page
                .iter()
                .map(|asset| referenced(&asset.url, asset.kind)),
        );

        let answers: Vec<PageEvent> = shared
            .iter()
            .chain(fresh.iter())
            .map(|url| stored_response(url))
            .collect();
        let engine = ScriptedEngine::serving(answers);

        let mut assets = AssetCapture::new(&engine, &archive, &seed, Instant::now());
        let first = assets
            .of_page(&first_page)
            .expect("a fake engine and a fresh archive do not fail a write");
        assert!(first.missed.is_empty(), "{:?}", first.missed);
        assert_eq!(assets.fetches(), MAX_ASSETS_PER_CAPTURE);

        let second = assets
            .of_page(&second_page)
            .expect("a fake engine and a fresh archive do not fail a write");

        assert!(second.missed.is_empty(), "{:?}", second.missed);
        assert_eq!(second.stored.len(), second_page.len());
        assert_eq!(
            assets.fetches(),
            MAX_ASSETS_PER_CAPTURE + fresh.len(),
            "only the addresses nobody had answered cost a request"
        );
    }

    /// What the ceiling was drawn against, unchanged: a page of addresses the run has never
    /// answered still deals with exactly the ceiling and asks for no more, however many it
    /// lists. Serving a memo hit for free would be worth nothing if it bought a hostile page
    /// an unbounded number of requests, and this is the test that says it does not.
    #[test]
    fn a_page_of_addresses_nobody_answered_before_still_stops_at_the_count_ceiling() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let seed = Seed::new("https://example.com/");

        let fresh: Vec<String> = (0..MAX_ASSETS_PER_CAPTURE * 2)
            .map(|i| format!("https://example.com/photo-{i}.png"))
            .collect();
        let page: Vec<ReferencedAsset> = fresh
            .iter()
            .map(|url| referenced(url, AssetKind::Image))
            .collect();
        let engine =
            ScriptedEngine::serving(fresh.iter().map(|url| stored_response(url)).collect());

        let mut assets = AssetCapture::new(&engine, &archive, &seed, Instant::now());
        let captured = assets
            .of_page(&page)
            .expect("a fake engine and a fresh archive do not fail a write");

        assert_eq!(captured.stored.len(), MAX_ASSETS_PER_CAPTURE);
        assert_eq!(captured.missed.len(), MAX_ASSETS_PER_CAPTURE);
        for miss in &captured.missed {
            assert_eq!(miss.reason, AssetMiss::CountCeilingReached);
        }
        assert_eq!(assets.fetches(), MAX_ASSETS_PER_CAPTURE);
    }

    #[test]
    fn a_capture_that_has_room_refuses_nothing() {
        assert_eq!(no_room_for_another(0, 0), None);
        assert_eq!(
            no_room_for_another(MAX_ASSETS_PER_CAPTURE - 1, MAX_ASSET_BYTES_PER_CAPTURE - 1),
            None
        );
    }

    /// Which ceiling was reached is the whole value of recording it: one says the page had
    /// more files than a capture holds, the other that it had heavier ones.
    #[test]
    fn a_capture_at_a_ceiling_says_which_one_it_reached() {
        assert_eq!(
            no_room_for_another(MAX_ASSETS_PER_CAPTURE, 0),
            Some(AssetMiss::CountCeilingReached)
        );
        assert_eq!(
            no_room_for_another(0, MAX_ASSET_BYTES_PER_CAPTURE),
            Some(AssetMiss::ByteCeilingReached)
        );
    }

    /// The count is checked first on purpose. A capture that is at both ceilings has too many
    /// files, and reporting the bytes would send whoever reads it after the wrong number.
    #[test]
    fn a_capture_at_both_ceilings_reports_the_count() {
        assert_eq!(
            no_room_for_another(MAX_ASSETS_PER_CAPTURE, MAX_ASSET_BYTES_PER_CAPTURE),
            Some(AssetMiss::CountCeilingReached)
        );
    }

    /// The ranking spelled out, so a change to it fails a test that names the reasoning
    /// rather than one built from two hundred generated URLs.
    #[test]
    fn fetch_priority_ranks_content_first_and_script_last() {
        assert!(fetch_priority(AssetKind::Image) < fetch_priority(AssetKind::Media));
        assert!(fetch_priority(AssetKind::Media) < fetch_priority(AssetKind::Stylesheet));
        assert!(fetch_priority(AssetKind::Stylesheet) < fetch_priority(AssetKind::Icon));
        assert!(fetch_priority(AssetKind::Icon) < fetch_priority(AssetKind::Script));
    }

    /// A response the engine never got back at all, the shape a 404 an underlying client
    /// turns into an error for, or a request that timed out, both take.
    fn no_response(url: &str) -> PageEvent {
        PageEvent::NoResponse(FetchFailure {
            url: url.to_owned(),
            reason: "no server answered".to_owned(),
        })
    }

    /// A response whose body arrived shorter than it promised.
    fn short_response(url: &str) -> PageEvent {
        let PageEvent::Response(mut response) = stored_response(url) else {
            unreachable!("stored_response answers with a response");
        };
        response.body_truncated = true;
        PageEvent::Response(response)
    }

    /// Every reason `retryable_miss` answers false for leaves the archive with nothing
    /// recoverable today, per `arch-3ff`: no later pass changes any of the four by asking
    /// the same address again. Each earns its own case because each is decided in a
    /// different place: a 404 comes back as no response at all, a short arrival and an
    /// oversized one are both read from a response that did arrive, and a private address is
    /// refused before a request is ever sent.
    #[test]
    fn a_widest_candidate_with_no_response_falls_back_to_the_next_widest() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let seed = Seed::new("https://example.com/");
        let referenced_assets = vec![referenced_with_fallback(
            "https://example.com/wide.jpg",
            AssetKind::Image,
            "https://example.com/narrow.jpg",
        )];
        let engine = ScriptedEngine::serving(vec![
            no_response("https://example.com/wide.jpg"),
            stored_response("https://example.com/narrow.jpg"),
        ]);

        let mut assets = AssetCapture::new(&engine, &archive, &seed, Instant::now());
        let captured = assets
            .of_page(&referenced_assets)
            .expect("a fake engine and a fresh archive do not fail a write");

        assert_eq!(
            captured.missed,
            [missed(
                "https://example.com/wide.jpg",
                AssetMiss::NoResponse {
                    detail: "no server answered".to_owned()
                }
            )],
            "the widest candidate's own miss is still reported, address and reason both"
        );
        assert_eq!(captured.stored.len(), 1, "{:?}", captured.stored);
        assert_eq!(
            captured.stored[0].final_url,
            "https://example.com/narrow.jpg"
        );
        assert!(
            captured.stored[0].is_fallback,
            "a rendition kept because the widest could not be fetched is marked as one"
        );
    }

    #[test]
    fn a_widest_candidate_that_arrives_short_falls_back_to_the_next_widest() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let seed = Seed::new("https://example.com/");
        let referenced_assets = vec![referenced_with_fallback(
            "https://example.com/wide.jpg",
            AssetKind::Image,
            "https://example.com/narrow.jpg",
        )];
        let engine = ScriptedEngine::serving(vec![
            short_response("https://example.com/wide.jpg"),
            stored_response("https://example.com/narrow.jpg"),
        ]);

        let mut assets = AssetCapture::new(&engine, &archive, &seed, Instant::now());
        let captured = assets
            .of_page(&referenced_assets)
            .expect("a fake engine and a fresh archive do not fail a write");

        assert_eq!(captured.stored.len(), 1, "{:?}", captured.stored);
        assert_eq!(
            captured.stored[0].final_url,
            "https://example.com/narrow.jpg"
        );
        assert!(captured.stored[0].is_fallback);
        assert_eq!(captured.missed.len(), 1, "{:?}", captured.missed);
        assert_eq!(captured.missed[0].url, "https://example.com/wide.jpg");
    }

    /// The byte ceiling is read after the response has already arrived, unlike the other
    /// three: the size is only known once the bytes have moved.
    #[test]
    fn a_widest_candidate_over_the_size_ceiling_falls_back_to_the_next_widest() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let seed = Seed::new("https://example.com/");
        let referenced_assets = vec![referenced_with_fallback(
            "https://example.com/wide.jpg",
            AssetKind::Image,
            "https://example.com/narrow.jpg",
        )];
        let heavy = usize::try_from(MAX_ASSET_BYTES + 1).expect("fits in usize");
        let engine = ScriptedEngine::serving(vec![
            response_of_size("https://example.com/wide.jpg", heavy),
            stored_response("https://example.com/narrow.jpg"),
        ]);

        let mut assets = AssetCapture::new(&engine, &archive, &seed, Instant::now());
        let captured = assets
            .of_page(&referenced_assets)
            .expect("a fake engine and a fresh archive do not fail a write");

        assert_eq!(captured.stored.len(), 1, "{:?}", captured.stored);
        assert_eq!(
            captured.stored[0].final_url,
            "https://example.com/narrow.jpg"
        );
        assert!(captured.stored[0].is_fallback);
        assert_eq!(
            captured.missed,
            [missed(
                "https://example.com/wide.jpg",
                AssetMiss::TooLarge {
                    byte_len: MAX_ASSET_BYTES + 1
                }
            )]
        );
    }

    /// The one of the four decided before any request is sent: the address itself is inside
    /// a network this run was not told to reach, so the widest candidate costs no request at
    /// all and the fallback is what the run actually asks for.
    #[test]
    fn a_widest_candidate_inside_a_network_falls_back_to_the_next_widest() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let seed = Seed::new("https://example.com/");
        let referenced_assets = vec![referenced_with_fallback(
            "http://169.254.169.254/wide.jpg",
            AssetKind::Image,
            "https://example.com/narrow.jpg",
        )];
        let engine =
            ScriptedEngine::serving(vec![stored_response("https://example.com/narrow.jpg")]);

        let mut assets = AssetCapture::new(&engine, &archive, &seed, Instant::now());
        let captured = assets
            .of_page(&referenced_assets)
            .expect("a fake engine and a fresh archive do not fail a write");

        assert_eq!(
            captured.missed,
            [missed(
                "http://169.254.169.254/wide.jpg",
                AssetMiss::InsideANetwork
            )]
        );
        assert_eq!(captured.stored.len(), 1, "{:?}", captured.stored);
        assert_eq!(
            captured.stored[0].final_url,
            "https://example.com/narrow.jpg"
        );
        assert!(captured.stored[0].is_fallback);
        assert_eq!(
            engine.fetched(),
            ["https://example.com/narrow.jpg"],
            "the address inside the network is never dialled"
        );
    }

    /// A miss `retryable_miss` answers true for is a fact about this capture's own budget
    /// rather than about the file, so a fallback is not spent on it: a later pass over the
    /// same reference already retries the address that failed, and asking a second address
    /// too would spend more of a budget that is the reason the first one missed.
    #[test]
    fn a_retryable_miss_is_not_given_a_fallback() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let seed = Seed::new("https://example.com/");
        let referenced_assets = vec![referenced_with_fallback(
            "https://example.com/wide.jpg",
            AssetKind::Image,
            "https://example.com/narrow.jpg",
        )];
        // A page of addresses ahead of it that fills the count ceiling, so the reference
        // under test is refused by `no_room_for_another` before it is ever asked for, which
        // is a retryable miss.
        let filler: Vec<ReferencedAsset> = (0..MAX_ASSETS_PER_CAPTURE)
            .map(|i| {
                referenced(
                    &format!("https://example.com/filler-{i}.png"),
                    AssetKind::Image,
                )
            })
            .collect();
        let mut all = filler.clone();
        all.extend(referenced_assets);
        let engine = ScriptedEngine::serving(
            filler
                .iter()
                .map(|asset| stored_response(&asset.url))
                .collect(),
        );

        let mut assets = AssetCapture::new(&engine, &archive, &seed, Instant::now());
        let captured = assets
            .of_page(&all)
            .expect("a fake engine and a fresh archive do not fail a write");

        assert_eq!(
            captured
                .missed
                .iter()
                .find(|miss| miss.url == "https://example.com/wide.jpg")
                .map(|miss| &miss.reason),
            Some(&AssetMiss::CountCeilingReached)
        );
        assert!(
            captured
                .stored
                .iter()
                .all(|asset| asset.final_url != "https://example.com/narrow.jpg"),
            "a retryable miss must not spend a request on its fallback: {:?}",
            captured.stored
        );
        assert!(
            !engine
                .fetched()
                .contains(&"https://example.com/narrow.jpg".to_owned()),
            "the fallback address must never be dialled for a retryable miss"
        );
    }

    /// The cost guard this fix must not violate: a reference whose widest candidate answers
    /// spends exactly the one request it always did, whether or not it carries a fallback
    /// address, because trying every candidate a `srcset` offered is the cost the
    /// widest-only rule exists to remove.
    #[test]
    fn a_widest_candidate_that_answers_never_spends_a_request_on_its_fallback() {
        let dir = TempDir::new().expect("temp dir");
        let archive = archive_in(&dir);
        let seed = Seed::new("https://example.com/");
        let referenced_assets = vec![referenced_with_fallback(
            "https://example.com/wide.jpg",
            AssetKind::Image,
            "https://example.com/narrow.jpg",
        )];
        let engine = ScriptedEngine::serving(vec![stored_response("https://example.com/wide.jpg")]);

        let mut assets = AssetCapture::new(&engine, &archive, &seed, Instant::now());
        let captured = assets
            .of_page(&referenced_assets)
            .expect("a fake engine and a fresh archive do not fail a write");

        assert!(captured.missed.is_empty(), "{:?}", captured.missed);
        assert_eq!(captured.stored.len(), 1);
        assert_eq!(captured.stored[0].final_url, "https://example.com/wide.jpg");
        assert!(!captured.stored[0].is_fallback);
        assert_eq!(
            assets.fetches(),
            1,
            "a fallback nobody needed must cost nothing"
        );
        assert_eq!(engine.fetched(), ["https://example.com/wide.jpg"]);
    }
}
