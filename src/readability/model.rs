//! What an extraction produced: the Markdown document and the record beside it.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The extractor that produced a record. Bumped when the meaning of a field or a rule that
/// fills one changes, not when a field is added, on the same terms as the metadata record.
///
/// 2 is the sliver rule: a page can now produce prose and still be refused as an article, so
/// the absence of a record beside a capture stopped meaning what it meant at 1.
///
/// The served document did not bump it. No record that exists changed its meaning: `heuristic`
/// and `site:<host>` say exactly what they said, and `served` is a value only records written
/// after it can carry. The absence it did change, a Markdown capture with no article beside it,
/// is answered where absences are answered, by what a repass counts as a media type worth
/// re-reading, and answering it there re-reads the handful of captures it applies to instead of
/// rewriting every article record in the archive to carry a larger number.
pub const EXTRACTOR_VERSION: u32 = 2;

/// How the prose in a record was obtained.
///
/// A generic scorer reads markup that follows convention and fails on sites with a layout of
/// their own, so a host can be told directly where its prose lives. Recording which of the two
/// produced an extraction is what lets a reader tell a page the heuristic happened to get right
/// from one that needed to be told, and it is what keeps a share measured under a rule out of
/// the distribution the heuristic's own numbers are calibrated against.
///
/// The third answer is not a third way of finding the article. It says no extraction happened
/// at all, which a reader comparing two articles has to be able to tell: one of them was
/// reconstructed and the other was published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractionRules {
    /// The scoring algorithm, with nothing said about this host.
    Heuristic,
    /// The scoring algorithm, over the document a host's rule left behind. The host is the one
    /// the rule is filed under, which is the canonical spelling and not the page's own.
    Site(String),
    /// Nothing scored anything: the response was already the prose, and the site's own
    /// separation of it from the furniture is what the record holds.
    Served,
}

impl fmt::Display for ExtractionRules {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Heuristic => formatter.write_str("heuristic"),
            Self::Site(host) => write!(formatter, "site:{host}"),
            Self::Served => formatter.write_str("served"),
        }
    }
}

/// One string and not a value whose shape depends on which variant it is.
///
/// `rules` was a string when the only answer was `heuristic`, and every reader that filters on
/// it, `jq` at a prompt included, compares it to one. Turning it into an object for the second
/// variant would break all of them for nothing, so the host is spelled inside the string.
impl Serialize for ExtractionRules {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ExtractionRules {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let spelled = String::deserialize(deserializer)?;
        if spelled == "heuristic" {
            return Ok(Self::Heuristic);
        }
        if spelled == "served" {
            return Ok(Self::Served);
        }
        match spelled.strip_prefix("site:") {
            Some(host) if !host.is_empty() => Ok(Self::Site(host.to_owned())),
            // A record naming rules this extractor cannot account for is refused rather than
            // read as the heuristic, which would claim the page was read with nothing said
            // about it.
            _ => Err(serde::de::Error::custom(format!(
                "{spelled:?} does not name an extraction rule"
            ))),
        }
    }
}

/// A ceiling the extraction reached, recorded rather than silently applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArticleBound {
    /// The excerpt was cut. What is stored is enough for review, not the whole page claim.
    Excerpt,
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
    /// measurement. It is wrong for languages that do not put spaces between words, which is
    /// why it is not what the sliver rule weighs.
    pub word_count: usize,
    /// What the sliver rule measured on this page, recorded for the articles it kept and not
    /// only for the pages it refused, on the same terms as `cost` below. A file per refusal
    /// says the rule is firing; only the shares that real articles reach can say whether the
    /// rule is about to start firing on them.
    ///
    /// Absent on records written before the rule existed, which is not the same as a page that
    /// measured nothing. A record must not answer a question the extractor that wrote it was
    /// never asked.
    ///
    /// A served document is measured like any other rather than left absent, and its two counts
    /// are equal because the document is the whole page. That keeps the absence above meaning
    /// one thing, which is the only reason it is worth reading.
    pub share: Option<ProseShare>,
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

/// How much of a page's text the article taken out of it holds.
///
/// The two counts and not the ratio between them. A ratio is a division somebody already did,
/// at a precision they chose, and the calibration these exist for is a question about the
/// distribution of both sides. Characters and not words, because the rule that reads them has
/// to mean the same thing in a language that does not separate words with spaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProseShare {
    /// The extracted article's own text, without the Markdown that will be written around it.
    pub article_chars: usize,
    /// The text of the document the scorer was handed, with the bodies of scripts, styles and
    /// templates left out and the rest left in: navigation, banners, and every block the scorer
    /// discarded. It is not what a reader would have seen, since a page can hide text with CSS
    /// and this never resolves it.
    ///
    /// The document and not the page, because a host's rule may have narrowed one to the other
    /// before this was counted. `rules` on the record beside it is what says which, and a
    /// calibration that mixes the two is comparing an article against itself.
    pub page_chars: usize,
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
/// The three ways of not producing an article are separate answers because each says something
/// different. A page that is not markup has nothing this extractor reads. A page that is markup
/// but not an article is worth marking so a later pass does not spend the same parse again. A
/// page that produced prose and was then refused is a disagreement between the algorithm and the
/// rule below it, and that disagreement is the material the numbers can be corrected against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Extraction {
    Article(Article),
    Refused(RefusedExtraction),
    NotArticle(NonArticle),
    Nothing,
}

/// A page the extractor read and declined to call an article.
///
/// This is narrower than `Extraction::Nothing`: a PDF or an image has nothing this extractor
/// reads, while this record says HTML was read and the answer was deliberately no.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NonArticle {
    pub extractor_version: u32,
    pub rules: ExtractionRules,
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
    pub share: ProseShare,
    /// What the prose said, so that reviewing the decision does not require re-deriving it.
    pub excerpt: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub truncated: Vec<ArticleBound>,
}
