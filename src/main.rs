//! The command line: what the verbs are, and what leaving with a code means.

mod cli;

use std::error::Error;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Published because a script reading the records has no other way to tell a short answer
/// from a complete one: a walk that skipped a damaged item still prints every item it could
/// read, and the code is the only part of that which is not easy to miss.
const EXIT_CODES: &str = "\
Exit codes:
  0  the command did what it was asked
  1  the archive is missing, damaged, or could not be written to
  2  the command line could not be read";

#[derive(Debug, Parser)]
#[command(version, about, after_help = EXIT_CODES)]
struct Cli {
    /// Answer with records rather than with a table: one JSON object per item for `list`,
    /// and one object for the run for `export`.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List the items stored in an archive.
    List {
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

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    match cli.command {
        Command::List { archive } => cli::list::list(archive, cli.json),
        Command::Export {
            all_captures,
            archive,
            destination,
        } => cli::export::export(archive, destination, all_captures, cli.json),
    }
}
