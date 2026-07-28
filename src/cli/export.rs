//! `archeion export`: the article captures of an archive, as a Markdown vault.

use std::error::Error;
use std::path::PathBuf;

use archeion::export::{ExportOptions, export_archive};
use archeion::storage::Archive;
use serde::Serialize;

use super::{damaged_archive, warn, write_stdout};

/// One object and not one per note: an export is a report on a run, while a listing is a
/// collection someone reads line by line.
#[derive(Debug, Serialize)]
struct ExportReport {
    notes_written: usize,
    unreadable: Vec<String>,
}

pub fn export(
    archive_path: PathBuf,
    destination: PathBuf,
    all_captures: bool,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let archive = Archive::open_existing(archive_path)?;
    let report = export_archive(&archive, destination, ExportOptions { all_captures })?;
    let report = ExportReport {
        notes_written: report.notes_written,
        unreadable: report.unreadable,
    };

    let output = if json {
        format!("{}\n", serde_json::to_string(&report)?)
    } else {
        notes_written_line(report.notes_written)
    };
    write_stdout(&output)?;
    warn(report.unreadable.iter().cloned());

    if report.unreadable.is_empty() {
        Ok(())
    } else {
        Err(damaged_archive(report.unreadable.len()).into())
    }
}

fn notes_written_line(notes: usize) -> String {
    let noun = if notes == 1 { "note" } else { "notes" };
    format!("exported {notes} {noun}\n")
}
