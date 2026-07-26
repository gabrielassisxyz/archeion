//! What an extraction produced: the record, and the vocabulary it is written in.
//!
//! Every field here is derived from bytes the archive already holds, which is why the
//! record carries the version of the extractor that produced it: a later, better pass over
//! the archive needs to know which files it has already improved.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// The extractor that produced a record. A stored record made by an older one is stale
/// rather than wrong, and a re-extraction pass keys on this to find what is worth redoing.
///
/// It is bumped when the meaning of a field changes or a rule that fills one changes, not
/// when a field is added: an added field is absent from older records, which already reads
/// correctly as "this extractor did not look for it".
pub const EXTRACTOR_VERSION: u32 = 1;

/// Where a resolved value came from. Kept beside the value because the precedence rules are
/// judgement calls: without this, a title that came out wrong gives nothing to look at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataSource {
    /// An `og:` meta tag.
    OpenGraph,
    /// A `twitter:` meta tag.
    Twitter,
    /// A JSON-LD block, which in practice means schema.org.
    SchemaOrg,
    /// The document's own markup: `<title>`, `<meta name="author">`, `<html lang>`.
    Html,
}

/// A value the extractor settled on, and the rule that won.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attributed {
    pub value: String,
    pub source: MetadataSource,
}

/// A publication date as the page stated it and, when it could be read, as an instant.
///
/// Both are kept because neither replaces the other. The parse is what makes the field
/// sortable; the raw string is what survives a date this extractor cannot read yet, and
/// dropping it would lose the only evidence that the page carried a date at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationDate {
    pub raw: String,
    /// Absent when the raw form is not a date this extractor knows how to read. A date
    /// written without a time or an offset is read as midnight UTC, which is an assumption
    /// the raw field above keeps recoverable.
    pub timestamp: Option<Timestamp>,
    pub source: MetadataSource,
}

/// One `<meta>` tag as it was written. The list is kept whole, on top of the resolved
/// fields above, because it is small, it answers "where did that come from", and it lets a
/// field this extractor does not know about yet be recovered without re-reading the body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetaTag {
    /// The `name` or the `property` attribute, lowercased. The two are merged because pages
    /// use them interchangeably for the same keys, and a reader looking for `og:title`
    /// should not have to know which spelling a particular site chose.
    pub name: String,
    pub content: String,
}

/// A link out of the page, resolved to an absolute address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundLink {
    pub url: String,
    pub rel: Option<String>,
    /// Whether the target is on the host the page was fetched from. It is recorded rather
    /// than recomputed because it is the field a crawl decision actually reads.
    pub same_host: bool,
}

/// A subresource the page referenced. This is the list a later asset capture works from;
/// nothing here has been fetched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferencedAsset {
    pub url: String,
    pub kind: AssetKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    Image,
    Stylesheet,
    Script,
    /// Audio and video.
    Media,
    Icon,
}

/// A ceiling the page reached. Recorded rather than silently applied, in the same spirit as
/// a truncated body: a record that says it holds all the links is different from one that
/// holds as many as the extractor was willing to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bound {
    Title,
    /// The list of `<meta>` tags stopped short.
    MetaTags,
    /// A `<meta>` tag's content was cut, which is a different claim from the one above:
    /// every tag is there, and one of them holds less than the page wrote.
    MetaContent,
    JsonLd,
    Links,
    Assets,
}

/// Everything the extractor read out of one captured page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageMetadata {
    pub extractor_version: u32,
    pub title: Option<Attributed>,
    pub description: Option<Attributed>,
    pub author: Option<Attributed>,
    pub site_name: Option<Attributed>,
    pub language: Option<Attributed>,
    pub published_at: Option<PublicationDate>,
    /// The address the page claims for itself, from `<link rel="canonical">`.
    ///
    /// It is recorded and never acted on. The address an item is filed under comes from the
    /// URL that was fetched, put through this project's own rules; letting a page's claim
    /// about itself decide where it lands would hand a remote document control over the
    /// layout of the archive.
    pub declared_canonical_url: Option<String>,
    pub meta: Vec<MetaTag>,
    /// JSON-LD blocks that parsed, kept as they were written. They stay untrusted: this is
    /// remote data that was copied into a record, not data this program vouches for.
    pub json_ld: Vec<serde_json::Value>,
    pub links: Vec<OutboundLink>,
    pub assets: Vec<ReferencedAsset>,
    /// Empty when the page fit inside every ceiling, which is the ordinary case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub truncated: Vec<Bound>,
}
