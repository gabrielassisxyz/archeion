//! Reading the document a site published, instead of reconstructing one from its markup.
//!
//! Some sites serve a Markdown copy of every page beside the HTML, which is what the `llms.txt`
//! convention proposes. Where a response arrives as one, no heuristic can beat taking it: it is
//! the author's own separation of the prose from the furniture rather than a guess at one.
//!
//! Nothing here trusts it. It arrives from the same place the HTML does, it passes through no
//! converter that escapes anything, and the ceilings that bound a parse bound nothing in a
//! document that is never parsed. So it is put through the same converter every extracted
//! article goes through, and weighed against the same ceilings on the way.
//! `docs/readability.md` has the reasoning and what the round trip costs.

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, html};
use url::Url;

use super::document::{MAX_DOCUMENT_BYTES, MAX_OPEN_ELEMENTS, TooExpensive, visible_chars};
use super::markdown;
use super::markup_scan::peak_open_elements;
use super::model::{
    AdmissionCost, Article, ArticleRecord, EXTRACTOR_VERSION, Extraction, ExtractionRules,
    NonArticle, ProseShare,
};

/// The schemes an archived document may still point at.
///
/// The list is short because a link destination is the one thing in a served document that a
/// reader may act on, and it is written by whoever wrote the document. `mailto` is here because
/// it is ordinary in prose and inert; everything absent, `data` and `vbscript` and `javascript`
/// among them, is a destination that exists to run rather than to be read.
const READABLE_SCHEMES: [&str; 3] = ["http", "https", "mailto"];

/// Reads a response that arrived as Markdown.
///
/// The refusals are spelled by the same type the HTML path refuses with, so a page turned away
/// for cost reads the same in a run report whichever of the two it came through.
pub(super) fn read(document: &str, final_url: &str) -> Result<Extraction, String> {
    if document.len() > MAX_DOCUMENT_BYTES {
        return Err(TooExpensive::Bytes {
            byte_len: document.len(),
        }
        .reason());
    }
    let rendered = render(document, final_url);
    // The markup just generated is balanced, so the quadratic parse the open-element ceiling
    // was measured against cannot come out of it. Depth can: a document of nothing but `>`
    // opens a blockquote per character, and it is the converter's parser below that pays for
    // it. Well-formed markup peaks at its own depth, so the same count answers both.
    let peak = peak_open_elements(&rendered.markup, MAX_OPEN_ELEMENTS);
    if peak > MAX_OPEN_ELEMENTS {
        return Err(TooExpensive::OpenElements { peak }.reason());
    }

    let mut truncated = Vec::new();
    // No title is handed in. A served document carries its own heading, and the metadata
    // extractor produces nothing for a response that is not markup, so the only title that
    // could be prepended here is one nobody has: what it would add is a second heading above
    // the document's own.
    let prose = markdown::render(&rendered.markup, None, &mut truncated)?;
    if prose.document.trim().is_empty() {
        // A response that is Markdown and holds no prose is a page the extractor read and
        // declined, which is the same answer as an HTML page that is not an article, and it is
        // what keeps a later pass from converting the same empty document again.
        return Ok(Extraction::NotArticle(NonArticle {
            extractor_version: EXTRACTOR_VERSION,
            rules: ExtractionRules::Served,
        }));
    }
    Ok(Extraction::Article(Article {
        record: ArticleRecord {
            extractor_version: EXTRACTOR_VERSION,
            rules: ExtractionRules::Served,
            word_count: markdown::word_count(&prose.body),
            // Equal on both sides, which is not a placeholder: the document is the page, so the
            // share is one by construction and the sliver rule cannot fire on it. That is the
            // same answer a `body` rule already gets, and for the same reason, which is that
            // somebody who looked at the site said where the prose is.
            share: Some(ProseShare {
                article_chars: rendered.text_chars,
                page_chars: rendered.text_chars,
            }),
            // The algorithm that fills these two on the HTML path never ran. A page description
            // and a byline are things this document did not say, and saying them anyway is the
            // claim the empty option exists to avoid.
            excerpt: None,
            byline: None,
            truncated,
            cost: AdmissionCost {
                document_bytes: document.len(),
                peak_open_elements: peak,
            },
        },
        markdown: prose.document,
    }))
}

/// The served document as markup, and the text a reader would have seen in it.
struct Rendered {
    markup: String,
    /// Counted the way both sides of the sliver rule are counted, so a served document and an
    /// extracted one report the same measurement of the same thing.
    text_chars: usize,
}

