//! The readability corpus: every page in `tests/fixtures/readability`, against the bounds
//! declared beside it.
//!
//! Extraction quality varies by site and cannot be asserted exactly, so nothing here compares
//! output to a stored expectation. Pinning today's Markdown would freeze current behavior as
//! correctness and break on every improvement. What is asserted is what has to be true of any
//! acceptable extraction: prose that survives, furniture that does not, the heading hierarchy,
//! and a range for the word count.
//!
//! `tests/fixtures/readability/README.md` has the file format and where a case comes from.

use std::fs;
use std::path::{Path, PathBuf};

use archeion::metadata::PageSource;
use archeion::readability::{self, Article, Extraction, SiteRules};
use serde::Deserialize;

/// Unknown fields are refused rather than ignored. A mistyped assertion that parses is worse
/// than one that fails: `must_not_contains` with an `s` reads as coverage, asserts nothing,
/// and leaves its fixture testing only that something came out.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expectation {
    /// Why this case is in the corpus. Read by a person, not by the test.
    #[allow(dead_code)]
    why: String,
    outcome: Outcome,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    must_contain: Vec<String>,
    #[serde(default)]
    must_not_contain: Vec<String>,
    #[serde(default)]
    heading_levels: Option<Vec<usize>>,
    #[serde(default)]
    word_count: Option<WordCount>,
    /// What this page's host has been told, in the shape a rule file declares it. A case that
    /// carries one is asserting two things at once: that the extraction with the rule is the one
    /// declared above, and that without it the extraction is not, which
    /// `no_rule_in_the_corpus_is_decorative` checks separately.
    #[serde(default)]
    rules: Option<serde_json::Value>,
}

/// What the extractor has to make of a page. The two ways of producing no article are
/// separate cases here because they leave different things behind: a page passed over in
/// silence leaves nothing, and a page refused leaves a record beside its capture for a later
/// review to answer. A fixture that only declared "not an article" would keep passing if a
/// refusal quietly turned into silence, and the review queue would empty without anyone
/// noticing.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Article,
    Refused,
    Nothing,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WordCount {
    min: usize,
    max: usize,
}

/// A case that produces no article checks none of the fields that describe one, so declaring
/// them there is writing an assertion that will never run. It reads as coverage in a file
/// somebody will trust, which is the same reason unknown fields are refused above rather than
/// ignored. `title` is exempt: it is handed to the extractor rather than asserted, and a page
/// is refused or passed over with the title its metadata resolved like any other.
#[test]
fn no_expectation_declares_an_assertion_its_outcome_will_never_check() {
    let unchecked: Vec<String> = corpus()
        .into_iter()
        .filter_map(|page| {
            let name = file_stem(&page);
            let expected = expectation_for(&page, &name);
            if expected.outcome == Outcome::Article {
                return None;
            }
            let declared = [
                (!expected.must_contain.is_empty(), "must_contain"),
                (!expected.must_not_contain.is_empty(), "must_not_contain"),
                (expected.heading_levels.is_some(), "heading_levels"),
                (expected.word_count.is_some(), "word_count"),
            ];
            let names: Vec<&str> = declared
                .into_iter()
                .filter_map(|(present, name)| present.then_some(name))
                .collect();
            (!names.is_empty()).then(|| format!("{name}: {}", names.join(", ")))
        })
        .collect();

    assert!(
        unchecked.is_empty(),
        "expectations declaring assertions nothing will check: {unchecked:?}"
    );
}

/// Every expectation file has a page. The corpus is keyed on the markup, so an expectation
/// whose page was renamed or deleted would otherwise sit there asserting nothing, and the
/// suite would keep passing with one case fewer than the directory appears to hold.
#[test]
fn no_expectation_in_the_corpus_has_lost_its_page() {
    let orphans: Vec<String> = fs::read_dir(corpus_dir())
        .expect("the corpus directory is readable")
        .map(|entry| entry.expect("a readable directory entry").path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".expected.json"))
        })
        .filter(|path| {
            let stem = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            let page = stem.trim_end_matches(".expected.json");
            !path.with_file_name(format!("{page}.html")).is_file()
        })
        .map(|path| path.display().to_string())
        .collect();

    assert!(orphans.is_empty(), "expectations with no page: {orphans:?}");
}

#[test]
fn every_page_in_the_corpus_extracts_within_its_declared_bounds() {
    let cases = corpus();
    assert!(!cases.is_empty(), "the corpus directory has no pages in it");

    for page in cases {
        let name = file_stem(&page);
        let expected = expectation_for(&page, &name);
        let extracted = extract(&page, &name, &expected, Rule::Applied);

        match (&expected.outcome, extracted) {
            (Outcome::Article, Extraction::Article(article)) => {
                let broken = violations(&expected, &article);
                assert!(
                    broken.is_empty(),
                    "{name}:\n{}\n\n{}",
                    broken.join("\n"),
                    article.markdown
                );
            }
            (Outcome::Refused, Extraction::Refused(_)) => {}
            (Outcome::Nothing, Extraction::NotArticle(_)) => {}
            (expected, Extraction::Article(article)) => panic!(
                "{name} is declared {expected:?}, and an article was extracted:\n{}",
                article.markdown
            ),
            (expected, other) => {
                panic!("{name} is declared {expected:?}, and extraction answered {other:?}")
            }
        }
    }
}

