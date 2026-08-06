//! Reading the markup: the one file that knows which HTML parser this project uses.
//!
//! Nothing here decides what a value means. It collects what the page literally said, with
//! a ceiling on every collection, and hands that to the resolution rules. The split is what
//! lets the parser be replaced without touching the precedence between OpenGraph and
//! schema.org, and the ceilings are what make an adversarial page cost a bounded amount.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::BTreeSet;

use html_escape::decode_html_entities;
use lol_html::errors::RewritingError;
use lol_html::{HtmlRewriter, MemorySettings, Settings, element, text};

use super::model::{AssetKind, Bound};

/// Ceilings on what one page may contribute. A page is remote input, so every one of these
/// is a bound on memory as much as on the size of the record. They are generous against
/// real pages, which is the point: reaching one is evidence about the page, not about the
/// limit, and the record says so.
const MAX_TEXT_FIELD_BYTES: usize = 4 * 1024;
/// How much of a title's own raw markup is held before it is decoded.
///
/// A single character reference can spell one decoded character with far more raw bytes than
/// `MAX_TEXT_FIELD_BYTES` allows, a numeric reference's leading zeros being the extreme case,
/// so capping the raw accumulation at that same ceiling would cut a legitimate title before it
/// is ever read. This ceiling exists only to bound the memory an adversarial title element can
/// hold before decoding runs; how much of the decoded result the record keeps is still
/// `MAX_TEXT_FIELD_BYTES`, applied once decoding is done.
const MAX_TITLE_RAW_BYTES: usize = 64 * MAX_TEXT_FIELD_BYTES;
const MAX_META_TAGS: usize = 256;
const MAX_JSON_LD_BLOCKS: usize = 16;
const MAX_JSON_LD_BYTES: usize = 64 * 1024;
const MAX_LINKS: usize = 2048;
const MAX_ASSETS: usize = 2048;
/// A URL longer than this is not a URL anyone linked, it is a payload. Browsers stop caring
/// somewhere near here too.
const MAX_URL_BYTES: usize = 2048;
/// What the parser may hold on top of the document it was handed.
///
/// It is a backstop and not a limit this reaches: the page arrives in one piece, so the
/// parser reads tokens out of it in place and buffers only what a token spanning two writes
/// would need, which is nothing here. The ceiling is what keeps that true if the document
/// ever starts arriving in chunks.
const MAX_PARSER_MEMORY_BYTES: usize = 8 * 1024 * 1024;

/// What one page said, before any of it is interpreted. URLs are still relative, dates are
/// still strings, and nothing has been chosen between two tags that claim the same field.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ScannedPage {
    pub title: Option<String>,
    pub language: Option<String>,
    pub base_href: Option<String>,
    pub metas: Vec<(String, String)>,
    pub json_ld: Vec<String>,
    pub declared_canonical: Option<String>,
    pub links: Vec<ScannedLink>,
    pub assets: Vec<(String, AssetKind)>,
    pub truncated: BTreeSet<Bound>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct ScannedLink {
    pub href: String,
    pub rel: Option<String>,
}

/// Reads a decoded page.
///
/// The parser runs in non-strict mode. Strict mode exists so that a rewriter never emits
/// markup whose meaning it guessed at, and nothing here emits anything: refusing a page over
/// an ambiguous parse would trade every field of a merely malformed document, which is a
/// large share of the archivable web, for a guarantee that only matters when writing HTML
/// back out.
pub(super) fn scan(html: &str) -> Result<ScannedPage, RewritingError> {
    scan_writes([html])
}

