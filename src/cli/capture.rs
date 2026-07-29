//! `archeion capture`: a seed crawled into an archive.
//!
//! Every flag here is one field of the `Seed`, spelled the same, and the defaults the help
//! text prints are read off `Seed::new` rather than repeated. The execution policy is a set
//! of decisions with reasons attached, written down in `docs/crawl-boundary.md`, and a
//! command line that restated the numbers would be a second opinion about them.

use std::error::Error;
use std::fmt::{self, Display, Write as _};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use archeion::capture::{CaptureError, CaptureRun, capture_seed, capture_sitemap};
use archeion::crawl::{
    CrawlEngine, CrawlStop, DEFAULT_MAX_RESPONSE_BYTES, SMALLEST_MAX_RESPONSE_BYTES, Seed,
    SpiderEngine,
};
use archeion::readability::SiteRules;
use archeion::sitemap::{SitemapListing, read_sitemap};
use archeion::storage::{Archive, StorageError};
use serde::Serialize;

use super::{warn, write_stdout};

#[derive(Debug, clap::Args)]
pub struct CaptureArgs {
    /// Archive directory to write into. One is created when the path holds nothing yet.
    archive: PathBuf,
    /// Where the crawl starts. A seed is one host: subdomains and other TLDs of the same
    /// name are separate seeds, and a crawl never leaves the host it was pointed at.
    seed_url: String,
    /// How many pages this run may archive.
    #[arg(long, value_name = "N", default_value_t = defaults().max_pages,
          value_parser = clap::builder::RangedU64ValueParser::<u32>::new().range(1..))]
    max_pages: u32,
    /// How far from the seed links are followed. A URL `--from-sitemap` lists follows no
    /// further links of its own unless this is given explicitly: see that flag below.
    #[arg(long, value_name = "N", help = max_depth_help(),
          value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
    max_depth: Option<usize>,
    /// How many requests may be in flight against the host at once.
    #[arg(long, value_name = "N", default_value_t = defaults().concurrency,
          value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
    concurrency: usize,
    /// How long to wait between requests, which slows the crawl rather than bounding it.
    /// Zero is the only span here that means what it says: no wait.
    #[arg(long, value_name = "SPAN", default_value_t = Span(defaults().delay))]
    delay: Span,
    /// The wall clock the whole run gets, or `none` for a run that is deliberately unbounded.
    #[arg(long, value_name = "SPAN", default_value_t = Deadline(defaults().deadline),
          value_parser = a_span_a_run_can_live_inside::<Deadline>)]
    deadline: Deadline,
    /// How long one request may take before it counts as no response at all.
    #[arg(long, value_name = "SPAN", default_value_t = Span(defaults().request_timeout),
          value_parser = a_span_a_run_can_live_inside::<Span>)]
    request_timeout: Span,
    /// How many times a request that failed in a way worth repeating is repeated.
    #[arg(long, value_name = "N", default_value_t = defaults().max_retries)]
    max_retries: u8,
    /// Ceiling on the body of one response, in bytes. It is settled for the whole process
    /// before anything is fetched, because that is the only channel the engine offers.
    #[arg(long, value_name = "BYTES", help = response_ceiling_help(),
          value_parser = clap::builder::RangedU64ValueParser::<usize>::new()
              .range(SMALLEST_MAX_RESPONSE_BYTES as u64..))]
    max_response_bytes: Option<usize>,
    /// Let the run reach loopback, private ranges, link-local addresses and the names a
    /// cloud metadata service answers on. Off, so a URL cannot talk the archive into
    /// reading the machine it runs on or the network around it.
    #[arg(long)]
    allow_private_addresses: bool,
    /// Additionally archive what the site's sitemap lists, for a site whose pages do not
    /// link to each other. With no address, the sitemap named by a `Sitemap:` directive in
    /// `robots.txt` is read, falling back to `/sitemap.xml`. Nothing is followed from a
    /// listed URL unless `--max-depth` is also given explicitly.
    #[arg(long, value_name = "URL", num_args = 0..=1)]
    from_sitemap: Option<Option<String>>,
}

