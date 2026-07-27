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
use archeion::readability::{self, Article};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Expectation {
    /// Why this case is in the corpus. Read by a person, not by the test.
    #[allow(dead_code)]
    why: String,
    is_article: bool,
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

#[derive(Debug, Deserialize)]
struct WordCount {
    min: usize,
    max: usize,
}

#[test]
fn every_page_in_the_corpus_extracts_within_its_declared_bounds() {
    let cases = corpus();
    assert!(!cases.is_empty(), "the corpus directory has no pages in it");

    for page in cases {
        let name = page
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("a fixture has a readable name")
            .to_owned();
        let expected = expectation_for(&page, &name);
        let extracted = extract(&page, &name, expected.title.as_deref());

        match (expected.is_article, extracted) {
            (false, Some(article)) => panic!(
                "{name} is not an article, but one was extracted:\n{}",
                article.markdown
            ),
            (false, None) => {}
            (true, None) => panic!("{name} is an article, and nothing was extracted"),
            (true, Some(article)) => check(&name, &expected, &article),
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

fn extract(page: &Path, name: &str, title: Option<&str>) -> Option<Article> {
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

fn expectation_for(page: &Path, name: &str) -> Expectation {
    let path = page.with_file_name(format!("{name}.expected.json"));
    let declared = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{name} has no expectation file at {path:?}: {error}"));
    serde_json::from_str(&declared)
        .unwrap_or_else(|error| panic!("{name} has an unreadable expectation file: {error}"))
}

fn corpus() -> Vec<PathBuf> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/readability");
    let mut pages: Vec<PathBuf> = fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("reading {directory:?}: {error}"))
        .map(|entry| entry.expect("a readable directory entry").path())
        .filter(|path| path.extension().is_some_and(|kind| kind == "html"))
        .collect();
    // Sorted so a failure names the same case on every machine.
    pages.sort();
    pages
}