/// The body of `scan`, taking the document one write at a time.
///
/// A capture is always handed to `scan` whole, so production code never needs more than one
/// write. This exists so a test can hand the parser the same document split at a chosen byte
/// offset, which is the only way to pin behaviour that depends on where a chunk boundary
/// falls, a character reference split across two of them being the case that matters here.
fn scan_writes<'a>(
    writes: impl IntoIterator<Item = &'a str>,
) -> Result<ScannedPage, RewritingError> {
    let scanner = RefCell::new(Scanner::default());

    {
        let mut rewriter = HtmlRewriter::new(
            Settings::new()
                .with_memory_settings(
                    MemorySettings::new().with_max_allowed_memory_usage(MAX_PARSER_MEMORY_BYTES),
                )
                .with_strict(false)
                // First wins, as everywhere else here. A document with a second `<html>`
                // tag is malformed markup that template concatenation produces by
                // accident and a hostile page can produce on purpose, and assigning
                // unconditionally let the later one either replace the language or, when
                // it carried no `lang` at all, erase it.
                .append_element_content_handler(element!("html", |el| {
                    let mut scanner = scanner.borrow_mut();
                    if scanner.page.language.is_none() {
                        scanner.page.language = el.get_attribute("lang").map(decode_entities);
                    }
                    Ok(())
                }))
                // Matching on `head > title` would have missed every page that leaves the
                // `<head>` tag out and lets the parser imply it, which is legal and
                // common, because a token stream has no implied elements to match. What
                // that would have excluded is handled by the next handler instead.
                .append_element_content_handler(element!("title", |_| {
                    scanner.borrow_mut().see_title_start();
                    Ok(())
                }))
                // The `<title>` of an inline graphic is that graphic's accessible name,
                // not the page's, and a logo in the header of a page whose own title
                // comes later would otherwise win on document order. An ancestor is the
                // only thing that separates the two: the namespace does not, because
                // this element is an HTML integration point and the parser reports it in
                // the HTML namespace exactly as it reports the page's own title.
                //
                // It runs after the handler above and undoes it, which is why the order
                // these are appended in is not incidental.
                .append_element_content_handler(element!("svg title", |_| {
                    scanner.borrow_mut().reject_title();
                    Ok(())
                }))
                // A formula names itself the same way and needs saying twice, because a
                // selector matches a tag name and never a namespace: the one above cannot
                // reach a `<title>` with no `<svg>` over it, and the page's own handler
                // reaches this one exactly as it reaches the page's title.
                .append_element_content_handler(element!("math title", |_| {
                    scanner.borrow_mut().reject_title();
                    Ok(())
                }))
                .append_element_content_handler(text!("title", |chunk| {
                    scanner.borrow_mut().see_title(chunk.as_str());
                    Ok(())
                }))
                .append_element_content_handler(element!("meta", |el| {
                    scanner.borrow_mut().see_meta(
                        el.get_attribute("name")
                            .or_else(|| el.get_attribute("property"))
                            .map(decode_entities),
                        el.get_attribute("content").map(decode_entities),
                    );
                    Ok(())
                }))
                .append_element_content_handler(element!("base[href]", |el| {
                    let mut scanner = scanner.borrow_mut();
                    // The first one wins, which is what a browser does with the second.
                    if scanner.page.base_href.is_none() {
                        scanner.page.base_href = el.get_attribute("href").map(decode_entities);
                    }
                    Ok(())
                }))
                .append_element_content_handler(element!("link[href]", |el| {
                    scanner.borrow_mut().see_link_tag(
                        el.get_attribute("rel").map(decode_entities),
                        el.get_attribute("href").map(decode_entities),
                    );
                    Ok(())
                }))
                .append_element_content_handler(element!("a[href]", |el| {
                    scanner.borrow_mut().see_anchor(
                        el.get_attribute("href").map(decode_entities),
                        el.get_attribute("rel").map(decode_entities),
                    );
                    Ok(())
                }))
                .append_element_content_handler(element!("script", |el| {
                    let mut scanner = scanner.borrow_mut();
                    if let Some(src) = el.get_attribute("src").map(decode_entities) {
                        scanner.see_asset(src, AssetKind::Script);
                    }
                    // Compared here rather than in the selector because the attribute is
                    // written in whatever case the page's generator felt like, and a CSS
                    // attribute match is case sensitive.
                    scanner.inside_json_ld = el
                        .get_attribute("type")
                        .map(decode_entities)
                        .is_some_and(|kind| kind.trim().eq_ignore_ascii_case(JSON_LD_TYPE));
                    Ok(())
                }))
                .append_element_content_handler(text!("script", |chunk| {
                    scanner
                        .borrow_mut()
                        .see_json_ld(chunk.as_str(), chunk.last_in_text_node());
                    Ok(())
                }))
                .append_element_content_handler(element!("img", |el| {
                    let mut scanner = scanner.borrow_mut();
                    if let Some(src) = el.get_attribute("src").map(decode_entities) {
                        scanner.see_asset(src, AssetKind::Image);
                    }
                    scanner.see_srcset(el.get_attribute("srcset"));
                    Ok(())
                }))
                // A `<source>` names an image inside `<picture>` and a media file inside
                // `<video>` or `<audio>`, and a token stream cannot see which parent it
                // is under. The attribute is the tell: the picture form carries
                // `srcset`, the media form carries `src`.
                .append_element_content_handler(element!("source", |el| {
                    let mut scanner = scanner.borrow_mut();
                    if let Some(src) = el.get_attribute("src").map(decode_entities) {
                        scanner.see_asset(src, AssetKind::Media);
                    }
                    scanner.see_srcset(el.get_attribute("srcset"));
                    Ok(())
                }))
                .append_element_content_handler(element!("video", |el| {
                    let mut scanner = scanner.borrow_mut();
                    if let Some(src) = el.get_attribute("src").map(decode_entities) {
                        scanner.see_asset(src, AssetKind::Media);
                    }
                    if let Some(poster) = el.get_attribute("poster").map(decode_entities) {
                        scanner.see_asset(poster, AssetKind::Image);
                    }
                    Ok(())
                }))
                .append_element_content_handler(element!("audio[src]", |el| {
                    if let Some(src) = el.get_attribute("src").map(decode_entities) {
                        scanner.borrow_mut().see_asset(src, AssetKind::Media);
                    }
                    Ok(())
                })),
            // The rewritten output is the input, and this is a reader: dropping it is what
            // keeps the cost of a large page the size of its tokens rather than of itself.
            |_: &[u8]| {},
        );

        for write in writes {
            rewriter.write(write.as_bytes())?;
        }
        rewriter.end()?;
    }

    Ok(scanner.into_inner().finish())
}

