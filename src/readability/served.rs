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

use pulldown_cmark::{CowStr, Event, LinkType, Options, Parser, Tag, TagEnd, html};
use url::Url;

use super::document::{
    MAX_DOCUMENT_BYTES, MAX_NESTING_DEPTH, MAX_OPEN_ELEMENTS, TooExpensive, visible_chars,
};
use super::markdown;
use super::markup_scan::peak_open_elements;
use super::model::{
    AdmissionCost, Article, ArticleRecord, EXTRACTOR_VERSION, Extraction, ExtractionRules,
    NonArticle, ProseShare,
};
use super::readable_markdown::{only_a_description, readable_destination};

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
    // Before anything is built, and reading the document rather than the markup it would
    // produce. A document of nothing but `>` opens a blockquote per level, the converter below
    // walks its tree with one stack frame per level, and two thousand of them end the process
    // rather than the page: a four kilobyte response aborts the run with a stack overflow.
    // Measuring this on the generated markup instead would mean generating it first, and a
    // guard that costs what it is protecting against is not a guard.
    if nests_deeper_than(document, MAX_NESTING_DEPTH) {
        return Err(TooExpensive::Depth.reason());
    }
    let rendered = render(document, final_url);
    // Not the bound, which the depth ceiling above already is: a level of Markdown opens at
    // most two elements, so under that ceiling this cannot reach 2048. It is here because it is
    // what fills `peak_open_elements` on the record, and because the sentence before this one is
    // an assumption about how a document's nesting maps onto the markup it generates. If this
    // ever fires, that mapping is what changed.
    let peak = peak_open_elements(&rendered.markup, MAX_OPEN_ELEMENTS);
    if peak > MAX_OPEN_ELEMENTS {
        return Err(TooExpensive::OpenElements { peak }.reason());
    }

    let mut truncated = Vec::new();
    // No title is handed in. A served document carries its own heading, and the metadata
    // extractor produces nothing for a response that is not markup, so the only title that
    // could be prepended here is one nobody has: what it would add is a second heading above
    // the document's own.
    let prose = markdown::render(&rendered.markup, None, None, &mut truncated)?;
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

