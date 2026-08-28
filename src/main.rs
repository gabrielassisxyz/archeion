//! The command line: what the verbs are, and what leaving with a code means.

mod cli;

use std::error::Error;
use std::path::PathBuf;

use archeion::crawl::settle_response_byte_ceiling;
use clap::{Parser, Subcommand};

use cli::capture::CaptureArgs;

/// The whole point of publishing these is that a script can tell the two failures apart. An
/// archive that came up short is a reason to stop a pipeline; a URL nobody answered is the
/// web, and a run that reported it did its job.
const EXIT_CODES: &str = "\
Exit codes:
  0  the command did what it was asked
  1  the archive is missing or damaged, a seed was refused, a write failed, a
     run ended up holding less than it fetched, or the crawl discovered a
     link it never fetched at all
  2  the command line could not be read";

#[derive(Debug, Parser)]
#[command(version, about, after_help = EXIT_CODES)]
struct Cli {
    /// Answer with records rather than with a table: one JSON object per item for `list`,
    /// and one object for the run for `capture`, `repass` and `export`.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Crawl a seed into an archive.
    Capture(CaptureArgs),
    /// Re-read stored captures and refresh their derived records.
    ///
    /// There is no `--cookie-file` here, and it is not an omission. A credential is bound to the
    /// origin of the seed that was typed, and a repass has no seed: it walks an archive that may
    /// hold captures of any number of hosts, so there is nothing for a binding to come from. A
    /// flag here would have to carry its own origin, which is a second surface for a need nobody
    /// has yet: what a repass retries is a subresource, and a page behind a paywall is refetched
    /// by capturing it again.
    Repass {
        /// Let recovered subresources reach addresses that exist only inside a network.
        #[arg(long)]
        allow_private_addresses: bool,
        /// Archive directory to update.
        archive: PathBuf,
    },
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
    let cli = Cli::parse();

    if let Command::Capture(args) = &cli.command
        && let Some(bytes) = args.response_byte_ceiling()
    {
        // SAFETY: this process is still the one thread that started it. Nothing has run but
        // the argument parse, and the engine builds the runtime it fetches on further in,
        // which is also the last moment the ceiling could be settled: the engine reads the
        // environment on its first fetch and keeps that value for the life of the process.
        unsafe { settle_response_byte_ceiling(bytes) };
    }

    if let Err(error) = run(cli) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    match cli.command {
        Command::Capture(args) => cli::capture::capture(args, cli.json),
        Command::Repass {
            allow_private_addresses,
            archive,
        } => cli::repass::repass(archive, allow_private_addresses, cli.json),
        Command::List { archive } => cli::list::list(archive, cli.json),
        Command::Export {
            all_captures,
            archive,
            destination,
        } => cli::export::export(archive, destination, all_captures, cli.json),
    }
}