const JSON_LD_TYPE: &str = "application/ld+json";

#[derive(Debug, Default)]
struct Scanner {
    page: ScannedPage,
    title: String,
    inside_page_title: bool,
    title_ended: bool,
    inside_json_ld: bool,
    json_ld: String,
    json_ld_bytes: usize,
}

impl Scanner {
    /// The first title the page's own markup declares is the one, so a second `<title>`,
    /// which is malformed markup, cannot overwrite it.
    fn see_title_start(&mut self) {
        self.inside_page_title = self.title.is_empty() && !self.title_ended;
    }

    /// This `<title>` belongs to something inside the page rather than to the page.
    fn reject_title(&mut self) {
        self.inside_page_title = false;
    }

    // A character reference can split across two chunks, `&#x2` in one and `7;` in the
    // next, so it cannot be decoded as each chunk arrives: decoding runs once in `finish`,
    // against the whole buffer. What is capped here is only the raw byte count this
    // scanner is willing to hold before that happens.
    fn see_title(&mut self, chunk: &str) {
        if !self.inside_page_title || self.title_ended {
            return;
        }
        if push_capped(&mut self.title, chunk, MAX_TITLE_RAW_BYTES) {
            self.title_ended = true;
        }
    }

    fn see_meta(&mut self, name: Option<String>, content: Option<String>) {
        // A tag with neither `name` nor `property` declares an encoding or a header, and
        // both of those are already recorded somewhere this record does not duplicate.
        let (Some(name), Some(content)) = (name, content) else {
            return;
        };
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() {
            return;
        }
        if self.page.metas.len() >= MAX_META_TAGS {
            self.page.truncated.insert(Bound::MetaTags);
            return;
        }
        let mut capped = String::new();
        if push_capped(&mut capped, &content, MAX_TEXT_FIELD_BYTES) {
            self.page.truncated.insert(Bound::MetaContent);
        }
        self.page.metas.push((name, capped));
    }

    fn see_json_ld(&mut self, chunk: &str, ends_the_node: bool) {
        if !self.inside_json_ld {
            return;
        }
        if push_capped(&mut self.json_ld, chunk, MAX_JSON_LD_BYTES) {
            self.page.truncated.insert(Bound::JsonLd);
        }
        if ends_the_node {
            self.finish_json_ld_block();
        }
    }

    fn finish_json_ld_block(&mut self) {
        let block = std::mem::take(&mut self.json_ld);
        let block = block.trim();
        if block.is_empty() {
            return;
        }
        if self.page.json_ld.len() >= MAX_JSON_LD_BLOCKS
            || self.json_ld_bytes.saturating_add(block.len()) > MAX_JSON_LD_BYTES
        {
            self.page.truncated.insert(Bound::JsonLd);
            return;
        }
        self.json_ld_bytes += block.len();
        self.page.json_ld.push(block.to_owned());
    }

    fn see_link_tag(&mut self, rel: Option<String>, href: Option<String>) {
        let (Some(rel), Some(href)) = (rel, href) else {
            return;
        };
        let relations = rel.to_ascii_lowercase();
        let mut relations = relations.split_ascii_whitespace();
        // A `preload` or an `alternate` is deliberately not an asset: the first names bytes
        // the page may not end up using, the second names a different document.
        if relations.clone().any(|token| token == "canonical") {
            if self.page.declared_canonical.is_none() && href.len() <= MAX_URL_BYTES {
                self.page.declared_canonical = Some(href);
            }
            return;
        }
        if relations.clone().any(|token| token == "stylesheet") {
            self.see_asset(href, AssetKind::Stylesheet);
            // `icon`, `shortcut icon`, `apple-touch-icon`, `mask-icon`: the token is the
            // whole word or a suffix behind a dash, never an arbitrary ending.
        } else if relations.any(|token| token == "icon" || token.ends_with("-icon")) {
            self.see_asset(href, AssetKind::Icon);
        }
    }

