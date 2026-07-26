//! Turning what a page said into what the record holds.
//!
//! Three or four tags routinely claim the same field, and they disagree. The order between
//! them is a judgement, so it is written down here in one place, applied uniformly, and
//! recorded on every value it produces: a title that came out wrong should be traceable to
//! the rule that chose it rather than to a guess about which tag the page happened to have.

use std::collections::{BTreeMap, BTreeSet};

use jiff::Timestamp;
use jiff::civil::{Date, DateTime, Time};
use jiff::tz::TimeZone;
use serde_json::Value;
use url::Url;

use super::model::{
    Attributed, EXTRACTOR_VERSION, MetaTag, MetadataSource, OutboundLink, PageMetadata,
    PublicationDate, ReferencedAsset,
};
use super::scan::ScannedPage;

/// The address every relative URL on the page is resolved against, and the identity a link
/// is same-host against.
struct PageAddress {
    base: Url,
    host: Option<String>,
}

pub(super) fn resolve(page: ScannedPage, final_url: &str) -> PageMetadata {
    let address = PageAddress::of(final_url, page.base_href.as_deref());
    // First wins. A page that carries `og:title` twice meant the first one, and the second
    // is either a template that ran twice or an attempt to be read differently by whoever
    // reads furthest.
    let mut metas: BTreeMap<&str, &str> = BTreeMap::new();
    for (name, content) in &page.metas {
        metas.entry(name.as_str()).or_insert(content.as_str());
    }
    let json_ld: Vec<Value> = page
        .json_ld
        .iter()
        // A block that is not JSON is dropped rather than recorded: the record would hold a
        // string nothing can read, and the body it came from is still in the archive.
        .filter_map(|block| serde_json::from_str::<Value>(block).ok())
        .collect();
    let nodes = schema_nodes(&json_ld);

    PageMetadata {
        extractor_version: EXTRACTOR_VERSION,
        title: resolve_title(&metas, &nodes, page.title.as_deref()),
        description: resolve_description(&metas, &nodes),
        author: resolve_author(&metas, &nodes),
        site_name: resolve_site_name(&metas, &nodes),
        language: resolve_language(&metas, page.language.as_deref()),
        published_at: resolve_published_at(&metas, &nodes),
        declared_canonical_url: page
            .declared_canonical
            .as_deref()
            .and_then(|href| address.absolute(href)),
        meta: page
            .metas
            .iter()
            .map(|(name, content)| MetaTag {
                name: name.clone(),
                content: content.clone(),
            })
            .collect(),
        json_ld,
        links: resolve_links(&page, &address),
        assets: resolve_assets(&page, &address),
        truncated: page.truncated.into_iter().collect(),
    }
}

impl PageAddress {
    fn of(final_url: &str, base_href: Option<&str>) -> Self {
        // A capture only exists for a URL the archive could parse, so the fallback is for
        // the caller that hands this an address from somewhere else. It makes every URL on
        // the page unresolvable, which is the honest answer when the page has no address.
        let Ok(page) = Url::parse(final_url) else {
            return Self {
                base: Url::parse("about:blank").expect("about:blank parses"),
                host: None,
            };
        };
        let host = page.host_str().map(str::to_owned);
        // A `<base href>` that does not resolve, or that points at something no fetch can
        // follow, leaves the page's own address in charge rather than breaking every link.
        let base = base_href
            .and_then(|href| page.join(href.trim()).ok())
            .filter(|base| matches!(base.scheme(), "http" | "https"))
            .unwrap_or(page);
        Self { base, host }
    }

    /// Resolves one reference to an absolute address, or refuses it.
    ///
    /// Anything that is not http or https is dropped: a `javascript:`, a `mailto:` or a
    /// `data:` reference names something no capture can ever fetch, and keeping them would
    /// leave every consumer of this list to filter them again.
    ///
    /// The fragment goes, for the reason canonicalization drops it: it is resolved by the
    /// client against bytes the server already sent, so two links differing only there name
    /// one fetch and would otherwise be counted twice.
    fn absolute(&self, reference: &str) -> Option<String> {
        let mut url = self.base.join(reference.trim()).ok()?;
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        url.set_fragment(None);
        Some(url.into())
    }

    fn is_same_host(&self, url: &str) -> bool {
        match (
            &self.host,
            Url::parse(url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned)),
        ) {
            (Some(page), Some(target)) => *page == target,
            _ => false,
        }
    }
}