impl CaptureArgs {
    /// The ceiling this run chose, which `main` settles before anything else starts. It is
    /// read here rather than applied here because the write is only sound while the process
    /// is still one thread, and by the time a verb runs that is no longer something this
    /// module can promise.
    pub fn response_byte_ceiling(&self) -> Option<usize> {
        self.max_response_bytes
    }
}

/// The seed every default on this command line is read from, so a number changed in the
/// library is a number changed here rather than a second opinion about it.
fn defaults() -> Seed {
    Seed::new(String::new())
}

fn response_ceiling_help() -> String {
    format!("Ceiling on the body of one response, in bytes [default: {DEFAULT_MAX_RESPONSE_BYTES}]")
}

fn max_depth_help() -> String {
    format!(
        "How far from the seed links are followed [default: {}]",
        defaults().max_depth
    )
}

/// A budget that a run has to be able to happen inside, which is every span here except the
/// politeness delay.
///
/// Zero is refused rather than obeyed, for the reason the page and depth limits refuse it,
/// arrived at from the other direction. There it means no limit and a run asking for the
/// smallest crawl gets an unbounded one; here it is a limit nothing can fit in, so every
/// request expires before it is sent. The run then reports a page count of zero and a list
/// of URLs that answered nothing, which is what a site being down looks like, and leaves
/// with the code that says the web misbehaved. Being told the flag is impossible is the only
/// answer that is not a lie about the site.
fn a_span_a_run_can_live_inside<T>(text: &str) -> Result<T, String>
where
    T: FromStr<Err = String> + Budget,
{
    let span: T = text.parse()?;
    if span.is_zero() {
        return Err(format!(
            "{text} is a budget no request can finish inside, so every one of them would \
             be reported as a server that answered nothing"
        ));
    }
    Ok(span)
}

/// Whether a span leaves a run any room at all. It is a trait rather than two functions
/// because the deadline carries the extra answer of having no budget on purpose, which is
/// not the same thing as a budget of nothing.
trait Budget {
    fn is_zero(&self) -> bool;
}

impl Budget for Span {
    fn is_zero(&self) -> bool {
        self.0.is_zero()
    }
}

impl Budget for Deadline {
    /// `none` is a run that asked for no ceiling, which is a decision rather than a budget
    /// of nothing, so it passes.
    fn is_zero(&self) -> bool {
        self.0.is_some_and(|budget| budget.is_zero())
    }
}

/// A span of time as the command line spells it: `250ms`, `30s`, `5m`, `1h`.
///
/// The unit is required. A bare number would have to mean seconds on one flag and
/// milliseconds on another, and a run whose deadline was read in the wrong unit is either
/// over before it starts or not over at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span(Duration);

/// The wall clock a run gets, including the decision that it gets none. `None` is a run that
/// is deliberately unbounded, which is a thing to say out loud rather than to reach by
/// leaving a flag alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Deadline(Option<Duration>);

impl FromStr for Span {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let digits = text
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(text.len());
        let (amount, unit) = text.split_at(digits);
        let Ok(amount) = amount.parse::<u64>() else {
            return Err(format!(
                "{text} is not a length of time: write it as 250ms, 30s, 5m or 1h"
            ));
        };
        let millis = match unit {
            "ms" => Some(amount),
            "s" => amount.checked_mul(1_000),
            "m" => amount.checked_mul(60_000),
            "h" => amount.checked_mul(3_600_000),
            "" => {
                return Err(format!(
                    "{text} has no unit: write it as {text}s or {text}m"
                ));
            }
            other => return Err(format!("{other} is not a unit of time: use ms, s, m or h")),
        };
        millis
            .map(|millis| Self(Duration::from_millis(millis)))
            .ok_or_else(|| format!("{text} is longer than this program can count"))
    }
}

/// Renders back into the vocabulary the flag was written in, since this is what the help
/// text prints as the default and a default nobody could type is a default nobody can adjust.
impl Display for Span {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let millis = self.0.as_millis();
        match millis {
            0 => write!(out, "0s"),
            _ if millis.is_multiple_of(3_600_000) => write!(out, "{}h", millis / 3_600_000),
            _ if millis.is_multiple_of(60_000) => write!(out, "{}m", millis / 60_000),
            _ if millis.is_multiple_of(1_000) => write!(out, "{}s", millis / 1_000),
            _ => write!(out, "{millis}ms"),
        }
    }
}