    fn see_anchor(&mut self, href: Option<String>, rel: Option<String>) {
        let Some(href) = href else {
            return;
        };
        // A dropped link is one the record does not hold, which is the same claim the
        // ceiling on their number makes, so it is recorded the same way.
        if href.len() > MAX_URL_BYTES {
            self.page.truncated.insert(Bound::Links);
            return;
        }
        if self.page.links.len() >= MAX_LINKS {
            self.page.truncated.insert(Bound::Links);
            return;
        }
        self.page.links.push(ScannedLink {
            href,
            rel: rel
                .map(|rel| rel.trim().to_ascii_lowercase())
                .filter(|rel| !rel.is_empty()),
        });
    }

    fn see_asset(&mut self, url: String, kind: AssetKind) {
        if url.len() > MAX_URL_BYTES {
            self.page.truncated.insert(Bound::Assets);
            return;
        }
        if self.page.assets.len() >= MAX_ASSETS {
            self.page.truncated.insert(Bound::Assets);
            return;
        }
        self.page.assets.push((url, kind));
    }

    /// A `srcset` is a comma separated list of candidates, each a URL followed by an
    /// optional descriptor, and every candidate is the same picture at another size. One of
    /// them is recorded, the widest, because an archive has no viewport to choose against and
    /// what it has instead is a reader who wants the best quality the page offered. It is the
    /// address the conversion into a note already names, so the two agree and the note keeps
    /// its picture offline.
    ///
    /// Recording all of them cost a fetch and a copy per size for a picture the archive
    /// already held. A publication writing two formats at four widths turns one photograph
    /// into eight addresses, which is where four fifths of a collection's bytes went, and it
    /// turns the subresource pass's per capture ceiling into a budget of sixteen photographs
    /// a page: the one page in that collection that lost a picture lost it to its own
    /// renditions rather than to the platform's scripts.
    ///
    /// The grouping is the attribute and nothing wider. A `<picture>` naming one photograph in
    /// two formats still costs two references, because a token stream cannot see that a
    /// `<source>` and an `<img>` stand under one parent, and pairing the two by their
    /// addresses would be reading a transformation network's path convention as though a page
    /// had promised it.
    ///
    /// The attribute is decoded before the candidates are split, which is the order a browser
    /// reads it in: references are resolved while the tag is tokenized, and the candidate
    /// grammar runs on what that produced. Splitting first would make `&#44;` mean something
    /// no browser reads it as, since a page writing a comma that way is writing the separator
    /// and the candidates after it would be swallowed as one descriptor.
    ///
    /// Nothing is lost by decoding first, because the candidate grammar does not split on the
    /// comma: a URL runs to whitespace, so a comma inside one stays inside it whether the page
    /// spelled it raw or as a reference.
    fn see_srcset(&mut self, srcset: Option<String>) {
        let Some(srcset) = srcset else {
            return;
        };
        let srcset = decode_entities(srcset);
        if let Some(widest) = crate::srcset::widest(&srcset) {
            self.see_asset(widest.to_owned(), AssetKind::Image);
        }
    }

    fn finish(mut self) -> ScannedPage {
        // A document that ends inside a script leaves the last block unflushed, and a
        // truncated page is exactly the case where the metadata is worth the most.
        self.finish_json_ld_block();
        // Decoded here, now that the buffer is whole, and capped after: the field's ceiling
        // is about what a reader would see, and a reference only reads as itself once it is
        // resolved. Reaching `MAX_TITLE_RAW_BYTES` above already means the title is bigger
        // than what the record keeps, whatever this decoded prefix happens to measure, so
        // that case counts as truncated on its own.
        let decoded_title = decode_entities(std::mem::take(&mut self.title));
        let title = decoded_title.trim();
        // Outside the emptiness check below, because a title whose first bytes are all
        // whitespace reaches the raw ceiling and then trims to nothing: the record would
        // otherwise say the page had no title and that nothing was cut, when what happened
        // is that everything worth keeping was past the ceiling.
        if self.title_ended {
            self.page.truncated.insert(Bound::Title);
        }
        if !title.is_empty() {
            let mut capped = String::new();
            if push_capped(&mut capped, title, MAX_TEXT_FIELD_BYTES) {
                self.page.truncated.insert(Bound::Title);
            }
            self.page.title = Some(capped);
        }
        self.page
    }
}

