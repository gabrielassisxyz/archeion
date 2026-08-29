//! `archeion capture`: a seed crawled into an archive.
//!
//! Every flag here is one field of the `Seed`, spelled the same, and the defaults the help
//! text prints are read off `Seed::new` rather than repeated. The execution policy is a set
//! of decisions with reasons attached, written down in `docs/crawl-boundary.md`, and a
//! command line that restated the numbers would be a second opinion about them.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{self, Display, Write as _};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use archeion::CanonicalUrl;
use archeion::capture::{CaptureError, CaptureRun, RunSoFar, capture_seed, capture_sitemap};
use archeion::crawl::{
    CrawlEngine, CrawlStop, DEFAULT_MAX_RESPONSE_BYTES, DEFAULT_USER_AGENT,
    SMALLEST_MAX_RESPONSE_BYTES, Seed, SessionCookie, SpiderEngine,
};
use archeion::readability::SiteRules;
use archeion::sitemap::{SitemapListing, read_sitemap};
use archeion::storage::{Archive, StorageError};
use serde::Serialize;

use super::session_cookie::{COOKIE_HEADER_VARIABLE, cookie_header_value};
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
    /// The identity this run announces to servers and matches `robots.txt` groups against,
    /// in place of the compiled default archeion otherwise sends.
    #[arg(long, value_name = "STRING", help = user_agent_help())]
    user_agent: Option<String>,
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
    /// File holding the `Cookie` header of an authenticated request, so that pages a
    /// subscription paid for are archived as the pages rather than as an appeal to subscribe.
    /// It must be readable by its owner alone, it is sent to the seed's own origin and to no
    /// other, and `ARCHEION_COOKIE_HEADER` is the alternative to it. There is deliberately no
    /// flag that takes the credential itself: an argument lands in shell history and in the
    /// process table.
    #[arg(long, value_name = "PATH")]
    cookie_file: Option<PathBuf>,
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