impl FromStr for Deadline {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text == "none" {
            return Ok(Self(None));
        }
        match text.parse::<Span>() {
            Ok(span) => Ok(Self(Some(span.0))),
            Err(reason) => Err(format!("{reason}, or none for a run with no deadline")),
        }
    }
}

impl Display for Deadline {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(budget) => Span(budget).fmt(out),
            None => write!(out, "none"),
        }
    }
}

pub fn capture(args: CaptureArgs, json: bool) -> Result<(), Box<dyn Error>> {
    let seed = seed_of(&args);
    // Before the archive, because opening one creates it. A seed the engine will not dial is
    // a run that was never going to fetch anything, and creating a directory for it leaves
    // an empty archive on the path that was typed wrong, which is exactly the case the line
    // announcing a new archive exists to make visible.
    let engine = SpiderEngine;
    engine.check_seed(&seed)?;
    let (archive, created) = open_or_create(&args.archive)?;
    // A rule file that cannot be used costs the extractions it would have improved and not
    // the capture, so it is a warning here rather than a reason to refuse the run: the
    // response is the part that cannot be fetched again, and a better reading of it can be
    // produced later from what is on disk.
    let (rules, unused_rules) = SiteRules::read(&archive.extraction_rules_path());
    warn(unused_rules.iter().map(ToString::to_string));

    let (mut run, mut failure) = match capture_seed(&engine, &archive, &seed, &rules) {
        Ok(run) => (run, None),
        // The report is the point of carrying the run inside the error: the archive holds
        // whatever was written before the disk refused, and a caller told only that a write
        // failed has to go looking for the rest.
        Err(CaptureError::Storage { source, run }) => (*run, Some(source)),
        Err(other) => return Err(other.into()),
    };

    // A disk that already refused one write will refuse the next, so the sitemap phase is
    // skipped once the ordinary crawl already failed for that reason: there is nothing left
    // to spend more requests archiving into.
    let (sitemap_report, sitemap_warning) = if failure.is_none() {
        sitemap_phase(
            &args,
            &engine,
            &archive,
            &seed,
            &rules,
            &mut run,
            &mut failure,
        )
    } else {
        (None, None)
    };

    let report = report_of(&args, &run, created, sitemap_report);
    let output = if json {
        format!("{}\n", serde_json::to_string(&report)?)
    } else {
        human_report(&report, run.stopped)
    };
    write_stdout(&output)?;
    warn(sitemap_warning.into_iter().chain(losses(&run)));

    if let Some(source) = failure {
        return Err(source.into());
    }
    // Pages the engine fetched and the archive never got. Every other shortfall above is the
    // web being the web; this one is bytes that were spent on somebody else's host for a
    // record that does not exist, which is the loss this archive can least afford to report
    // as a success.
    if run.pages_dropped > 0 {
        return Err(format!(
            "{} page(s) the crawl fetched never reached the archive",
            run.pages_dropped
        )
        .into());
    }
    // A link the crawl found and never fetched is the same failure from the other side:
    // the frontier lost it before a request was ever made, and a run that says it
    // exhausted the seed while one of these is sitting in the report would be lying.
    if !run.links_never_followed.is_empty() {
        return Err(format!(
            "{} link(s) the crawl discovered were never fetched",
            run.links_never_followed.len()
        )
        .into());
    }
    Ok(())
}

fn seed_of(args: &CaptureArgs) -> Seed {
    let mut seed = Seed::new(args.seed_url.clone());
    seed.max_pages = args.max_pages;
    seed.max_depth = args.max_depth.unwrap_or_else(|| defaults().max_depth);
    seed.concurrency = args.concurrency;
    seed.delay = args.delay.0;
    seed.deadline = args.deadline.0;
    seed.request_timeout = args.request_timeout.0;
    seed.max_retries = args.max_retries;
    seed.allow_private_addresses = args.allow_private_addresses;
    seed
}

/// Opens the archive, bringing one into existence when the path holds nothing yet.
///
/// `capture` is the only verb that writes, so it is the only one that may create an archive,
/// and it says when it did. A path typed wrong would otherwise be a new empty archive nobody
/// was told about, found later by someone wondering where their collection went.
fn open_or_create(path: &Path) -> Result<(Archive, bool), StorageError> {
    match Archive::open_existing(path) {
        Ok(archive) => Ok((archive, false)),
        Err(StorageError::MissingArchive { .. } | StorageError::NoArchiveMarker { .. }) => {
            Archive::open(path).map(|archive| (archive, true))
        }
        Err(other) => Err(other),
    }
}