/// An attribute value or an element's text, as the parser hands it back, still carries
/// whatever character references the page wrote: neither `get_attribute` nor a `text!` chunk
/// decodes the markup, only the byte encoding of the document. Everything downstream, the
/// title, a URL, a description, expects the character the page meant rather than the
/// reference that spells it, so this is the one place that turns `&#x27;` and `&amp;` into
/// `'` and `&` before anything else looks at the string.
///
/// It also has to run before every ceiling that measures a decoded value, not after: no
/// named or numeric reference in `html_escape`'s tables decodes to more bytes than it took to
/// write, so calling this first is what makes a ceiling measure the value a reader would see
/// rather than a page's choice of how verbosely to spell it.
fn decode_entities(value: String) -> String {
    match decode_html_entities(&value) {
        Cow::Borrowed(_) => value,
        Cow::Owned(decoded) => decoded,
    }
}

/// Appends what fits and reports whether the ceiling was reached. Characters are appended
/// whole so the result is never cut through the middle of one.
fn push_capped(buffer: &mut String, chunk: &str, cap: usize) -> bool {
    if buffer.len() >= cap {
        return true;
    }
    for character in chunk.chars() {
        if buffer.len() + character.len_utf8() > cap {
            return true;
        }
        buffer.push(character);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The trap this fix exists for: `&#x2` ends one write and `7;` begins the next, and
    /// decoding either half on its own turns the reference into nothing rather than into
    /// `'`. Only waiting for the whole buffer before decoding reads it correctly.
    ///
    /// A capture arrives whole and a reference carries no `<`, so the parser does not split
    /// one today. This is a seam held open on purpose against the day something feeds this
    /// scanner in pieces, and it fails against the code before this change on content alone.
    #[test]
    fn a_character_reference_split_across_a_chunk_boundary_still_decodes() {
        let page = scan_writes(["<title>Isn&#x2", "7;t</title>"])
            .expect("html this test wrote is readable");
        assert_eq!(page.title.as_deref(), Some("Isn't"));
        assert!(page.truncated.is_empty());
    }

    #[test]
    fn a_title_with_nothing_to_decode_is_unchanged() {
        let page = scan("<title>plain title</title>").expect("html this test wrote is readable");
        assert_eq!(page.title.as_deref(), Some("plain title"));
    }

    /// Capping the raw buffer at the field's own ceiling, which is what this fix removes,
    /// would have cut this title before it was ever decoded: spelled with a reference for
    /// every character, it runs well past `MAX_TEXT_FIELD_BYTES` as written despite reading
    /// far short of it once decoded, and the ceiling is only supposed to measure the latter.
    #[test]
    fn the_ceiling_is_measured_after_decoding_not_before() {
        let repetitions = MAX_TEXT_FIELD_BYTES / 5 + 100;
        let raw_title = "&amp;".repeat(repetitions);
        assert!(raw_title.len() > MAX_TEXT_FIELD_BYTES);

        let page =
            scan(&format!("<title>{raw_title}</title>")).expect("html this test wrote is readable");

        let title = page.title.expect("a title");
        assert_eq!(title.len(), repetitions);
        assert!(title.len() < MAX_TEXT_FIELD_BYTES);
        assert!(title.chars().all(|character| character == '&'));
        assert!(page.truncated.is_empty());
    }

    /// A title longer than the raw ceiling is cut before it is ever decoded, and the record
    /// has to say so: the ceiling is evidence about the page, which is the whole reason
    /// `Bound` exists.
    #[test]
    fn a_title_past_the_raw_ceiling_records_that_it_was_cut() {
        let raw_title = "a".repeat(MAX_TITLE_RAW_BYTES + 1);

        let page =
            scan(&format!("<title>{raw_title}</title>")).expect("html this test wrote is readable");

        assert!(page.truncated.contains(&Bound::Title));
        assert_eq!(page.title.expect("a title").len(), MAX_TEXT_FIELD_BYTES);
    }

    /// The case that made the truncation mark independent of whether a title survived. What
    /// the raw ceiling kept here is whitespace, so the decoded value trims away to nothing,
    /// and a record saying both that the page had no title and that nothing was cut would be
    /// asserting the opposite of what happened.
    #[test]
    fn a_title_cut_where_only_whitespace_fit_still_records_that_it_was_cut() {
        let padding = " ".repeat(MAX_TITLE_RAW_BYTES + 1);

        let page = scan(&format!("<title>{padding}Real Title</title>"))
            .expect("html this test wrote is readable");

        assert!(page.title.is_none());
        assert!(page.truncated.contains(&Bound::Title));
    }
}