/// Turns the served document into the markup the converter reads.
///
/// This is the whole safety argument, and it is one sentence: the document goes through the
/// same converter every extracted article goes through, so it comes out in the same closed
/// vocabulary, escaped by the same escaper. That is what `docs/readability.md` already decided
/// for the title, which was the one page-controlled string that used to bypass the converter.
/// A served document is that problem with nothing left over.
///
/// Two things are handled here rather than left to the converter, because by the time markup
/// exists it is too late to tell them apart from markup this program wrote.
fn render(document: &str, final_url: &str) -> Rendered {
    let base = Url::parse(final_url).ok();
    let mut text_chars = 0;
    // Links and images do not nest inside themselves, so a count is enough to keep an end tag
    // with the start tag it belongs to, and it stays correct when a kept link follows a
    // dropped one.
    let (mut dropped_links, mut dropped_images) = (0usize, 0usize);

    let events = Parser::new_ext(document, extensions()).filter_map(|event| {
        if let Event::Text(text) | Event::Code(text) | Event::Html(text) | Event::InlineHtml(text) =
            &event
        {
            text_chars += visible_chars(text);
        }
        match event {
            // Raw HTML is what a Markdown document can carry that an extracted article never
            // can: every renderer passes it through, so a served page could put a script into
            // the archive where the HTML path would have converted one away. It is kept as
            // text rather than deleted, so the document still says what it said, inertly, on
            // the same terms the title escaping already set.
            Event::Html(raw) | Event::InlineHtml(raw) => Some(Event::Text(raw)),
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => match readable_destination(&dest_url, base.as_ref()) {
                Some(dest_url) => Some(Event::Start(Tag::Link {
                    link_type,
                    dest_url,
                    title,
                    id,
                })),
                None => {
                    dropped_links += 1;
                    None
                }
            },
            Event::End(TagEnd::Link) if dropped_links > 0 => {
                dropped_links -= 1;
                None
            }
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            }) => match readable_destination(&dest_url, base.as_ref()) {
                Some(dest_url) => Some(Event::Start(Tag::Image {
                    link_type,
                    dest_url,
                    title,
                    id,
                })),
                None => {
                    dropped_images += 1;
                    None
                }
            },
            Event::End(TagEnd::Image) if dropped_images > 0 => {
                dropped_images -= 1;
                None
            }
            other => Some(other),
        }
    });

    let mut markup = String::new();
    html::push_html(&mut markup, events);
    Rendered { markup, text_chars }
}

/// A destination worth keeping, spelled absolutely, or nothing when the link has to go.
///
/// A dropped link loses the link and keeps its text, which is what the HTML path does with the
/// one scheme its library happens to catch. Resolving against the address the document was
/// served from is the other half: the HTML path stores absolute destinations, and export
/// matches notes to each other by comparing them.
fn readable_destination<'a>(destination: &str, base: Option<&Url>) -> Option<CowStr<'a>> {
    let destination = destination.trim();
    // A fragment addresses this document, so there is nothing to resolve and nothing a scheme
    // could hide in. The HTML path leaves them alone too.
    if destination.starts_with('#') {
        return Some(CowStr::from(destination.to_owned()));
    }
    let absolute = match Url::parse(destination) {
        Ok(absolute) => absolute,
        // A relative destination carries no scheme to judge, so it is resolved and judged after.
        // Anything the parser refuses outright is a destination no reader could follow either.
        Err(url::ParseError::RelativeUrlWithoutBase) => base?.join(destination).ok()?,
        Err(_) => return None,
    };
    READABLE_SCHEMES
        .contains(&absolute.scheme())
        .then(|| CowStr::from(absolute.to_string()))
}