/// A rule is an escape hatch for what the heuristic cannot do, so a case carrying one has to
/// show the heuristic failing without it. Otherwise a rule that stopped being needed, because the
/// scorer improved or because it never was, would sit in the corpus reading as evidence that a
/// site needs telling while proving nothing at all.
///
/// What counts as failing is the case's own expectation: the same bounds, read against the
/// extraction the heuristic reaches alone, have to be broken somewhere.
#[test]
fn no_rule_in_the_corpus_is_decorative() {
    let decorative: Vec<String> = corpus()
        .into_iter()
        .filter_map(|page| {
            let name = file_stem(&page);
            let expected = expectation_for(&page, &name);
            expected.rules.as_ref()?;
            let alone = extract(&page, &name, &expected, Rule::Withheld);

            let held = match (&expected.outcome, &alone) {
                (Outcome::Article, Extraction::Article(article)) => {
                    violations(&expected, article).is_empty()
                }
                (Outcome::Refused, Extraction::Refused(_)) => true,
                (Outcome::Nothing, Extraction::NotArticle(_)) => true,
                _ => false,
            };
            held.then(|| format!("{name} declares a rule and extracts the same without it"))
        })
        .collect();

    assert!(decorative.is_empty(), "{decorative:?}");
}

/// Every bound the article breaks, rather than the first one.
///
/// It answers a list instead of asserting, because the same comparison is read two ways: an
/// empty list is what the case declares, and a non-empty one is what proves a rule was needed.
fn violations(expected: &Expectation, article: &Article) -> Vec<String> {
    let markdown = &article.markdown;
    let mut broken = Vec::new();
    for prose in &expected.must_contain {
        if !markdown.contains(prose.as_str()) {
            broken.push(format!("the article lost prose it had to keep: {prose:?}"));
        }
    }
    for furniture in &expected.must_not_contain {
        if markdown.contains(furniture.as_str()) {
            broken.push(format!(
                "furniture survived into the article: {furniture:?}"
            ));
        }
    }
    if let Some(levels) = &expected.heading_levels {
        let found = heading_levels(markdown);
        if &found != levels {
            broken.push(format!(
                "the heading hierarchy is {found:?} and not {levels:?}"
            ));
        }
    }
    if let Some(range) = &expected.word_count {
        let counted = article.record.word_count;
        if !(range.min..=range.max).contains(&counted) {
            broken.push(format!(
                "{counted} words, outside the declared {}..={}",
                range.min, range.max
            ));
        }
    }
    broken
}

/// The `#` prefixes in document order. Markdown puts them at the start of a line, and a `#`
/// inside a fenced code block is not one, which is why the fences are tracked.
fn heading_levels(markdown: &str) -> Vec<usize> {
    let mut levels = Vec::new();
    let mut fenced = false;
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let hashes = line.chars().take_while(|c| *c == '#').count();
        if hashes > 0 && line.chars().nth(hashes) == Some(' ') {
            levels.push(hashes);
        }
    }
    levels
}

/// Whether the case's host is told what its expectation declares, or nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    Applied,
    Withheld,
}

fn extract(page: &Path, name: &str, expected: &Expectation, rule: Rule) -> Extraction {
    let body = fs::read(page).unwrap_or_else(|error| panic!("reading {name}: {error}"));
    let rules = match (rule, &expected.rules) {
        (Rule::Applied, Some(declared)) => rules_for(name, declared),
        _ => SiteRules::default(),
    };
    readability::extract(
        PageSource {
            body: &body,
            content_type: Some("text/html; charset=utf-8"),
            // Every fixture is filed under its own name, so a rule keys on the host of the page
            // it was declared beside without the corpus having to be rearranged.
            final_url: &format!("https://{name}.example.com/page"),
        },
        expected.title.as_deref(),
        &rules,
    )
    .unwrap_or_else(|error| panic!("{name} was refused: {error}"))
}

/// The declared rule, put through the same reader an operator's file goes through. Building the
/// rule directly would let a case declare something no rule file could express.
fn rules_for(name: &str, declared: &serde_json::Value) -> SiteRules {
    let file = serde_json::json!({ "hosts": { format!("{name}.example.com"): declared } });
    let (rules, unused) = SiteRules::parse(&file.to_string(), name);
    assert!(
        unused.is_empty(),
        "{name} declares a rule nothing can read: {unused:?}"
    );
    rules
}

fn file_stem(page: &Path) -> String {
    page.file_stem()
        .and_then(|stem| stem.to_str())
        .expect("a fixture has a readable name")
        .to_owned()
}

fn expectation_for(page: &Path, name: &str) -> Expectation {
    let path = page.with_file_name(format!("{name}.expected.json"));
    let declared = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{name} has no expectation file at {path:?}: {error}"));
    serde_json::from_str(&declared)
        .unwrap_or_else(|error| panic!("{name} has an unreadable expectation file: {error}"))
}

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/readability")
}

fn corpus() -> Vec<PathBuf> {
    let directory = corpus_dir();
    let mut pages: Vec<PathBuf> = fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("reading {directory:?}: {error}"))
        .map(|entry| entry.expect("a readable directory entry").path())
        .filter(|path| path.extension().is_some_and(|kind| kind == "html"))
        .collect();
    // Sorted so a failure names the same case on every machine.
    pages.sort();
    pages
}
