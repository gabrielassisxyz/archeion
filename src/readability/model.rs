//! What an extraction produced: the Markdown document and the record beside it.

use serde::{Deserialize, Serialize};

/// The extractor that produced a record. Bumped when the meaning of a field or a rule that
/// fills one changes, not when a field is added, on the same terms as the metadata record.
///
/// 2 is the sliver rule: a page can now produce prose and still be refused as an article, so
/// the absence of a record beside a capture stopped meaning what it meant at 1.
pub const EXTRACTOR_VERSION: u32 = 2;

/// What decided where the article was in the page.
///
/// A generic scorer reads markup that follows convention and fails on sites with a layout of
/// their own, so a per-host override layer is coming. Recording which of the two produced an
/// extraction is what lets a reader tell a page the heuristic happened to get right from one
/// that needed to be told.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionRules {
    /// The scoring algorithm, with nothing said about this host.
    Heuristic,
}

/// A ceiling the article reached, recorded rather than silently applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArticleBound {
    /// The Markdown was cut. What is stored is a prefix of the article, not the article.
    Markdown,
}

/// What is known about an extracted article, without the prose itself.
///
/// The prose is a separate file because it is the artifact a person reads and a reader
/// renders, and burying it in a JSON string would escape every newline and quote in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArticleRecord {
    pub extractor_version: u32,
    pub rules: ExtractionRules,
    /// Split on whitespace, so it is a rough figure for sorting and filtering rather than a
    /// measurement. It is wrong for languages that do not put spaces between words.
    pub word_count: usize,
    /// The prose of the whole page the article was taken out of, counted the same rough way.
    ///
    /// It is here so that what the sliver rule measures is recorded for the pages it kept and
    /// not only for the ones it refused, on the same terms as `cost` below. A file per refusal
    /// says the rule is firing; only the ratios that real articles reach can say whether the
    /// rule is about to start firing on them.
    pub page_word_count: usize,
    pub excerpt: Option<String>,
    /// The attribution the page's own markup carried, which is deliberately not the resolved
    /// author in the metadata record. The two disagree often, and collapsing them would hide
    /// which one to look at when an attribution comes out wrong.
    pub byline: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub truncated: Vec<ArticleBound>,
    /// What this page cost to admit, against the ceilings that admitted it.
    pub cost: AdmissionCost,
}

/// What one page measured against the ceilings it had to pass.
///
/// It is recorded for every article and not only for the pages that were refused, because the
/// two questions are different. A count of refusals says whether a ceiling is firing. Only the
/// values that real articles actually reach can say whether a lower ceiling would start
/// refusing them, and lowering the ceilings is the plan: they are set where a hostile page is
/// certainly refused, not where a real page is certainly kept, and the distance between those
/// two is what this measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionCost {
    /// The decoded document, which is what the byte ceiling is applied to. It is not the
    /// stored body's length: a page in a legacy encoding decodes to a different size.
    pub document_bytes: usize,
    /// The most elements open at one time, counted on the markup before any tree was built.
    /// For well-formed markup this is its nesting depth; above that it is unclosed tags.
    pub peak_open_elements: usize,
}

/// One page's prose, and what is known about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Article {
    /// A standalone Markdown document: the title as an `#` heading, then the prose.
    pub markdown: String,
    pub record: ArticleRecord,
}

/// What extraction made of one capture.
///
/// The two ways of not producing an article are separate answers because only one of them has
/// anything to say. A page that is not markup, or whose markup holds nothing the scorer would
/// call prose, is most of the web and is passed over in silence. A page that did produce prose
/// and was then refused is a disagreement between the algorithm and the rule below it, and
/// that disagreement is the only material either number can ever be corrected against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Extraction {
    Article(Article),
    Refused(RefusedExtraction),
    Nothing,
}

/// Prose that came out of a page and was not stored as an article.
///
/// This is not the sibling of `UnreadableArticle`, which is a page refused for what reading it
/// would cost and belongs to the run rather than to the archive. This one is an ordinary
/// reading of an ordinary page, kept because the rule that refused it is holding two numbers
/// that were chosen against a handful of sites and have to answer for themselves later.
///
/// The prose itself is deliberately not in here. It is derivable from the stored response at
/// any time, and writing it out is the claim this refusal exists to avoid making.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusedExtraction {
    pub extractor_version: u32,
    pub rules: ExtractionRules,
    /// The two measurements the rule compared, and nothing named as the reason beside them.
    /// One rule refuses here, its inputs are these, and a reader can see the comparison it
    /// made. A second rule is what makes naming them worth a field, and it will arrive with
    /// its own version bump.
    pub word_count: usize,
    pub page_word_count: usize,
    /// What the prose said, so that reviewing the decision does not require re-deriving it.
    pub excerpt: Option<String>,
}
