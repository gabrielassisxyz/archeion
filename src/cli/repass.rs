//! `archeion repass`: derived records refreshed from captures already on disk.

use std::error::Error;
use std::fmt::Write as _;
use std::path::PathBuf;

use archeion::crawl::SpiderEngine;
use archeion::readability::SiteRules;
use archeion::repass::{RepassError, RepassOptions, RepassRun, repass_archive};
use archeion::storage::Archive;
use serde::Serialize;

use super::{warn, write_stdout};

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
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let archive = Archive::open_existing(&archive_path)?;
    let (rules, unused_rules) = SiteRules::read(&archive.extraction_rules_path());
    warn(unused_rules.iter().map(ToString::to_string));

    let options = RepassOptions {
        allow_private_addresses,
    };
    let (run, failure) = match repass_archive(&SpiderEngine::default(), &archive, &rules, options) {
        Ok(run) => (run, None),
        Err(RepassError::Storage { source, run }) => (*run, Some(source)),
    };
    let report = report_of(&archive_path, &run);
    let output = if json {
        format!("{}\n", serde_json::to_string(&report)?)
    } else {
        human_report(&report)
    };
    write_stdout(&output)?;
    warn(losses(&report));

    if let Some(source) = failure {
        return Err(source.into());
    }
    if report_has_damage(&report) {
        return Err("archive has unreadable records the repass could not refresh".into());
    }
    Ok(())
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
        ("metadata", format!("{} written", report.metadata_written)),
        (
            "articles",
            format!(
                "{} written, {} refused, {} not article",
                report.articles_written, report.extractions_refused, report.non_articles_marked
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
            "unchanged",
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