/// What one URL cost the run and why. Four of the five shortfalls a run reports have this
/// shape already, so the machine readable answer keeps it rather than inventing one per kind.
#[derive(Debug, Serialize)]
struct Loss {
    url: String,
    reason: String,
}

/// Reads the sitemap this run was asked for and archives what it lists, additionally to the
/// ordinary crawl already run from the seed.
///
/// A sitemap that cannot be found or parsed is reported as a warning rather than treated as a
/// reason to end the run: the ordinary crawl's captures are already written, and a run that
/// turned those into a failure over a sitemap it could not read would be throwing away a
/// working archive over a page that happens not to be one. A write failure inside this phase
/// is the one thing that still matters to the caller, and it is folded into the same run the
/// ordinary crawl already produced rather than reported on its own.
fn sitemap_phase(
    args: &CaptureArgs,
    engine: &dyn CrawlEngine,
    archive: &Archive,
    seed: &Seed,
    rules: &SiteRules,
    run: &mut CaptureRun,
    failure: &mut Option<StorageError>,
) -> (Option<SitemapReport>, Option<String>) {
    let Some(requested) = &args.from_sitemap else {
        return (None, None);
    };
    let listing = match read_sitemap(engine, seed, requested.as_deref()) {
        Ok(listing) => listing,
        Err(error) => return (None, Some(error.to_string())),
    };
    // Given explicitly, the same depth that already bounds the ordinary crawl now also
    // bounds how far a sitemap URL is traversed from; left alone, a sitemap URL is fetched
    // on its own, since a depth bound has no meaning for a page nobody linked to.
    let follow_links = args.max_depth.is_some();
    let report = SitemapReport::from(&listing);
    match capture_sitemap(engine, archive, seed, rules, &listing.urls, follow_links) {
        Ok(sitemap_run) => {
            run.merge(sitemap_run);
            (Some(report), None)
        }
        Err(CaptureError::Storage { source, run: extra }) => {
            run.merge(*extra);
            *failure = Some(source);
            (Some(report), None)
        }
        Err(CaptureError::Crawl(error)) => (
            Some(report),
            Some(format!("the sitemap's URLs could not be crawled: {error}")),
        ),
    }
}

/// What `--from-sitemap` found, kept small on purpose: a sitemap listing 247 URLs against a
/// run that archived 200 of them is exactly the gap this field exists to make visible.
#[derive(Debug, Serialize)]
struct SitemapReport {
    sitemap_url: String,
    urls_listed: usize,
    urls_taken: usize,
    urls_refused: usize,
}

impl From<&SitemapListing> for SitemapReport {
    fn from(listing: &SitemapListing) -> Self {
        Self {
            sitemap_url: listing.sitemap_url.clone(),
            urls_listed: listing.urls_listed,
            urls_taken: listing.urls.len(),
            urls_refused: listing.refused_off_host + listing.refused_over_ceiling,
        }
    }
}

/// The run as this command line publishes it.
///
/// It is declared here rather than serialized off `CaptureRun` so that the library's report
/// stays free to grow a field without that field becoming a promise to every script reading
/// this output.
#[derive(Debug, Serialize)]
struct CaptureReport {
    seed_url: String,
    archive: String,
    archive_created: bool,
    captures_written: usize,
    articles_extracted: usize,
    extractions_refused: usize,
    assets_stored: usize,
    assets_missed: usize,
    asset_fetches: usize,
    pages_dropped: usize,
    links_never_followed: Vec<String>,
    stopped: &'static str,
    sitemap: Option<SitemapReport>,
    failed_fetches: Vec<Loss>,
    unaddressable_pages: Vec<Loss>,
    pages_inside_a_network: Vec<String>,
    unreadable_pages: Vec<Loss>,
    unreadable_articles: Vec<Loss>,
}