fn resolve_title(
    metas: &BTreeMap<&str, &str>,
    nodes: &[&Value],
    document_title: Option<&str>,
) -> Option<Attributed> {
    from_meta(metas, "og:title", MetadataSource::OpenGraph)
        .or_else(|| from_meta(metas, "twitter:title", MetadataSource::Twitter))
        .or_else(|| {
            // `headline` is what schema.org calls an article's title; `name` is the generic
            // form every other type uses.
            from_schema(nodes, "headline").or_else(|| from_schema(nodes, "name"))
        })
        .or_else(|| attributed(document_title, MetadataSource::Html))
}

fn resolve_description(metas: &BTreeMap<&str, &str>, nodes: &[&Value]) -> Option<Attributed> {
    from_meta(metas, "og:description", MetadataSource::OpenGraph)
        .or_else(|| from_meta(metas, "twitter:description", MetadataSource::Twitter))
        .or_else(|| from_schema(nodes, "description"))
        .or_else(|| from_meta(metas, "description", MetadataSource::Html))
}

/// The structured form is preferred because it names a person: `<meta name="author">` is
/// free text that sites fill with a byline, a company or a CMS user id, and `article:author`
/// is most often a profile URL rather than a name, which is why it is the last resort.
fn resolve_author(metas: &BTreeMap<&str, &str>, nodes: &[&Value]) -> Option<Attributed> {
    named_entity(nodes, "author")
        .map(|value| Attributed {
            value,
            source: MetadataSource::SchemaOrg,
        })
        .or_else(|| from_meta(metas, "author", MetadataSource::Html))
        .or_else(|| from_meta(metas, "article:author", MetadataSource::OpenGraph))
}

fn resolve_site_name(metas: &BTreeMap<&str, &str>, nodes: &[&Value]) -> Option<Attributed> {
    from_meta(metas, "og:site_name", MetadataSource::OpenGraph).or_else(|| {
        named_entity(nodes, "publisher").map(|value| Attributed {
            value,
            source: MetadataSource::SchemaOrg,
        })
    })
}

/// The `lang` attribute is preferred over `og:locale` because it is the one a browser acts
/// on, so it is the one an author notices being wrong.
fn resolve_language(
    metas: &BTreeMap<&str, &str>,
    document_language: Option<&str>,
) -> Option<Attributed> {
    attributed(document_language, MetadataSource::Html)
        .or_else(|| from_meta(metas, "og:locale", MetadataSource::OpenGraph))
}

fn resolve_published_at(metas: &BTreeMap<&str, &str>, nodes: &[&Value]) -> Option<PublicationDate> {
    let (raw, source) = from_meta(metas, "article:published_time", MetadataSource::OpenGraph)
        .or_else(|| from_schema(nodes, "datePublished"))
        .or_else(|| from_meta(metas, "date", MetadataSource::Html))
        .map(|found| (found.value, found.source))?;

    Some(PublicationDate {
        timestamp: parse_date(&raw),
        raw,
        source,
    })
}

/// Reads a date in the forms pages actually publish.
///
/// A form without an offset is read as UTC. That is an assumption and it can be off by a
/// day at the edges, which is why the raw string is kept beside the result: the record says
/// what the page said, and what this build made of it.
fn parse_date(raw: &str) -> Option<Timestamp> {
    let raw = raw.trim();
    if let Ok(timestamp) = raw.parse::<Timestamp>() {
        return Some(timestamp);
    }
    if let Ok(datetime) = raw.parse::<DateTime>() {
        return datetime
            .to_zoned(TimeZone::UTC)
            .ok()
            .map(|zoned| zoned.timestamp());
    }
    if let Ok(date) = raw.parse::<Date>() {
        return date
            .to_datetime(Time::midnight())
            .to_zoned(TimeZone::UTC)
            .ok()
            .map(|zoned| zoned.timestamp());
    }
    None
}

fn resolve_links(page: &ScannedPage, address: &PageAddress) -> Vec<OutboundLink> {
    let mut seen = BTreeSet::new();
    let mut links = Vec::new();
    for link in &page.links {
        let Some(url) = address.absolute(&link.href) else {
            continue;
        };
        if !seen.insert(url.clone()) {
            continue;
        }
        links.push(OutboundLink {
            same_host: address.is_same_host(&url),
            url,
            rel: link.rel.clone(),
        });
    }
    links
}

