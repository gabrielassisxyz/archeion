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
use crate::metadata::ReferencedAsset;
use crate::storage::{Archive, Asset, AssetMiss, MissedAsset, NewAsset, StorageError};

/// The most references one capture deals with, which is also the most subresources it holds.
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

/// What one ask taught the run, and what it cost to find out.
struct Asked {
    learned: Learned,
    /// Bytes that came over the wire, kept or not. A body refused for its size was paid for
    /// before its size was known, and the ceiling that bounds a capture has to see that.
    transferred: u64,
}

impl Asked {
    /// An answer that cost no transfer: a URL refused before it was dialled.
    fn for_nothing(reason: AssetMiss) -> Self {
        Self {
            learned: Learned::Missed(reason),
            transferred: 0,
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

    /// Acquires what one page referenced, in the order the page referenced it.
    ///
    /// A failed write ends the run, like any other, and is the only error here: everything
    /// else that can go wrong is one subresource the capture will not have, which the record
    /// carries as a miss.
    pub(crate) fn of_page(
        &mut self,
        referenced: &[ReferencedAsset],
    ) -> Result<CapturedAssets, StorageError> {
        let mut captured = CapturedAssets::default();
        let mut bytes_spent = 0u64;

        for reference in referenced {
            let dealt_with = captured.stored.len() + captured.missed.len();
            if let Some(reason) = no_room_for_another(dealt_with, bytes_spent) {
                captured.missed.push(missed(&reference.url, reason));
                continue;
            }
            let learned = match self.known.get(&reference.url) {
                // A hit costs nothing, so it is served even when the run is out of time and it
                // counts against no ceiling: the bytes are already in the archive and the
                // record that references them is already written down.
                Some(known) => known.clone(),
                None if self.out_of_time() => Learned::Missed(AssetMiss::DeadlineReached),
                None => {
                    let asked = self.ask_for(&reference.url)?;
                    bytes_spent = bytes_spent.saturating_add(asked.transferred);
                    self.known
                        .insert(reference.url.clone(), asked.learned.clone());
                    asked.learned
                }
            };
            match learned {
                Learned::Stored(asset) => captured.stored.push(asset),
                Learned::Missed(reason) => captured.missed.push(missed(&reference.url, reason)),
            }
        }
        Ok(captured)
    }

    /// Fetches one subresource and stores it, or learns why it will not be stored.
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
                return Ok(Asked::for_nothing(AssetMiss::NoResponse {
                    detail: failure.reason,
                }));
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

/// Whether one more reference can be dealt with at all, given what this capture already did.
///
/// Both ceilings are checked before anything is asked for, which is what keeps a page that
/// references two thousand files from costing two thousand requests to then refuse most of
/// them.
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
    use super::*;

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
}