/// Read off the engine's own constant rather than off `defaults()`: `Seed::new` carries
/// `None` for this field, which means "the engine's compiled default" and prints as nothing
/// useful, while the string an operator actually gets when they omit the flag lives in
/// `archeion::crawl::DEFAULT_USER_AGENT`.
fn user_agent_help() -> String {
    format!(
        "The identity announced to servers and matched against robots rules [default: {DEFAULT_USER_AGENT}]"
    )
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
    // Read before anything else and refused rather than worked around: a run that silently went
    // out anonymous would archive a publication's paid half as teasers, which is the state this
    // flag exists to leave behind and looks like nothing in the report.
    let credential = cookie_header_value(
        args.cookie_file.as_deref(),
        std::env::var_os(COOKIE_HEADER_VARIABLE),
    )?;
    // Refused here for the same reason the credential above is: a byte no header can carry,
    // let through, is a value the engine's own client cannot build at all rather than one it
    // sends oddly, and the crate underneath assumes that construction never fails.
    if let Some(agent) = args.user_agent.as_deref() {
        usable_user_agent(agent)?;
    }
    let seed = seed_of(&args, credential);
    // Before the archive, because opening one creates it. A seed the engine will not dial is
    // a run that was never going to fetch anything, and creating a directory for it leaves
    // an empty archive on the path that was typed wrong, which is exactly the case the line
    // announcing a new archive exists to make visible.
    let engine = SpiderEngine::default();
    engine.check_seed(&seed)?;
    let (archive, created) = open_or_create(&args.archive)?;
    // Said before the first request rather than folded into the report at the end: appending
    // is correct, but it is easy to start this by accident, and the run has to say so while
    // there is still time to stop it rather than after the fact.
    if !created {
        warn_if_seed_already_captured(&archive, &args.seed_url);
    }
    // A rule file that cannot be used costs the extractions it would have improved and not
    // the capture, so it is a warning here rather than a reason to refuse the run: the
    // response is the part that cannot be fetched again, and a better reading of it can be
    // produced later from what is on disk.
    let (rules, unused_rules) = SiteRules::read(&archive.extraction_rules_path());
    warn(unused_rules.iter().map(ToString::to_string));

    // Stamped before the first phase rather than inside either one, because `--deadline` bounds
    // the run and the run is what starts here. A phase reading its own clock measures how long
    // that phase took, which is the same number only when there is one phase.
    let run_started = Instant::now();

    // A sitemap exists for a site whose pages do not link one another, so crawling such a site
    // anyway spends the run's budget on the listing, navigation and comment pages the sitemap
    // was reached for in order to skip, and files the pages both phases find twice. Asking for
    // a depth explicitly is asking for both, and then both run.
    let (mut run, mut failure) = if traverses_from_the_seed(&args) {
        match capture_seed(&engine, &archive, &seed, &rules) {
            Ok(run) => (run, None),
            // The report is the point of carrying the run inside the error: the archive holds
            // whatever was written before the disk refused, and a caller told only that a write
            // failed has to go looking for the rest.
            Err(CaptureError::Storage { source, run }) => (*run, Some(source)),
            Err(other) => return Err(other.into()),
        }
    } else {
        (CaptureRun::default(), None)
    };

    // A disk that already refused one write will refuse the next, so the sitemap phase is
    // skipped once the ordinary crawl already failed for that reason: there is nothing left
    // to spend more requests archiving into.
    // The `from_sitemap` half is the phase's own precondition, checked here as well so that a
    // run without one does not pay for the snapshot below: on a fifty thousand page crawl that
    // is a set of that many strings copied and dropped unread.
    let sitemap = if failure.is_none() && args.from_sitemap.is_some() {
        // Snapshotted before the phase starts, because that is what the phase has to answer
        // to: the pages the crawl already filed come out of the same ceiling, the addresses
        // it filed are the ones not worth buying twice, and the clock is the run's.
        let archived = run.archived_urls.clone();
        let so_far = RunSoFar {
            started: run_started,
            pages_written: run.captures_written,
            archived: &archived,
        };
        sitemap_phase(&args, &engine, &archive, &seed, &rules, &mut run, so_far)
    } else {
        SitemapOutcome::default()
    };
    failure = failure.or(sitemap.failure);

    let report = report_of(&args, &seed, &run, created, sitemap.report);
    let output = if json {
        format!("{}\n", serde_json::to_string(&report)?)
    } else {
        human_report(&report, run.stopped)
    };
    write_stdout(&output)?;
    warn(sitemap.warning.into_iter().chain(losses(&run)));

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

/// Why `--user-agent`'s value cannot be used: a byte no header can carry, refused before a
/// run starts rather than at the client that would otherwise be asked to build one from it.
#[derive(Debug, thiserror::Error)]
#[error("--user-agent holds a character that cannot be sent in a header")]
struct UnusableUserAgent;

/// Whether `value` could be sent as the value of some header, HTTP's own rule and not
/// ASCII's: a `User-Agent` naming an operator with an accent in it is ordinary and stays
/// accepted, since only a control byte, `\r` and `\n` chief among them, is what no header
/// can hold. Letting one through would build a second header nobody wrote the moment this
/// reaches the client, which for `--cookie-file` is refused the same way in
/// `session_cookie.rs`; this mirrors that rule rather than the narrower, ASCII-only one the
/// cookie path also applies, since a cookie's own grammar is stricter than a header's.
fn usable_user_agent(value: &str) -> Result<(), UnusableUserAgent> {
    let carries_only_what_a_header_can_hold = value
        .bytes()
        .all(|byte| byte == b'\t' || (byte >= 0x20 && byte != 0x7f));
    if carries_only_what_a_header_can_hold {
        Ok(())
    } else {
        Err(UnusableUserAgent)
    }
}

/// The seed this run crawls with. The credential arrives separately because reading it can fail
/// and because it is bound here rather than where it was read: the origin it may be sent to is
/// the seed's own, which is what keeps this flag ignorant of any particular publisher.
fn seed_of(args: &CaptureArgs, credential: Option<String>) -> Seed {
    let mut seed = Seed::new(args.seed_url.clone());
    seed.max_pages = args.max_pages;
    seed.max_depth = args.max_depth.unwrap_or_else(|| defaults().max_depth);
    seed.concurrency = args.concurrency;
    seed.delay = args.delay.0;
    seed.deadline = args.deadline.0;
    seed.request_timeout = args.request_timeout.0;
    seed.max_retries = args.max_retries;
    seed.user_agent = args.user_agent.clone();
    seed.allow_private_addresses = args.allow_private_addresses;
    seed.session_cookie = credential.map(|value| SessionCookie::bound_to(&args.seed_url, value));
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

/// Says, before anything is fetched, that this seed has been captured into this archive
/// before. It has happened twice by accident: a run started, stopped and started again, with
/// nothing about the invocation saying that a second run and a first look the same.
///
/// A seed that fails to canonicalize is left to whatever refuses it once the crawl actually
/// starts; this line is a courtesy about a run that would otherwise say nothing; it is not
/// where a malformed seed is reported.
fn warn_if_seed_already_captured(archive: &Archive, seed_url: &str) {
    if let Ok(canonical) = CanonicalUrl::parse(seed_url)
        && archive.has_captures(&canonical)
    {
        warn(std::iter::once(format!(
            "{seed_url} already has captures in this archive; this run appends to them \
             rather than replacing them"
        )));
    }
}

/// What one URL cost the run and why. Four of the five shortfalls a run reports have this
/// shape already, so the machine readable answer keeps it rather than inventing one per kind.
#[derive(Debug, Serialize)]
struct Loss {
    url: String,
    reason: String,
}

/// Whether links are followed out of the seed at all.
///
/// A sitemap exists for a site whose pages do not link one another, so a run told to read one
/// and given no depth of its own is the sitemap rather than a crawl with a sitemap after it:
/// crawling such a site anyway spends the budget on the listing, navigation and comment pages
/// the sitemap was reached for in order to skip, and files whatever both phases find twice.
/// Naming a depth explicitly is asking for both, and then both run.
fn traverses_from_the_seed(args: &CaptureArgs) -> bool {
    args.from_sitemap.is_none() || args.max_depth.is_some()
}

/// Reads the sitemap this run was asked for and archives what it lists.
///
/// A sitemap that cannot be found or parsed is reported as a warning rather than treated as a
/// reason to end the run. Where the ordinary crawl also ran, its captures are already written
/// and turning those into a failure over a page that happens not to be one would throw away a
/// working archive. Where it did not, the seed is archived and the warning says exactly what
/// was not read, which leaves the operator a run to repeat rather than a silent half job. A
/// write failure inside this phase is the one thing that still matters to the caller, and it
/// is folded into the same run rather than reported on its own.
///
/// The captures land in `run`, which is the run both phases share. Everything else the phase
/// produced comes back in `SitemapOutcome`, the write failure included: a phase that took the
/// failure as an out parameter needed an eighth argument to also be told when the run began.
fn sitemap_phase(
    args: &CaptureArgs,
    engine: &dyn CrawlEngine,
    archive: &Archive,
    seed: &Seed,
    rules: &SiteRules,
    run: &mut CaptureRun,
    so_far: RunSoFar<'_>,
) -> SitemapOutcome {
    let Some(requested) = &args.from_sitemap else {
        return SitemapOutcome::default();
    };
    // A run that has already spent its budget has nothing to learn from a listing, and reading
    // one is a request the host answers for nothing. Reachable only since the two phases share
    // the budget: a phase that started its own count always had pages to spend.
    if so_far.nothing_left_to_spend(seed) {
        return SitemapOutcome::default();
    }
    let (listing, warning) = match read_sitemap(engine, seed, requested.as_deref()) {
        Ok(listing) => (Some(listing), None),
        Err(error) => (None, Some(error.to_string())),
    };
    // The seed leads the list when nothing crawled from it, because it is still the address
    // somebody typed and a run that archived everything except the page it was pointed at
    // would be answering a question nobody asked.
    let mut urls: Vec<String> = Vec::new();
    if !traverses_from_the_seed(args) {
        urls.push(seed.url.clone());
    }
    if let Some(listing) = &listing {
        urls.extend(listing.urls.iter().cloned());
    }
    if urls.is_empty() {
        return SitemapOutcome {
            warning,
            ..SitemapOutcome::default()
        };
    }
    // Given explicitly, the same depth that already bounds the ordinary crawl now also
    // bounds how far a sitemap URL is traversed from; left alone, a sitemap URL is fetched
    // on its own, since a depth bound has no meaning for a page nobody linked to.
    let follow_links = args.max_depth.is_some();
    let report = listing.as_ref().map(SitemapReport::from);
    match capture_sitemap(engine, archive, seed, rules, &urls, follow_links, so_far) {
        Ok(sitemap_run) => {
            run.merge(sitemap_run);
            SitemapOutcome {
                report,
                warning,
                failure: None,
            }
        }
        Err(CaptureError::Storage { source, run: extra }) => {
            run.merge(*extra);
            SitemapOutcome {
                report,
                warning,
                failure: Some(source),
            }
        }
        Err(CaptureError::Crawl(error)) => SitemapOutcome {
            report,
            warning: Some(format!("the sitemap's URLs could not be crawled: {error}")),
            failure: None,
        },
    }
}

/// What the sitemap phase leaves behind once its captures are folded into the run.
#[derive(Debug, Default)]
struct SitemapOutcome {
    report: Option<SitemapReport>,
    /// Something the operator should read but that does not end the run: a sitemap that could
    /// not be found, parsed, or crawled.
    warning: Option<String>,
    /// A refused write, which is the one thing here the caller acts on rather than prints.
    failure: Option<StorageError>,
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

/// What a run's subscription reached, present only when the run was given one.
///
/// A credential can apply to nothing and cost nothing but the archive: a seed spelled with a
/// trailing dot, an `http` seed whose host redirects to `https`, a credential bound to an origin
/// the run never asks for. Every one of those archives a publication's paid half as teasers and
/// exits zero. Naming the origin and the count is what makes the difference readable: an origin
/// that is not the one the operator meant, or a count of zero against a run of hundreds, says the
/// session did nothing.
#[derive(Debug, Serialize)]
struct SessionReport {
    origin: String,
    captures_used: usize,
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
    /// How many of the captures above landed on an item this archive already held one for.
    /// `None` when this run created the archive, since nothing it just brought into
    /// existence can have held anything before it: a run appending to nothing is not a case
    /// this row exists to say anything about, unlike `responses_refused`, which is worth
    /// reading even at zero.
    items_appended: Option<usize>,
    /// Keyed by status, declared here in the shape this command promises rather than
    /// serialized off the library's report, which is this file's convention for every field.
    responses_refused: BTreeMap<String, usize>,
    articles_extracted: usize,
    extractions_refused: usize,
    assets_stored: usize,
    assets_missed: usize,
    asset_fetches: usize,
    pages_dropped: usize,
    links_never_followed: Vec<String>,
    stopped: &'static str,
    session: Option<SessionReport>,
    sitemap: Option<SitemapReport>,
    failed_fetches: Vec<Loss>,
    unaddressable_pages: Vec<Loss>,
    pages_inside_a_network: Vec<String>,
    unreadable_pages: Vec<Loss>,
    unreadable_articles: Vec<Loss>,
}

fn report_of(
    args: &CaptureArgs,
    seed: &Seed,
    run: &CaptureRun,
    created: bool,
    sitemap: Option<SitemapReport>,
) -> CaptureReport {
    CaptureReport {
        seed_url: args.seed_url.clone(),
        archive: args.archive.display().to_string(),
        archive_created: created,
        captures_written: run.captures_written,
        items_appended: (!created).then_some(run.items_appended),
        responses_refused: run
            .responses_refused
            .iter()
            .map(|(status, count)| (status.to_string(), *count))
            .collect(),
        articles_extracted: run.articles_extracted,
        extractions_refused: run.extractions_refused,
        assets_stored: run.assets_stored,
        assets_missed: run.assets_missed,
        asset_fetches: run.asset_fetches,
        pages_dropped: run.pages_dropped,
        links_never_followed: run.links_never_followed.clone(),
        stopped: stop_name(run.stopped),
        session: seed.session_cookie.as_ref().map(|cookie| SessionReport {
            origin: cookie.origin(),
            captures_used: run.captures_with_a_session,
        }),
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

/// What the host answered with an error, said in one line.
///
/// It is a row that prints even when it is empty, like the losses beside it, because a row
/// that only appears on a bad run is a row nobody has learned to look for. A run refused by a
/// host used to say nothing at all, and the only symptom was a count of articles that read as
/// a defect somewhere else.
fn refused_sentence(refused: &BTreeMap<String, usize>) -> String {
    if refused.is_empty() {
        return "none".to_owned();
    }
    refused
        .iter()
        .map(|(status, count)| format!("{count} answered {status}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn stop_name(stopped: CrawlStop) -> &'static str {
    match stopped {
        CrawlStop::Exhausted => "exhausted",
        CrawlStop::DeadlineReached => "deadline-reached",
        CrawlStop::PageCeilingReached => "page-ceiling-reached",
        CrawlStop::CallerStopped => "stopped-by-the-archive",
    }
}

/// The same answers a person reads. It is a second exhaustive match rather than a lookup on
/// the name above, so a stop that gets added is a stop this file fails to compile without
/// rather than one that quietly reads as an ordinary end.
fn stop_sentence(stopped: CrawlStop) -> &'static str {
    match stopped {
        CrawlStop::Exhausted => "nothing was left to fetch",
        CrawlStop::DeadlineReached => "the seed's deadline ran out",
        CrawlStop::PageCeilingReached => "the run had archived every page it was allowed",
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
        // First, because it qualifies the line above it rather than the ones below: those
        // captures are part of the count just printed, and every other row here is about
        // what a run made of a page it did have.
        ("host refused", refused_sentence(&report.responses_refused)),
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
    // Only for a run that carried one, since a row about a credential nobody supplied is a row
    // every ordinary run has to read past. What it says when it is there is both halves of the
    // question: where the credential was allowed to go, and how many captures it reached. A
    // count of zero is the whole point of the row, and it is the shape a mistyped seed takes.
    if let Some(session) = &report.session {
        writeln!(
            output,
            "  {:<14}{}, {} capture(s)",
            "session", session.origin, session.captures_used
        )
        .expect("writing to a string cannot fail");
    }
    if let Some(sitemap) = &report.sitemap {
        writeln!(
            output,
            "  {:<14}{} taken, {} refused, {} listed",
            "sitemap", sitemap.urls_taken, sitemap.urls_refused, sitemap.urls_listed
        )
        .expect("writing to a string cannot fail");
    }
    // Present whenever the run did not create the archive, at zero included: a zero here says
    // this run touched nothing it had already captured, which is only worth knowing about an
    // archive old enough to have captured something. Absent for a run that just created the
    // archive, since nothing that new could have held anything for this run to append to.
    if let Some(items_appended) = report.items_appended {
        writeln!(
            output,
            "  {:<14}{items_appended} item(s) gained a further capture",
            "appended"
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
        let seed = seed_of(&parse(&[]), None);
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
        let seed = seed_of(
            &parse(&[
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
            ]),
            None,
        );

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
        assert_eq!(
            seed_of(&parse(&["--delay", "0s"]), None).delay,
            Duration::ZERO
        );
    }

    /// A run that asked for no deadline is making a decision, not asking for a budget of
    /// nothing, so the guard above has to let it through.
    #[test]
    fn a_run_with_no_deadline_is_not_mistaken_for_one_with_an_empty_budget() {
        assert_eq!(
            seed_of(&parse(&["--deadline", "none"]), None).deadline,
            None
        );
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

    /// The credential is a path on this command line and never the value, and a run given none
    /// carries none. What the flag names is checked here; where that credential may be sent is
    /// the seed's own rule, and the library tests it.
    #[test]
    fn the_cookie_file_is_a_path_and_the_credential_reaches_the_seed() {
        assert_eq!(parse(&[]).cookie_file, None);
        assert!(seed_of(&parse(&[]), None).session_cookie.is_none());
        assert_eq!(
            parse(&["--cookie-file", "/home/reader/.config/archeion/a.cookie"]).cookie_file,
            Some(PathBuf::from("/home/reader/.config/archeion/a.cookie"))
        );
        assert!(
            seed_of(&parse(&[]), Some("substack.sid=secret".to_owned()))
                .session_cookie
                .is_some()
        );
    }

    /// A credential bound to an origin the run never asks for archives teasers and exits zero, so
    /// the report is the only thing that can say the session did nothing. It names the origin as
    /// well as the count, because a count of zero on the origin the operator meant and a count of
    /// zero on a seed they mistyped are different problems.
    #[test]
    fn a_run_carrying_a_session_reports_the_origin_it_was_bound_to_and_what_it_reached() {
        let args = parse(&[]);
        let seed = seed_of(&args, Some("substack.sid=secret".to_owned()));
        let run = CaptureRun {
            captures_written: 3,
            captures_with_a_session: 2,
            ..CaptureRun::default()
        };

        let report = report_of(&args, &seed, &run, false, None);
        let printed = human_report(&report, run.stopped);

        assert!(
            printed.contains("session       https://example.com, 2 capture(s)"),
            "the run reported: {printed}"
        );
        assert!(!printed.contains("secret"), "the credential was printed");
    }

    /// The ordinary run has no such row at all, because a line about a credential nobody supplied
    /// is one every run would have to read past.
    #[test]
    fn a_run_carrying_no_session_reports_no_such_row() {
        let args = parse(&[]);
        let report = report_of(
            &args,
            &seed_of(&args, None),
            &CaptureRun::default(),
            false,
            None,
        );

        assert!(report.session.is_none());
        assert!(!human_report(&report, CrawlStop::Exhausted).contains("session"));
    }

    /// The row's own boundary: present and zero for an archive this run did not create, gone
    /// entirely for one it did, whatever `items_appended` happens to hold. A run that just
    /// created its archive cannot have appended to anything in it, so the field carries no
    /// number worth printing rather than a zero indistinguishable from the other case.
    #[test]
    fn the_appended_row_is_a_number_into_an_old_archive_and_absent_from_a_new_one() {
        let args = parse(&[]);
        let seed = seed_of(&args, None);

        let untouched = report_of(
            &args,
            &seed,
            &CaptureRun {
                items_appended: 0,
                ..CaptureRun::default()
            },
            false,
            None,
        );
        assert_eq!(untouched.items_appended, Some(0));
        assert!(
            human_report(&untouched, CrawlStop::Exhausted).contains("appended      0 item(s)"),
            "{}",
            human_report(&untouched, CrawlStop::Exhausted)
        );

        let appended = report_of(
            &args,
            &seed,
            &CaptureRun {
                items_appended: 3,
                ..CaptureRun::default()
            },
            false,
            None,
        );
        assert_eq!(appended.items_appended, Some(3));
        assert!(
            human_report(&appended, CrawlStop::Exhausted).contains("appended      3 item(s)"),
            "{}",
            human_report(&appended, CrawlStop::Exhausted)
        );

        let fresh_archive = report_of(
            &args,
            &seed,
            &CaptureRun {
                items_appended: 3,
                ..CaptureRun::default()
            },
            true,
            None,
        );
        assert_eq!(fresh_archive.items_appended, None);
        assert!(!human_report(&fresh_archive, CrawlStop::Exhausted).contains("appended"));
    }

    /// The stop is the row an operator reads to decide which flag to change, so the page
    /// ceiling and the deadline have to read as the two different answers they are. The name
    /// in the JSON report is asserted beside the sentence because a pipeline reads that one
    /// and would otherwise keep filing ceiling-bounded runs under a deadline.
    #[test]
    fn a_run_stopped_by_the_page_ceiling_reports_the_ceiling_and_not_the_deadline() {
        let args = parse(&["--max-pages", "2"]);
        let seed = seed_of(&args, None);
        let run = CaptureRun {
            captures_written: 2,
            stopped: CrawlStop::PageCeilingReached,
            ..CaptureRun::default()
        };

        let report = report_of(&args, &seed, &run, false, None);
        let printed = human_report(&report, run.stopped);

        assert_eq!(report.stopped, "page-ceiling-reached");
        assert!(
            printed.contains("stopped       the run had archived every page it was allowed"),
            "the run reported: {printed}"
        );
        assert!(
            !printed.contains("deadline"),
            "a page ceiling is not a deadline: {printed}"
        );
    }

    /// A depth nobody typed has to still be the library's own default, since that is the
    /// number every other test here assumes a bare seed crawls with. Whether it was typed at
    /// all is what `--from-sitemap` needs to tell apart from a depth of two nobody asked for.
    #[test]
    fn a_max_depth_nobody_typed_is_the_library_s_own_default() {
        assert_eq!(parse(&[]).max_depth, None);
        assert_eq!(
            seed_of(&parse(&[]), None).max_depth,
            Seed::new("https://example.com/").max_depth
        );
        assert_eq!(parse(&["--max-depth", "3"]).max_depth, Some(3));
    }
}