fn report_of(
    args: &CaptureArgs,
    run: &CaptureRun,
    created: bool,
    sitemap: Option<SitemapReport>,
) -> CaptureReport {
    CaptureReport {
        seed_url: args.seed_url.clone(),
        archive: args.archive.display().to_string(),
        archive_created: created,
        captures_written: run.captures_written,
        articles_extracted: run.articles_extracted,
        extractions_refused: run.extractions_refused,
        assets_stored: run.assets_stored,
        assets_missed: run.assets_missed,
        asset_fetches: run.asset_fetches,
        pages_dropped: run.pages_dropped,
        links_never_followed: run.links_never_followed.clone(),
        stopped: stop_name(run.stopped),
        sitemap,
        failed_fetches: run
            .failed_fetches
            .iter()
            .map(|failure| Loss {
                url: failure.url.clone(),
                reason: failure.reason.clone(),
            })
            .collect(),
        unaddressable_pages: run
            .unaddressable_pages
            .iter()
            .map(|page| Loss {
                url: page.url.clone(),
                reason: page.reason.to_string(),
            })
            .collect(),
        pages_inside_a_network: run.pages_inside_a_network.clone(),
        unreadable_pages: run
            .unreadable_pages
            .iter()
            .map(|page| Loss {
                url: page.url.clone(),
                reason: page.reason.clone(),
            })
            .collect(),
        unreadable_articles: run
            .unreadable_articles
            .iter()
            .map(|article| Loss {
                url: article.url.clone(),
                reason: article.reason.clone(),
            })
            .collect(),
    }
}

fn stop_name(stopped: CrawlStop) -> &'static str {
    match stopped {
        CrawlStop::Exhausted => "exhausted",
        CrawlStop::DeadlineReached => "deadline-reached",
        CrawlStop::CallerStopped => "stopped-by-the-archive",
    }
}

/// The same three answers a person reads. It is a second exhaustive match rather than a
/// lookup on the name above, so a stop that gets added is a stop this file fails to compile
/// without rather than one that quietly reads as an ordinary end.
fn stop_sentence(stopped: CrawlStop) -> &'static str {
    match stopped {
        CrawlStop::Exhausted => "nothing was left to fetch",
        CrawlStop::DeadlineReached => "the seed's deadline ran out",
        CrawlStop::CallerStopped => "the archive ended the run",
    }
}

fn human_report(report: &CaptureReport, stopped: CrawlStop) -> String {
    let mut output = String::new();
    if report.archive_created {
        writeln!(output, "created an archive at {}", report.archive)
            .expect("writing to a string cannot fail");
    }
    writeln!(
        output,
        "archived {} capture(s) from {} into {}",
        report.captures_written, report.seed_url, report.archive
    )
    .expect("writing to a string cannot fail");

    let rows = [
        (
            "articles",
            format!(
                "{} extracted, {} refused",
                report.articles_extracted, report.extractions_refused
            ),
        ),
        (
            "assets",
            format!(
                "{} stored, {} missed, {} request(s)",
                report.assets_stored, report.assets_missed, report.asset_fetches
            ),
        ),
        ("pages dropped", report.pages_dropped.to_string()),
        ("links lost", report.links_never_followed.len().to_string()),
        ("stopped", stop_sentence(stopped).to_owned()),
    ];
    for (label, value) in rows {
        writeln!(output, "  {label:<14}{value}").expect("writing to a string cannot fail");
    }
    if let Some(sitemap) = &report.sitemap {
        writeln!(
            output,
            "  {:<14}{} taken, {} refused, {} listed",
            "sitemap", sitemap.urls_taken, sitemap.urls_refused, sitemap.urls_listed
        )
        .expect("writing to a string cannot fail");
    }
    output
}

