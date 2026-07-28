//! The command line: what the verbs are, and what leaving with a code means.

mod cli;

use std::error::Error;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

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

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    match cli.command {
        Command::List { json, archive } => cli::list::list(archive, json),
        Command::Export {
            all_captures,
            archive,
            destination,
        } => cli::export::export(archive, destination, all_captures),
    }
}
