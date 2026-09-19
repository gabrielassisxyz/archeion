//! `archeion repass`: derived records refreshed from captures already on disk.

use std::error::Error;
use std::fmt::Write as _;
use std::io::{self, IsTerminal, Write as _};
use std::path::PathBuf;
use std::time::Instant;

use archeion::crawl::SpiderEngine;
use archeion::readability::SiteRules;
use archeion::repass::{
    RepassError, RepassEvent, RepassOptions, RepassRun, repass_archive_reporting,
};
use archeion::report_words;
use archeion::storage::Archive;
use serde::Serialize;

use super::progress::{BarFacts, ProgressLevel, TickingBar};
use super::{warn, write_stdout};

/// The live progress a repass prints while it runs. There is no page limit and no deadline to
/// weigh it against, the way `capture`'s bar does: a repass walks an archive it already has, so
/// the one denominator it can know before the walk starts is how many items the walk found.
enum RepassProgress {
    /// One complete line per capture, naming the item and what its derived records became.
    Lines,
    /// One line about the whole walk, redrawn in place on a terminal and kept moving by a
    /// thread of its own, since a repass that fetches a missed subresource can wait on a host
    /// exactly as a capture does.
    Bar(TickingBar<RepassPace>),
}

/// How far a walk has got, as the bar states it. Items rather than captures, because the item
/// count is the only denominator the walk knows before it starts.
struct RepassPace {
    items_done: usize,
    total_items: usize,
    started: Instant,
    /// When the last item finished, or `None` while none has. See `CapturePace` in
    /// `cli::capture`: the two read differently on purpose.
    last_item: Option<Instant>,
}

impl BarFacts for RepassPace {
    fn line(&self) -> String {
        let since = match self.last_item {
            Some(last_item) => format!("{}s since the last item", last_item.elapsed().as_secs()),
            None => format!("{}s since the pass began", self.started.elapsed().as_secs()),
        };
        format!("{}/{} items, {since}", self.items_done, self.total_items)
    }
}

impl RepassProgress {
    fn new(level: ProgressLevel) -> Self {
        match level {
            ProgressLevel::Lines => Self::Lines,
            ProgressLevel::Bar => Self::Bar(TickingBar::new(
                io::stderr().is_terminal(),
                RepassPace {
                    items_done: 0,
                    total_items: 0,
                    started: Instant::now(),
                    last_item: None,
                },
            )),
        }
    }

    /// What the walk just said. A capture's own fate is the `Lines` level's whole question and
    /// nothing the bar counts; an item finishing is the bar's whole question and nothing
    /// `Lines` prints, since the URL was already named by the captures under it and an item
    /// with none has nothing to say. Splitting them is what lets an item the walk could not
    /// read at all still advance the bar to its own total.
    fn event(&mut self, event: RepassEvent<'_>) {
        match (self, event) {
            (Self::Lines, RepassEvent::Capture { url, outcome }) => {
                let _ = writeln!(io::stderr(), "{url}: {}", outcome.as_word());
            }
            (
                Self::Bar(bar),
                RepassEvent::ItemFinished {
                    items_done,
                    total_items,
                },
            ) => bar.advance(|pace| {
                pace.items_done = items_done;
                pace.total_items = total_items;
                pace.last_item = Some(Instant::now());
            }),
            _ => {}
        }
    }

    /// See `CaptureProgress::warn` in `cli::capture`: the same reasoning, over the same risk.
    fn warn(&mut self, lines: impl IntoIterator<Item = String>) {
        match self {
            Self::Lines => warn(lines),
            Self::Bar(bar) => bar.interrupt(lines),
        }
    }

    /// See `CaptureProgress::clear`.
    fn clear(&mut self) {
        if let Self::Bar(bar) = self {
            bar.clear();
        }
    }
}

#[derive(Debug, Serialize)]
struct Loss {
    url: String,
    capture: Option<String>,
    reason: String,
}

#[derive(Debug, Serialize)]
struct RepassReport {
    archive: String,
    captures_seen: usize,
    metadata_written: usize,
    articles_written: usize,
    extractions_refused: usize,
    non_articles_marked: usize,
    derived_unchanged: usize,
    assets_recovered: usize,
    asset_fetches: usize,
    assets_still_missing: usize,
    assets_not_retried: usize,
    unreadable_items: Vec<String>,
    unreadable_captures: Vec<Loss>,
    unreadable_bodies: Vec<Loss>,
    unreadable_pages: Vec<Loss>,
    unreadable_articles: Vec<Loss>,
}

