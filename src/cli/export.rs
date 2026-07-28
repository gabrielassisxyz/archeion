//! `archeion export`: the article captures of an archive, as a Markdown vault.

use std::error::Error;
use std::path::PathBuf;

use archeion::export::{ExportOptions, export_archive};
use archeion::storage::Archive;

use super::{damaged_archive, warn, write_stdout};

pub fn export(
    archive_path: PathBuf,
    destination: PathBuf,
    all_captures: bool,
) -> Result<(), Box<dyn Error>> {
    let archive = Archive::open_existing(archive_path)?;
    let report = export_archive(&archive, destination, ExportOptions { all_captures })?;

    write_stdout(&notes_written_line(report.notes_written))?;
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