/// Whether anything in the document nests deeper than the ceiling.
///
/// It answers the question rather than measuring the depth, so it stops at the first level past
/// the ceiling: the documents this exists to refuse are the ones where reading to the end is
/// itself part of the cost. That is the same shape, and the same ceiling, the HTML path applies
/// to its tree, and it is applied here to the events rather than to a tree because the tree that
/// would be measured is the one that must not be built.
///
/// A level of Markdown is one nested container, so a list level counts twice, once for the list
/// and once for the item. Erring that way is erring toward refusing, which is the direction a
/// guard against a stack overflow has to err in.
fn nests_deeper_than(document: &str, ceiling: usize) -> bool {
    let mut depth = 0usize;
    for event in Parser::new_ext(document, extensions()) {
        match event {
            Event::Start(_) => {
                depth += 1;
                if depth > ceiling {
                    return true;
                }
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    false
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
/// Three things are handled here rather than left to the converter, because by the time markup
/// exists it is too late to tell them apart from markup this program wrote.
fn render(document: &str, final_url: &str) -> Rendered {
    let base = Url::parse(final_url).ok();
    let mut text_chars = 0;
    // A stack and not a count. A dropped start tag has to suppress the end tag that belongs to
    // it and no other, and an image's description may hold another image, so a count would
    // suppress the inner end tag and let the outer one through: what follows the inner image is
    // then swallowed into its description, and prose silently leaves the body of the article.
    let mut links: Vec<bool> = Vec::new();
    let mut inside_front_matter = false;
    let mut image: Option<ImageBeingRead> = None;

    let events = Parser::new_ext(document, extensions()).flat_map(|event| {
        // Front matter is what the document says about itself, not what it says. It is dropped
        // whole rather than merely left out of the markup, because the text inside it would
        // otherwise be counted as article text, and a record reporting a page's own metadata as
        // prose it holds is a record claiming something the document never said.
        match &event {
            Event::Start(Tag::MetadataBlock(_)) => inside_front_matter = true,
            Event::End(TagEnd::MetadataBlock(_)) => {
                inside_front_matter = false;
                return Vec::new();
            }
            _ => {}
        }
        if inside_front_matter {
            return Vec::new();
        }
        // An image's description is read here and handed back as one piece of text, because it
        // is the one string on this path that reaches the archived document without passing
        // through the converter: both converters flatten it into an attribute, and the second
        // writes that attribute into `![...](...)` almost verbatim. Read whole, it can be
        // reduced to something that is only a description before it gets there.
        if let Some(reading) = &mut image {
            let written = reading.read(event);
            if reading.is_closed() {
                image = None;
            }
            return written;
        }
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
            // the same terms the title escaping already set. It is counted above because that
            // is what it becomes: text the article holds and a reader sees.
            Event::Html(raw) | Event::InlineHtml(raw) => vec![Event::Text(raw)],
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => match readable_destination(&dest_url, base.as_ref()) {
                Some(dest_url) => {
                    links.push(true);
                    vec![Event::Start(Tag::Link {
                        link_type,
                        dest_url: CowStr::from(dest_url),
                        title,
                        id,
                    })]
                }
                None => {
                    links.push(false);
                    Vec::new()
                }
            },
            Event::End(TagEnd::Link) => match links.pop() {
                // A stray end tag with no start belongs to nothing this rewrote, so it is passed
                // on exactly as it arrived rather than guessed about.
                Some(false) => Vec::new(),
                _ => vec![Event::End(TagEnd::Link)],
            },
            Event::Start(Tag::Image {
                dest_url, title, ..
            }) => {
                image = Some(ImageBeingRead::new(&dest_url, &title, base.as_ref()));
                Vec::new()
            }
            other => vec![other],
        }
    });

    let mut markup = String::new();
    html::push_html(&mut markup, events);
    Rendered { markup, text_chars }
}

/// An image whose description is being read, and what is known about where it points.
struct ImageBeingRead {
    /// How many image starts are open, so the description ends at the one that opened it. An
    /// image's description may hold another image, which is a CommonMark example and not a
    /// corner case.
    depth: usize,
    destination: Option<CowStr<'static>>,
    /// The image's own title, which reaches the archived document as unescaped as the
    /// description does and is reduced the same way.
    title: String,
    description: String,
}

impl ImageBeingRead {
    fn new(destination: &str, title: &str, base: Option<&Url>) -> Self {
        Self {
            depth: 1,
            destination: readable_destination(destination, base).map(CowStr::from),
            title: only_a_description(title),
            description: String::new(),
        }
    }

    fn is_closed(&self) -> bool {
        self.depth == 0
    }

    /// Swallows one event of the description, or closes it and hands back what to write.
    fn read<'a>(&mut self, event: Event<'a>) -> Vec<Event<'a>> {
        match event {
            Event::Start(Tag::Image { .. }) => {
                self.depth += 1;
                Vec::new()
            }
            Event::End(TagEnd::Image) if self.depth > 1 => {
                self.depth -= 1;
                Vec::new()
            }
            Event::End(TagEnd::Image) => {
                self.depth = 0;
                self.finish()
            }
            Event::Text(text) | Event::Code(text) | Event::Html(text) | Event::InlineHtml(text) => {
                self.description.push_str(&text);
                Vec::new()
            }
            // Everything else is structure inside a description, and both converters flatten a
            // description to plain text regardless. Dropping it here keeps the count above the
            // only thing that decides where the description ends.
            _ => Vec::new(),
        }
    }

    fn finish<'a>(&mut self) -> Vec<Event<'a>> {
        let description = Event::Text(CowStr::from(only_a_description(&self.description)));
        match self.destination.take() {
            // Written back as the image's own description, between the start and the end the
            // converter reads as one thing.
            Some(destination) => vec![
                Event::Start(Tag::Image {
                    link_type: LinkType::Inline,
                    dest_url: destination,
                    title: CowStr::from(std::mem::take(&mut self.title)),
                    id: CowStr::Borrowed(""),
                }),
                description,
                Event::End(TagEnd::Image),
            ],
            // The image goes and its description stays, which is what a dropped link does with
            // its text. As ordinary text it is escaped by the converter like any other prose.
            None => vec![description],
        }
    }
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
    use super::super::markdown::MAX_MARKDOWN_BYTES;
    use super::super::model::ArticleBound;
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
        // Each side against the count, and not against the other. Both fields are filled from
        // one measurement, so asserting they match would hold however wrong that measurement is.
        assert_eq!(share.article_chars, "Short.".len());
        assert_eq!(share.page_chars, "Short.".len());
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
                .all(|(at, _)| at > 0 && article.markdown.as_bytes()[at - 1] == b'\\'),
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

    /// An image's description is the one string on this path that reaches the archived document
    /// without passing through the converter: both converters flatten it into an attribute and
    /// the second writes that attribute into `![...](...)` almost verbatim. So a `]` inside it
    /// ends the description early and hands the rest of the line to a destination nothing
    /// screened, which is the scheme filter defeated by the string it was meant to catch.
    #[test]
    fn an_image_description_cannot_write_a_destination_nobody_screened() {
        // The `\]` is a legal escape, so the description really does hold a closing bracket.
        let article = article_of(r"![a\](javascript:alert(1))](/p.png)");

        // One image, and its destination is the screened one. The rest is inert text inside the
        // description, which is where the document put it.
        assert_eq!(
            article.markdown.matches("](").count(),
            1,
            "{}",
            article.markdown
        );
        assert!(
            article.markdown.ends_with("](https://example.com/p.png)"),
            "{}",
            article.markdown
        );
    }

    /// The same string one step further: a line break in a description ends the line the whole
    /// construct sits on, and everything after it becomes the document's own structure. This is
    /// exactly what collapsing whitespace in the title was written for, reached by another route.
    #[test]
    fn an_image_description_cannot_write_the_documents_structure() {
        // A numeric character reference, which the reader resolves before anything here sees it.
        let article = article_of("![x&#10;# Injected heading&#10;y](/p.png)");

        assert_eq!(article.markdown.lines().count(), 1, "{}", article.markdown);
        assert!(!article.markdown.contains("\n# "), "{}", article.markdown);
    }

    /// An image's description may hold another image, which is a CommonMark example rather than
    /// a corner case. Suppressing a dropped image's end tag by counting would suppress the inner
    /// one instead, and the converter would then keep reading the description past where it
    /// ended, swallowing the prose that follows into an attribute.
    #[test]
    fn an_image_inside_an_image_description_does_not_swallow_the_prose_after_it() {
        let article = article_of("![foo ![bar](/url) baz](javascript:1)");

        assert!(article.markdown.contains("baz"), "{}", article.markdown);
        assert!(
            !article.markdown.contains("javascript"),
            "{}",
            article.markdown
        );

        // The same shape with a destination that stays, so the case is pinned on both sides of
        // the decision rather than only where the outer image is dropped.
        let kept = article_of("![foo ![bar](/url) baz](/outer.png)");
        assert!(kept.markdown.contains("baz"), "{}", kept.markdown);
        assert!(
            kept.markdown.contains("https://example.com/outer.png"),
            "{}",
            kept.markdown
        );
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

    /// Front matter has to leave the count as well as the markup. It is what the document says
    /// about itself, so counting it would have the record report a page's own metadata as prose
    /// the page holds, and the number is the one thing about a served article that a later
    /// calibration reads.
    #[test]
    fn front_matter_is_not_counted_as_text_the_document_said() {
        let prose = "# A post\n\nProse.\n";
        let front = format!("---\ntitle: {}\n---\n\n{prose}", "a".repeat(200));

        assert_eq!(
            article_of(&front).record.share,
            article_of(prose).record.share
        );
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

    /// The ceiling the byte one does not cover, and the one that decides whether the process
    /// survives the document. Every character opens a blockquote, and the converter walks its
    /// tree with a stack frame per level: at two thousand levels, a four kilobyte response
    /// aborted the whole run with a stack overflow rather than being refused as one page.
    ///
    /// Both sides, because a ceiling only tested from above would be satisfied by one that
    /// refuses every document, and the range this admits is the range that has to stay cheap.
    #[test]
    fn a_document_that_nests_deeper_than_the_ceiling_is_refused_before_it_is_converted() {
        let quoted = |levels: usize| format!("{}text\n", "> ".repeat(levels));

        // A blockquote per level plus the paragraph inside the innermost one.
        let admitted = read(&quoted(MAX_NESTING_DEPTH - 1), "https://example.com/deep")
            .expect("under the ceiling");
        let Extraction::Article(article) = admitted else {
            panic!("expected an article, got {admitted:?}");
        };
        assert!(article.markdown.contains("text"), "{}", article.markdown);

        for levels in [MAX_NESTING_DEPTH, 2_000] {
            let refused = read(&quoted(levels), "https://example.com/deep")
                .expect_err("refused at {levels} levels");
            assert!(refused.contains("nests deeper than"), "{refused}");
        }
    }

    /// The ceiling on what gets written, reached from a served document rather than only from an
    /// extracted one. A document can be under the byte ceiling and still convert to more
    /// Markdown than the file is allowed to hold, and the record has to say the file is a
    /// prefix rather than the article.
    #[test]
    fn a_served_document_over_the_markdown_ceiling_is_cut_and_says_so() {
        let document = "word ".repeat(MAX_DOCUMENT_BYTES / 5);

        let article = article_of(&document);

        assert!(article.markdown.len() <= MAX_MARKDOWN_BYTES);
        assert_eq!(article.record.truncated, [ArticleBound::Markdown]);
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