/// Which CommonMark extensions are read, and why only these two.
///
/// Each is here because of what the document looks like without it, not because of what it adds.
/// Tables, because the converter writes a table back out as a table, while a pipe table left
/// unparsed collapses into one mangled paragraph. YAML metadata blocks, because a document
/// opening with `---` and no extension to read it parses as a horizontal rule followed by a
/// setext heading, so its front matter becomes a heading the document never had.
///
/// Strikethrough is the shape of the ones left out. The converter has no Markdown for `<del>`,
/// so reading `~~gone~~` loses the marks, where leaving it unread keeps the characters standing
/// as the text they already were. An extension is worth enabling only when parsing it preserves
/// more than not parsing it does.
fn extensions() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn served(document: &str) -> Extraction {
        read(document, "https://example.com/posts/one").expect("a document this test wrote")
    }

    fn article_of(document: &str) -> Article {
        match served(document) {
            Extraction::Article(article) => article,
            other => panic!("expected an article, got {other:?}"),
        }
    }

    #[test]
    fn a_served_document_becomes_an_article_that_says_it_was_not_extracted() {
        let article = article_of("# The oven is fixed\n\nThe element went in this morning.\n");

        assert_eq!(article.record.rules, ExtractionRules::Served);
        assert_eq!(article.record.extractor_version, EXTRACTOR_VERSION);
        assert!(article.markdown.starts_with("# The oven is fixed"));
        assert!(
            article
                .markdown
                .contains("The element went in this morning")
        );
        // Nothing scored anything, so there is no page description and no byline to report.
        assert_eq!(article.record.excerpt, None);
        assert_eq!(article.record.byline, None);
    }

    /// The document is the page, so the two counts are equal and the sliver rule cannot fire.
    /// A note of a few words published as its own document is exactly as much an article as a
    /// long one, and the floor that guesses at that from a page's shape has nothing to guess
    /// about here.
    #[test]
    fn a_served_document_is_the_whole_page_it_is_measured_against() {
        let article = article_of("Short.\n");

        let share = article
            .record
            .share
            .expect("a served record measures its share");
        assert_eq!(share.article_chars, share.page_chars);
        assert_eq!(share.article_chars, "Short.".len());
    }

    /// The one thing a Markdown document can carry that an extracted article never can. Every
    /// renderer passes raw HTML through, so a served page would otherwise put markup into the
    /// archive that the HTML path spends a whole converter removing.
    #[test]
    fn raw_markup_in_a_served_document_survives_as_text_and_not_as_markup() {
        let article = article_of(
            "# Post\n\n<script>alert(1)</script>\n\nInline <img src=x onerror=alert(1)> here.\n",
        );

        // Not "the tag is absent": it is present as text, so the document still says what it
        // said. What matters is that no `<` in it is left standing as the start of markup.
        assert!(article.markdown.contains("script"), "{}", article.markdown);
        assert!(
            article
                .markdown
                .match_indices('<')
                .all(|(at, _)| article.markdown.as_bytes()[at.wrapping_sub(1)] == b'\\'),
            "{}",
            article.markdown
        );
    }

    /// A fenced block is already inert to every renderer, and it is often the point of the
    /// document. Escaping inside one would corrupt the code it holds without buying anything.
    #[test]
    fn markup_inside_a_fenced_block_is_left_as_the_code_it_is() {
        let article = article_of("# Post\n\n```html\n<script>in a fence</script>\n```\n");

        assert!(
            article.markdown.contains("<script>in a fence</script>"),
            "{}",
            article.markdown
        );
    }

    /// The test the next proposed extension has to pass. Reading one costs whatever the
    /// converter cannot write back out: strikethrough parses to `<del>`, which has no Markdown,
    /// so enabling it would delete the marks, while leaving it unread keeps the characters
    /// standing as the text they already were.
    #[test]
    fn an_extension_the_converter_cannot_write_back_is_left_unread() {
        let article = article_of("# Post\n\nThe oven is ~~broken~~ fixed, and this is prose.\n");

        assert!(
            article.markdown.contains("~~broken~~"),
            "the marks were read and then lost: {}",
            article.markdown
        );
    }

    /// The destination is the one thing in a served document a reader may act on, and it is
    /// written by whoever wrote the document. The link goes and the text stays, which is what
    /// the HTML path does with the single scheme its library catches.
    #[test]
    fn a_destination_that_exists_to_run_loses_its_link_and_keeps_its_text() {
        for destination in [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            "  javascript:alert(1)",
            "vbscript:msgbox(1)",
            "data:text/html,<script>alert(1)</script>",
        ] {
            let article = article_of(&format!("Start here [click]({destination}) now.\n"));

            assert!(
                article.markdown.contains("click"),
                "the text went with the link for {destination}: {}",
                article.markdown
            );
            assert!(
                !article.markdown.contains("]("),
                "{destination} survived as a link: {}",
                article.markdown
            );
        }
    }

    #[test]
    fn an_image_pointing_at_something_that_runs_loses_its_link_too() {
        let article = article_of(
            "# Post\n\n![a picture](javascript:alert(1))\n\n![real](/pictures/one.png)\n",
        );

        assert!(
            !article.markdown.contains("javascript"),
            "{}",
            article.markdown
        );
        assert!(
            article
                .markdown
                .contains("https://example.com/pictures/one.png"),
            "{}",
            article.markdown
        );
    }

    /// Resolved against the address the document was served from, which is what the HTML path
    /// stores and what export compares notes by.
    #[test]
    fn a_relative_destination_is_spelled_out_against_the_address_it_came_from() {
        let article = article_of(
            "Read [the other one](../two) and [mail](mailto:someone@example.com) \
             and [back](#top).\n",
        );

        assert!(
            article.markdown.contains("https://example.com/two"),
            "{}",
            article.markdown
        );
        assert!(article.markdown.contains("mailto:someone@example.com"));
        assert!(article.markdown.contains("(#top)"));
    }

    /// Front matter is the document's metadata, not its prose. Without the extension that reads
    /// it, `---` opens a horizontal rule and closes a setext heading, so the archived article
    /// would carry a heading nobody wrote.
    #[test]
    fn front_matter_does_not_become_a_heading_the_document_never_had() {
        let article =
            article_of("---\ntitle: A post\nauthor: someone\n---\n\n# A post\n\nProse.\n");

        assert_eq!(
            article
                .markdown
                .lines()
                .filter(|line| line.starts_with('#'))
                .collect::<Vec<_>>(),
            ["# A post"],
            "{}",
            article.markdown
        );
        assert!(!article.markdown.contains("author"), "{}", article.markdown);
    }

    /// The extension that earns its place by what is lost without it. A pipe table read as
    /// paragraph text collapses into one line and stops being a table at all.
    #[test]
    fn a_table_survives_as_a_table() {
        let article = article_of("# Post\n\n| a | b |\n|---|---|\n| 1 | 2 |\n");

        assert!(
            article.markdown.contains("| a | b |"),
            "{}",
            article.markdown
        );
        assert!(
            article.markdown.contains("| 1 | 2 |"),
            "{}",
            article.markdown
        );
    }

    /// A response that is Markdown and holds no prose is a page the extractor read and
    /// declined, which is what keeps a later pass from converting the same nothing again.
    #[test]
    fn a_served_document_with_no_prose_in_it_is_marked_rather_than_stored() {
        for empty in ["", "   \n\n", "---\ntitle: only front matter\n---\n"] {
            assert_eq!(
                served(empty),
                Extraction::NotArticle(NonArticle {
                    extractor_version: EXTRACTOR_VERSION,
                    rules: ExtractionRules::Served,
                }),
                "for {empty:?}"
            );
        }
    }

    /// The outer ceiling, which is the only thing bounding how much work the conversion below
    /// can be asked to do. It is the same number and the same wording the HTML path refuses
    /// with, so a run report reads the same whichever path turned the page away.
    #[test]
    fn a_served_document_over_the_byte_ceiling_is_refused_before_it_is_converted() {
        let document = "a".repeat(MAX_DOCUMENT_BYTES + 1);

        let refused = read(&document, "https://example.com/big").expect_err("refused");
        assert!(refused.contains("byte ceiling"), "{refused}");
    }

    /// The ceiling the byte one does not cover. Every character opens a blockquote, so a
    /// document far under the byte ceiling generates markup nested deeply enough that the
    /// converter's own parser is what pays, which is the cost this scan exists to see first.
    #[test]
    fn a_document_that_opens_an_element_per_character_is_refused() {
        let document = format!("{}text\n", ">".repeat(MAX_OPEN_ELEMENTS + 10));

        let refused = read(&document, "https://example.com/deep").expect_err("refused");
        assert!(refused.contains("elements open at once"), "{refused}");
    }

    /// The document a real capture found: a post published as Markdown beside its HTML.
    #[test]
    fn an_ordinary_published_post_comes_through_whole() {
        let article = article_of(
            "# How to bake bread\n\nBread is mostly *patience*.\n\n\
             ## The method\n\n- flour\n- water\n\n> Wait for the dough.\n\n\
             ```\noven: 250C\n```\n",
        );

        assert!(article.markdown.contains("## The method"));
        assert!(article.markdown.contains("Bread is mostly *patience*"));
        assert!(article.markdown.contains("flour"));
        assert!(article.markdown.contains("> Wait for the dough."));
        assert!(article.markdown.contains("```\noven: 250C\n```"));
        assert!(article.record.word_count > 0);
    }
}
