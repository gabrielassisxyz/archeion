use std::error::Error;
use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::path::PathBuf;

use archeion::export::{ExportOptions, export_archive};
use archeion::storage::Archive;
use clap::{Parser, Subcommand};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List the items stored in an archive.
    List {
        /// Emit one JSON object per item.
        #[arg(long)]
        json: bool,
        /// Archive directory to read.
        archive: PathBuf,
    },
    /// Export article captures as a Markdown vault.
    Export {
        /// Export every article capture instead of only the latest capture per item.
        #[arg(long)]
        all_captures: bool,
        /// Archive directory to read.
        archive: PathBuf,
        /// Empty or absent directory to write the vault into.
        destination: PathBuf,
    },
}

#[derive(Debug, Serialize)]
struct ListRow {
    canonical_url: String,
    captures: usize,
    last_captured_at: Option<String>,
    has_article: bool,
}

#[derive(Debug, Default)]
struct ListedArchive {
    rows: Vec<ListRow>,
    unreadable: Vec<String>,
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    match cli.command {
        Command::List { json, archive } => list_archive(archive, json),
        Command::Export {
            all_captures,
            archive,
            destination,
        } => export_markdown_vault(archive, destination, all_captures),
    }
}

fn export_markdown_vault(
    archive_path: PathBuf,
    destination: PathBuf,
    all_captures: bool,
) -> Result<(), Box<dyn Error>> {
    let archive = Archive::open_existing(archive_path)?;
    let report = export_archive(&archive, destination, ExportOptions { all_captures })?;
    write_stdout(&export_report_line(report.notes_written))?;
    for unreadable in &report.unreadable {
        eprintln!("warning: {unreadable}");
    }
    if report.unreadable.is_empty() {
        Ok(())
    } else {
        Err(format!("archive has {} unreadable item(s)", report.unreadable.len()).into())
    }
}

fn list_archive(path: PathBuf, json: bool) -> Result<(), Box<dyn Error>> {
    let archive = Archive::open_existing(path)?;
    let walk = archive.walk()?;
    let mut listed = list_rows(&archive, &walk.items);
    listed
        .unreadable
        .extend(walk.unreadable.iter().map(ToString::to_string));

    let output = if json {
        json_lines(&listed.rows)?
    } else {
        table(&listed.rows)
    };
    write_stdout(&output)?;

    for unreadable in &listed.unreadable {
        eprintln!("warning: {unreadable}");
    }
    if listed.unreadable.is_empty() {
        Ok(())
    } else {
        Err(format!("archive has {} unreadable item(s)", listed.unreadable.len()).into())
    }
}

fn list_rows(archive: &Archive, items: &[archeion::storage::Item]) -> ListedArchive {
    let mut listed = ListedArchive {
        rows: Vec::with_capacity(items.len()),
        unreadable: Vec::new(),
    };
    for item in items {
        let captures = match archive.list_captures(&item.canonical_url) {
            Ok(captures) => captures,
            Err(error) => {
                listed
                    .unreadable
                    .push(format!("{}: {error}", item.canonical_url));
                continue;
            }
        };
        let has_article = match captures.last() {
            Some(capture) => match archive.read_article(&item.canonical_url, capture) {
                Ok(article) => article.is_some(),
                Err(error) => {
                    listed
                        .unreadable
                        .push(format!("{}: {error}", item.canonical_url));
                    continue;
                }
            },
            None => false,
        };
        listed.rows.push(ListRow {
            canonical_url: item.canonical_url.to_string(),
            captures: captures.len(),
            last_captured_at: captures.last().map(|_| item.last_captured_at.to_string()),
            has_article,
        });
    }
    listed
}

fn json_lines(rows: &[ListRow]) -> Result<String, serde_json::Error> {
    let mut output = String::new();
    for row in rows {
        output.push_str(&serde_json::to_string(row)?);
        output.push('\n');
    }
    Ok(output)
}

fn export_report_line(notes: usize) -> String {
    let noun = if notes == 1 { "note" } else { "notes" };
    format!("exported {notes} {noun}\n")
}

fn table(rows: &[ListRow]) -> String {
    let url_width = rows
        .iter()
        .map(|row| row.canonical_url.len())
        .max()
        .unwrap_or(0)
        .max("URL".len());
    let captures_width = rows
        .iter()
        .map(|row| row.captures.to_string().len())
        .max()
        .unwrap_or(0)
        .max("CAPTURES".len());
    let last_width = rows
        .iter()
        .filter_map(|row| row.last_captured_at.as_ref())
        .map(String::len)
        .max()
        .unwrap_or(0)
        .max("LAST_CAPTURED_AT".len());

    let mut output = String::new();
    writeln!(
        output,
        "{:<url_width$}  {:>captures_width$}  {:<last_width$}  ARTICLE",
        "URL", "CAPTURES", "LAST_CAPTURED_AT"
    )
    .expect("writing to a string cannot fail");
    for row in rows {
        writeln!(
            output,
            "{:<url_width$}  {:>captures_width$}  {:<last_width$}  {}",
            row.canonical_url,
            row.captures,
            row.last_captured_at.as_deref().unwrap_or(""),
            if row.has_article { "yes" } else { "no" }
        )
        .expect("writing to a string cannot fail");
    }
    output
}

fn write_stdout(output: &str) -> io::Result<()> {
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    match stdout
        .write_all(output.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error),
    }
}