/// Deduplicated by address alone. The same file referenced as an icon and as an image is
/// one fetch, and the first role it appeared in is as good an answer as the second.
fn resolve_assets(page: &ScannedPage, address: &PageAddress) -> Vec<ReferencedAsset> {
    let mut seen = BTreeSet::new();
    let mut assets = Vec::new();
    for (reference, kind) in &page.assets {
        let Some(url) = address.absolute(reference) else {
            continue;
        };
        if !seen.insert(url.clone()) {
            continue;
        }
        assets.push(ReferencedAsset { url, kind: *kind });
    }
    assets
}

fn attributed(value: Option<&str>, source: MetadataSource) -> Option<Attributed> {
    let value = value?.trim();
    (!value.is_empty()).then(|| Attributed {
        value: value.to_owned(),
        source,
    })
}

fn from_meta(
    metas: &BTreeMap<&str, &str>,
    name: &str,
    source: MetadataSource,
) -> Option<Attributed> {
    attributed(metas.get(name).copied(), source)
}

fn from_schema(nodes: &[&Value], field: &str) -> Option<Attributed> {
    let value = nodes
        .iter()
        .find_map(|node| node.get(field).and_then(Value::as_str))?;
    attributed(Some(value), MetadataSource::SchemaOrg)
}

/// Reads a field that schema.org lets be a string, an object with a `name`, or a list of
/// either. All three spellings are common in the wild for `author` and `publisher`.
fn named_entity(nodes: &[&Value], field: &str) -> Option<String> {
    fn name_of(value: &Value) -> Option<String> {
        match value {
            Value::String(name) => Some(name.clone()),
            Value::Object(_) => value.get("name").and_then(Value::as_str).map(str::to_owned),
            Value::Array(entries) => entries.iter().find_map(name_of),
            _ => None,
        }
    }

    nodes
        .iter()
        .find_map(|node| node.get(field).and_then(name_of))
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
}

/// Types that describe the site, its navigation, or something the page merely mentions,
/// rather than the page in front of the reader.
///
/// A `@graph` almost always opens with one of them, and they carry the same field names the
/// page's own node does: a `WebSite` has a `name` and it is the name of the site, a `Person`
/// has one and it is the author's. `WebPage` is deliberately absent: that node is the page,
/// and its `name` is the page's.
const NOT_THE_PAGE: [&str; 8] = [
    "website",
    "breadcrumblist",
    "sitenavigationelement",
    "organization",
    "itemlist",
    "listitem",
    "person",
    "imageobject",
];

/// Flattens the shapes a JSON-LD block arrives in into the objects that carry fields: a
/// bare object, a list of them, or a `@graph` holding the list. Nesting deeper than that is
/// not followed, because a field found at an arbitrary depth belongs to some sub-entity of
/// the page rather than to the page.
///
/// The nodes that describe the site rather than its content are dropped, since they answer
/// to the same field names and usually come first. When every node is one of those, the
/// list is kept whole: a page whose only structured data describes the site is better read
/// from it than from nothing, and that is also the behavior a reader of an older record
/// would recognize.
fn schema_nodes(blocks: &[Value]) -> Vec<&Value> {
    let mut nodes = Vec::new();
    for block in blocks {
        match block {
            Value::Array(entries) => nodes.extend(entries.iter()),
            Value::Object(_) => {
                if let Some(Value::Array(graph)) = block.get("@graph") {
                    nodes.extend(graph.iter());
                }
                nodes.push(block);
            }
            _ => {}
        }
    }

    let about_the_page: Vec<&Value> = nodes
        .iter()
        .copied()
        .filter(|node| !describes_the_site(node))
        .collect();
    if about_the_page.is_empty() {
        nodes
    } else {
        about_the_page
    }
}

/// `@type` is a string on most nodes and a list on some, and either can name a type this
/// does not want to read the page's fields out of.
fn describes_the_site(node: &Value) -> bool {
    fn is_structural(value: &Value) -> bool {
        value
            .as_str()
            .is_some_and(|kind| NOT_THE_PAGE.contains(&kind.trim().to_ascii_lowercase().as_str()))
    }

    match node.get("@type") {
        Some(Value::Array(kinds)) => kinds.iter().any(is_structural),
        Some(kind) => is_structural(kind),
        None => false,
    }
}