pub fn repass(
    archive_path: PathBuf,
    allow_private_addresses: bool,
    progress: Option<ProgressLevel>,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let archive = Archive::open_existing(&archive_path)?;
    let (rules, unused_rules) = SiteRules::read(&archive.extraction_rules_path());

    let options = RepassOptions {
        allow_private_addresses,
    };
    let mut progress = progress.map(RepassProgress::new);
    // Held until there is a progress writer to print through rather than printed where it was
    // found, so a bar cannot be drawn over it. See the same move in `cli::capture`.
    emit_warnings(
        progress.as_mut(),
        unused_rules.iter().map(ToString::to_string),
    );
    let mut on_capture = |event: RepassEvent<'_>| {
        if let Some(progress) = progress.as_mut() {
            progress.event(event);
        }
    };
    let (run, failure) = match repass_archive_reporting(
        &SpiderEngine::default(),
        &archive,
        &rules,
        options,
        &mut on_capture,
    ) {
        Ok(run) => (run, None),
        Err(RepassError::Storage { source, run }) => (*run, Some(source)),
    };
    let report = report_of(&archive_path, &run);
    let output = if json {
        format!("{}\n", serde_json::to_string(&report)?)
    } else {
        human_report(&report)
    };
    // Before the report and not after it: see `finish` in `cli::capture`.
    if let Some(progress) = progress.as_mut() {
        progress.clear();
    }
    write_stdout(&output)?;
    // See `finish` in `cli::capture`: this can be the first thing printed since the bar last
    // redrew, so it goes through the bar's own line rather than straight to `warn`.
    emit_warnings(progress.as_mut(), losses(&report));

    if let Some(source) = failure {
        return Err(source.into());
    }
    if report_has_damage(&report) {
        return Err("archive has unreadable records the repass could not refresh".into());
    }
    Ok(())
}

/// See `emit_warnings` in `cli::capture`: the same reasoning, over this verb's own writer.
fn emit_warnings(progress: Option<&mut RepassProgress>, lines: impl IntoIterator<Item = String>) {
    match progress {
        Some(progress) => progress.warn(lines),
        None => warn(lines),
    }
}

fn report_of(path: &std::path::Path, run: &RepassRun) -> RepassReport {
    RepassReport {
        archive: path.display().to_string(),
        captures_seen: run.captures_seen,
        metadata_written: run.metadata_written,
        articles_written: run.articles_written,
        extractions_refused: run.extractions_refused,
        non_articles_marked: run.non_articles_marked,
        derived_unchanged: run.derived_unchanged,
        assets_recovered: run.assets_recovered,
        asset_fetches: run.asset_fetches,
        assets_still_missing: run.assets_still_missing,
        assets_not_retried: run.assets_not_retried,
        unreadable_items: run.unreadable_items.clone(),
        unreadable_captures: losses_of(&run.unreadable_captures),
        unreadable_bodies: losses_of(&run.unreadable_bodies),
        unreadable_pages: losses_of(&run.unreadable_pages),
        unreadable_articles: losses_of(&run.unreadable_articles),
    }
}

fn losses_of(losses: &[archeion::repass::RepassLoss]) -> Vec<Loss> {
    losses
        .iter()
        .map(|loss| Loss {
            url: loss.url.clone(),
            capture: loss.capture.clone(),
            reason: loss.reason.clone(),
        })
        .collect()
}

fn human_report(report: &RepassReport) -> String {
    let mut output = String::new();
    writeln!(
        output,
        "repassed {} capture(s) in {}",
        report.captures_seen, report.archive
    )
    .expect("writing to a string cannot fail");
    let rows = [
        (
            "metadata",
            format!("{} {}", report.metadata_written, report_words::WRITTEN),
        ),
        (
            "articles",
            format!(
                "{} {}, {} {}, {} {}",
                report.articles_written,
                report_words::WRITTEN,
                report.extractions_refused,
                report_words::REFUSED_IN_ARTICLES_ROW,
                report.non_articles_marked,
                report_words::NOT_ARTICLE
            ),
        ),
        (
            "assets",
            format!(
                "{} recovered, {} still missing, {} not retried, {} request(s)",
                report.assets_recovered,
                report.assets_still_missing,
                report.assets_not_retried,
                report.asset_fetches
            ),
        ),
        (
            report_words::UNCHANGED,
            format!("{} derived record(s)", report.derived_unchanged),
        ),
    ];
    for (label, value) in rows {
        writeln!(output, "  {label:<14}{value}").expect("writing to a string cannot fail");
    }
    output
}

fn losses(report: &RepassReport) -> Vec<String> {
    let mut warnings = Vec::new();
    warnings.extend(
        report
            .unreadable_items
            .iter()
            .map(|item| format!("unreadable item: {item}")),
    );
    warnings.extend(named_losses("capture", &report.unreadable_captures));
    warnings.extend(named_losses("body", &report.unreadable_bodies));
    warnings.extend(named_losses("markup", &report.unreadable_pages));
    warnings.extend(named_losses("article", &report.unreadable_articles));
    warnings
}

fn named_losses(kind: &str, losses: &[Loss]) -> Vec<String> {
    losses
        .iter()
        .map(|loss| match &loss.capture {
            Some(capture) => format!(
                "{kind} for {} capture {} could not be refreshed: {}",
                loss.url, capture, loss.reason
            ),
            None => format!(
                "{kind} for {} could not be refreshed: {}",
                loss.url, loss.reason
            ),
        })
        .collect()
}

fn report_has_damage(report: &RepassReport) -> bool {
    !report.unreadable_items.is_empty()
        || !report.unreadable_captures.is_empty()
        || !report.unreadable_bodies.is_empty()
}
