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
use archeion::readability::{self, Article, Extraction};
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
        let extracted = extract(&page, &name, expected.title.as_deref());

        match (&expected.outcome, extracted) {
            (Outcome::Article, Extraction::Article(article)) => check(&name, &expected, &article),
            (Outcome::Refused, Extraction::Refused(_)) => {}
            (Outcome::Nothing, Extraction::Nothing) => {}
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

fn check(name: &str, expected: &Expectation, article: &Article) {
    let markdown = &article.markdown;
    for prose in &expected.must_contain {
        assert!(
            markdown.contains(prose.as_str()),
            "{name}: the article lost prose it had to keep: {prose:?}\n\n{markdown}"
        );
    }
    for furniture in &expected.must_not_contain {
        assert!(
            !markdown.contains(furniture.as_str()),
            "{name}: furniture survived into the article: {furniture:?}\n\n{markdown}"
        );
    }
    if let Some(levels) = &expected.heading_levels {
        assert_eq!(
            &heading_levels(markdown),
            levels,
            "{name}: the heading hierarchy is not the one declared\n\n{markdown}"
        );
    }
    if let Some(range) = &expected.word_count {
        let counted = article.record.word_count;
        assert!(
            (range.min..=range.max).contains(&counted),
            "{name}: {counted} words, outside the declared {}..={}\n\n{markdown}",
            range.min,
            range.max
        );
    }
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

fn extract(page: &Path, name: &str, title: Option<&str>) -> Extraction {
    let body = fs::read(page).unwrap_or_else(|error| panic!("reading {name}: {error}"));
    readability::extract(
        PageSource {
            body: &body,
            content_type: Some("text/html; charset=utf-8"),
            // Every fixture is filed under its own name, so a rule that ever keys on the host
            // has somewhere to key from without the corpus having to be rearranged.
            final_url: &format!("https://{name}.example.com/page"),
        },
        title,
    )
    .unwrap_or_else(|error| panic!("{name} was refused: {error}"))
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
