//! Reading the markup: the one file that knows which HTML parser this project uses.
//!
//! Nothing here decides what a value means. It collects what the page literally said, with
//! a ceiling on every collection, and hands that to the resolution rules. The split is what
//! lets the parser be replaced without touching the precedence between OpenGraph and
//! schema.org, and the ceilings are what make an adversarial page cost a bounded amount.

use std::cell::RefCell;
use std::collections::BTreeSet;

use lol_html::errors::RewritingError;
use lol_html::{HtmlRewriter, MemorySettings, Settings, element, text};

use super::model::{AssetKind, Bound};

/// Ceilings on what one page may contribute. A page is remote input, so every one of these
/// is a bound on memory as much as on the size of the record. They are generous against
/// real pages, which is the point: reaching one is evidence about the page, not about the
/// limit, and the record says so.
const MAX_TEXT_FIELD_BYTES: usize = 4 * 1024;
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
    let scanner = RefCell::new(Scanner::default());

    {
        let mut rewriter = HtmlRewriter::new(
            Settings {
                element_content_handlers: vec![
                    // First wins, as everywhere else here. A document with a second `<html>`
                    // tag is malformed markup that template concatenation produces by
                    // accident and a hostile page can produce on purpose, and assigning
                    // unconditionally let the later one either replace the language or, when
                    // it carried no `lang` at all, erase it.
                    element!("html", |el| {
                        let mut scanner = scanner.borrow_mut();
                        if scanner.page.language.is_none() {
                            scanner.page.language = el.get_attribute("lang");
                        }
                        Ok(())
                    }),
                    // Matching on `head > title` would have missed every page that leaves the
                    // `<head>` tag out and lets the parser imply it, which is legal and
                    // common, because a token stream has no implied elements to match. What
                    // that would have excluded is handled by the next handler instead.
                    element!("title", |_| {
                        scanner.borrow_mut().see_title_start();
                        Ok(())
                    }),
                    // The `<title>` of an inline graphic is that graphic's accessible name,
                    // not the page's, and a logo in the header of a page whose own title
                    // comes later would otherwise win on document order. An ancestor is the
                    // only thing that separates the two: the namespace does not, because
                    // this element is an HTML integration point and the parser reports it in
                    // the HTML namespace exactly as it reports the page's own title.
                    //
                    // It runs after the handler above and undoes it, which is why the order
                    // of this list is not incidental.
                    element!("svg title", |_| {
                        scanner.borrow_mut().reject_title();
                        Ok(())
                    }),
                    text!("title", |chunk| {
                        scanner.borrow_mut().see_title(chunk.as_str());
                        Ok(())
                    }),
                    element!("meta", |el| {
                        scanner.borrow_mut().see_meta(
                            el.get_attribute("name")
                                .or_else(|| el.get_attribute("property")),
                            el.get_attribute("content"),
                        );
                        Ok(())
                    }),
                    element!("base[href]", |el| {
                        let mut scanner = scanner.borrow_mut();
                        // The first one wins, which is what a browser does with the second.
                        if scanner.page.base_href.is_none() {
                            scanner.page.base_href = el.get_attribute("href");
                        }
                        Ok(())
                    }),
                    element!("link[href]", |el| {
                        scanner
                            .borrow_mut()
                            .see_link_tag(el.get_attribute("rel"), el.get_attribute("href"));
                        Ok(())
                    }),
                    element!("a[href]", |el| {
                        scanner
                            .borrow_mut()
                            .see_anchor(el.get_attribute("href"), el.get_attribute("rel"));
                        Ok(())
                    }),
                    element!("script", |el| {
                        let mut scanner = scanner.borrow_mut();
                        if let Some(src) = el.get_attribute("src") {
                            scanner.see_asset(src, AssetKind::Script);
                        }
                        // Compared here rather than in the selector because the attribute is
                        // written in whatever case the page's generator felt like, and a CSS
                        // attribute match is case sensitive.
                        scanner.inside_json_ld = el
                            .get_attribute("type")
                            .is_some_and(|kind| kind.trim().eq_ignore_ascii_case(JSON_LD_TYPE));
                        Ok(())
                    }),
                    text!("script", |chunk| {
                        scanner
                            .borrow_mut()
                            .see_json_ld(chunk.as_str(), chunk.last_in_text_node());
                        Ok(())
                    }),
                    element!("img", |el| {
                        let mut scanner = scanner.borrow_mut();
                        if let Some(src) = el.get_attribute("src") {
                            scanner.see_asset(src, AssetKind::Image);
                        }
                        scanner.see_srcset(el.get_attribute("srcset"));
                        Ok(())
                    }),
                    // A `<source>` names an image inside `<picture>` and a media file inside
                    // `<video>` or `<audio>`, and a token stream cannot see which parent it
                    // is under. The attribute is the tell: the picture form carries
                    // `srcset`, the media form carries `src`.
                    element!("source", |el| {
                        let mut scanner = scanner.borrow_mut();
                        if let Some(src) = el.get_attribute("src") {
                            scanner.see_asset(src, AssetKind::Media);
                        }
                        scanner.see_srcset(el.get_attribute("srcset"));
                        Ok(())
                    }),
                    element!("video", |el| {
                        let mut scanner = scanner.borrow_mut();
                        if let Some(src) = el.get_attribute("src") {
                            scanner.see_asset(src, AssetKind::Media);
                        }
                        if let Some(poster) = el.get_attribute("poster") {
                            scanner.see_asset(poster, AssetKind::Image);
                        }
                        Ok(())
                    }),
                    element!("audio[src]", |el| {
                        if let Some(src) = el.get_attribute("src") {
                            scanner.borrow_mut().see_asset(src, AssetKind::Media);
                        }
                        Ok(())
                    }),
                ],
                memory_settings: MemorySettings {
                    max_allowed_memory_usage: MAX_PARSER_MEMORY_BYTES,
                    ..MemorySettings::new()
                },
                strict: false,
                ..Settings::new()
            },
            // The rewritten output is the input, and this is a reader: dropping it is what
            // keeps the cost of a large page the size of its tokens rather than of itself.
            |_: &[u8]| {},
        );

        rewriter.write(html.as_bytes())?;
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

    fn see_title(&mut self, chunk: &str) {
        if !self.inside_page_title || self.title_ended {
            return;
        }
        if push_capped(&mut self.title, chunk, MAX_TEXT_FIELD_BYTES) {
            self.title_ended = true;
            self.page.truncated.insert(Bound::Title);
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
    /// optional descriptor. Every candidate is the same image at another size, so all of
    /// them are recorded: which one a reader would have been served depends on a viewport
    /// the archive does not have.
    fn see_srcset(&mut self, srcset: Option<String>) {
        let Some(srcset) = srcset else {
            return;
        };
        for url in srcset_urls(&srcset) {
            self.see_asset(url.to_owned(), AssetKind::Image);
        }
    }

    fn finish(mut self) -> ScannedPage {
        // A document that ends inside a script leaves the last block unflushed, and a
        // truncated page is exactly the case where the metadata is worth the most.
        self.finish_json_ld_block();
        let title = self.title.trim();
        if !title.is_empty() {
            self.page.title = Some(title.to_owned());
        }
        self.page
    }
}

/// The URLs a `srcset` lists, in the order it lists them.
///
/// The separator is a comma, and a comma is legal inside a URL: an image served through a
/// transformation network spells its parameters that way, `w_320,h_213,c_fill`. Splitting on
/// the character alone turns one candidate into a handful of fragments, and a fragment is a
/// relative reference, so the archive ends up asking the page's own origin for addresses that
/// were never on the page. That is a request the page did not make, which is a rate limit at
/// best and a page choosing where the archive knocks at worst.
///
/// What separates candidates is therefore the end of the URL rather than the character: a URL
/// runs to whitespace, and the comma that follows it, or the commas it ends with when it
/// carries no descriptor, are the separator. This is the specification's own reading, and the
/// only one under which `a.png,b.png` is the single address a browser requests.
///
/// What is deliberately not done is validating the descriptor. A browser drops a candidate
/// whose descriptor is malformed, and this keeps it, because the two are answering different
/// questions: a browser is choosing which one image to fetch for a viewport, and the archive
/// is recording every address the page listed, having already said above that it does not model
/// that choice. It buys no safety either way, since a page that wants a request made writes a
/// descriptor that is valid.
///
/// Lazy rather than collected, so an attribute holding more candidates than the archive will
/// keep costs the ceiling above rather than a vector the size of the attribute.
fn srcset_urls(srcset: &str) -> impl Iterator<Item = &str> {
    let bytes = srcset.as_bytes();
    let mut position = 0;

    std::iter::from_fn(move || {
        loop {
            // Whitespace and commas are skipped together, so an empty candidate between two
            // separators disappears rather than becoming an empty address.
            while position < bytes.len()
                && (bytes[position].is_ascii_whitespace() || bytes[position] == b',')
            {
                position += 1;
            }
            let start = position;
            while position < bytes.len() && !bytes[position].is_ascii_whitespace() {
                position += 1;
            }
            if start == position {
                return None;
            }

            let token = &srcset[start..position];
            let url = token.trim_end_matches(',');
            // A URL that kept its last character ran to whitespace rather than to a separator,
            // so a descriptor follows and reaches to the next comma. A descriptor may hold one
            // inside parentheses, and being inside them is a state rather than a depth: a
            // second opening parenthesis is content there, so counting it would hide the comma
            // that ends the candidate and swallow the next one.
            if url.len() == token.len() {
                let mut inside_parentheses = false;
                while position < bytes.len() {
                    match bytes[position] {
                        b'(' => inside_parentheses = true,
                        b')' => inside_parentheses = false,
                        b',' if !inside_parentheses => {
                            position += 1;
                            break;
                        }
                        _ => {}
                    }
                    position += 1;
                }
            }
            if !url.is_empty() {
                return Some(url);
            }
        }
    })
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