/// Every URL the run did not archive, said one per line. They are on stderr rather than in
/// the report above because the run is a result and these are a queue somebody reads: a
/// pipeline consuming the records still gets told, and the records stay parseable.
fn losses(run: &CaptureRun) -> Vec<String> {
    let failed = run
        .failed_fetches
        .iter()
        .map(|failure| format!("no response from {}: {}", failure.url, failure.reason));
    let unaddressable = run.unaddressable_pages.iter().map(|page| {
        format!(
            "{} has no address in this archive: {}",
            page.url, page.reason
        )
    });
    let internal = run
        .pages_inside_a_network
        .iter()
        .map(|url| format!("{url} is inside a network and was not stored"));
    let unreadable_markup = run.unreadable_pages.iter().map(|page| {
        format!(
            "the markup of {} could not be read: {}",
            page.url, page.reason
        )
    });
    let unreadable_prose = run.unreadable_articles.iter().map(|article| {
        format!(
            "the prose of {} could not be read: {}",
            article.url, article.reason
        )
    });
    let never_followed = run
        .links_never_followed
        .iter()
        .map(|url| format!("{url} was discovered and never fetched"));
    failed
        .chain(unaddressable)
        .chain(internal)
        .chain(unreadable_markup)
        .chain(unreadable_prose)
        .chain(never_followed)
        .collect()
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::*;

    /// The parse has to go through the whole command line, because what is being checked is
    /// that a flag reaches the field it names, and a struct built by hand would only check
    /// that this test agrees with itself.
    #[derive(Debug, clap::Parser)]
    struct OnlyCapture {
        #[command(flatten)]
        args: CaptureArgs,
    }

    fn parse(flags: &[&str]) -> CaptureArgs {
        let mut argv = vec!["archeion", "/tmp/archive", "https://example.com/"];
        argv.extend_from_slice(flags);
        OnlyCapture::parse_from(argv).args
    }

    fn refuse(flags: &[&str]) -> String {
        let mut argv = vec!["archeion", "/tmp/archive", "https://example.com/"];
        argv.extend_from_slice(flags);
        OnlyCapture::try_parse_from(argv)
            .expect_err("the flag was accepted")
            .to_string()
    }

    #[test]
    fn a_seed_with_no_flags_is_the_seed_the_library_would_have_built() {
        let seed = seed_of(&parse(&[]));
        let library = Seed::new("https://example.com/");

        assert_eq!(seed.max_pages, library.max_pages);
        assert_eq!(seed.max_depth, library.max_depth);
        assert_eq!(seed.concurrency, library.concurrency);
        assert_eq!(seed.delay, library.delay);
        assert_eq!(seed.deadline, library.deadline);
        assert_eq!(seed.request_timeout, library.request_timeout);
        assert_eq!(seed.max_retries, library.max_retries);
        assert_eq!(
            seed.allow_private_addresses,
            library.allow_private_addresses
        );
    }

    #[test]
    fn every_flag_lands_on_the_seed_field_it_names() {
        let seed = seed_of(&parse(&[
            "--max-pages",
            "12",
            "--max-depth",
            "3",
            "--concurrency",
            "2",
            "--delay",
            "250ms",
            "--deadline",
            "90s",
            "--request-timeout",
            "5s",
            "--max-retries",
            "0",
            "--allow-private-addresses",
        ]));

        assert_eq!(seed.url, "https://example.com/");
        assert_eq!(seed.max_pages, 12);
        assert_eq!(seed.max_depth, 3);
        assert_eq!(seed.concurrency, 2);
        assert_eq!(seed.delay, Duration::from_millis(250));
        assert_eq!(seed.deadline, Some(Duration::from_secs(90)));
        assert_eq!(seed.request_timeout, Duration::from_secs(5));
        assert_eq!(seed.max_retries, 0);
        assert!(seed.allow_private_addresses);
    }

    /// The engine reads a zero as no limit at all, so a command line that passed one through
    /// would answer a request for the smallest possible crawl with an unbounded one.
    #[test]
    fn a_limit_of_zero_is_refused_rather_than_read_as_no_limit() {
        for flag in ["--max-pages", "--max-depth", "--concurrency"] {
            let message = refuse(&[flag, "0"]);
            assert!(
                message.contains("not in 1.."),
                "{flag} 0 was refused with: {message}"
            );
        }
    }

    #[test]
    fn a_span_without_a_unit_is_refused_rather_than_guessed() {
        assert!(refuse(&["--deadline", "300"]).contains("has no unit"));
        assert!(refuse(&["--delay", "10"]).contains("has no unit"));
        assert!(refuse(&["--request-timeout", "1 minute"]).contains("not a unit of time"));
        assert!(refuse(&["--deadline", "forever"]).contains("none for a run with no deadline"));
    }

    #[test]
    fn a_span_reads_back_as_what_it_was_written_as() {
        for spelling in ["250ms", "30s", "5m", "2h", "0s"] {
            let span: Span = spelling.parse().expect("a span the flags accept");
            assert_eq!(span.to_string(), spelling);
        }
    }

    /// The defaults in the help text are the seed's, and the only way they can be printed is
    /// through this rendering, so a span the help shows has to be a span the flag accepts.
    #[test]
    fn the_defaults_the_help_prints_are_defaults_the_flags_accept() {
        let library = Seed::new("https://example.com/");

        assert_eq!(
            Span(library.request_timeout).to_string().parse::<Span>(),
            Ok(Span(library.request_timeout))
        );
        assert_eq!(
            Deadline(library.deadline).to_string().parse::<Deadline>(),
            Ok(Deadline(library.deadline))
        );
        assert_eq!(
            Span(library.delay).to_string().parse::<Span>(),
            Ok(Span(library.delay))
        );
    }

    /// The other direction of the same mistake. A budget of zero is a limit no request can
    /// finish inside, so every URL would be reported as a server that answered nothing, which
    /// is indistinguishable from the site being down and leaves with a code saying so.
    #[test]
    fn a_budget_no_request_could_finish_inside_is_refused() {
        for flag in ["--deadline", "--request-timeout"] {
            let message = refuse(&[flag, "0s"]);
            assert!(
                message.contains("no request can finish inside"),
                "{flag} 0s was refused with: {message}"
            );
        }
    }

    /// The politeness delay is the one span where zero is the ordinary answer: it is a wait
    /// between requests rather than a budget one has to fit in, and the library's own default
    /// is exactly that.
    #[test]
    fn a_delay_of_zero_is_the_default_and_stays_accepted() {
        assert_eq!(seed_of(&parse(&["--delay", "0s"])).delay, Duration::ZERO);
    }

    /// A run that asked for no deadline is making a decision, not asking for a budget of
    /// nothing, so the guard above has to let it through.
    #[test]
    fn a_run_with_no_deadline_is_not_mistaken_for_one_with_an_empty_budget() {
        assert_eq!(seed_of(&parse(&["--deadline", "none"])).deadline, None);
    }

    /// The engine raises anything under a megabyte to a megabyte without saying so, so a
    /// smaller number accepted here would be a ceiling the flag reports and no run applies.
    #[test]
    fn the_response_ceiling_is_the_run_s_to_choose_and_absent_by_default() {
        assert_eq!(parse(&[]).response_byte_ceiling(), None);
        assert_eq!(
            parse(&["--max-response-bytes", "4194304"]).response_byte_ceiling(),
            Some(4 * 1024 * 1024)
        );
        for below in ["0", "4096", &(SMALLEST_MAX_RESPONSE_BYTES - 1).to_string()] {
            let message = refuse(&["--max-response-bytes", below]);
            assert!(
                message.contains(&format!("not in {SMALLEST_MAX_RESPONSE_BYTES}..")),
                "{below} was refused with: {message}"
            );
        }
    }

    /// The three states `--from-sitemap` can be in: left alone, given with nothing after it,
    /// and given a specific address. The middle one is what asks for discovery rather than
    /// naming a sitemap directly, and only the parse can prove clap actually tells it apart
    /// from the other two.
    #[test]
    fn from_sitemap_tells_absent_bare_and_addressed_apart() {
        assert_eq!(parse(&[]).from_sitemap, None);
        assert_eq!(parse(&["--from-sitemap"]).from_sitemap, Some(None));
        assert_eq!(
            parse(&["--from-sitemap", "https://example.com/sitemap.xml"]).from_sitemap,
            Some(Some("https://example.com/sitemap.xml".to_owned()))
        );
    }

    /// A depth nobody typed has to still be the library's own default, since that is the
    /// number every other test here assumes a bare seed crawls with. Whether it was typed at
    /// all is what `--from-sitemap` needs to tell apart from a depth of two nobody asked for.
    #[test]
    fn a_max_depth_nobody_typed_is_the_library_s_own_default() {
        assert_eq!(parse(&[]).max_depth, None);
        assert_eq!(
            seed_of(&parse(&[])).max_depth,
            Seed::new("https://example.com/").max_depth
        );
        assert_eq!(parse(&["--max-depth", "3"]).max_depth, Some(3));
    }
}
