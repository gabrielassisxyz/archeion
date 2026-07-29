//! Turning the markup the scorer kept into the document that gets written.
//!
//! The one file that knows which HTML-to-Markdown converter this project uses. Nothing here
//! decides what the article is; it has already been decided by the time this runs.

use htmd::{
    Element, HtmlToMarkdown,
    element_handler::{HandlerResult, Handlers},
};
use url::Url;

use super::model::ArticleBound;
use super::readable_markdown::{only_a_description, readable_destination};

/// How much Markdown one article may contribute.
///
/// A long-form piece runs to tens of kilobytes, so this is generous by more than an order of
/// magnitude and reaching it says something about the page rather than about the limit.
pub(super) const MAX_MARKDOWN_BYTES: usize = 1024 * 1024;

/// An article as it is written out: the whole document, and the prose on its own.
///
/// Both, because they answer different questions. The document is the file; the body is what
/// the page actually said, which is what a word count is a count of.
pub(super) struct Prose {
    pub(super) document: String,
    pub(super) body: String,
}

/// Renders an article as a standalone Markdown document.
///
/// The title is passed in rather than taken from the markup. The scoring algorithm removes
/// the article's own heading from the content, because in its model the title is metadata,
/// and the title it derives alongside is the raw `<title>` with the site name still on it.
/// Metadata extraction already resolves that across OpenGraph, schema.org and the document,
/// with precedence rules that are written down, so this takes the answer from there instead
/// of forming a second opinion about what the page is called.
pub(super) fn render(
    article_html: &str,
    title: Option<&str>,
    final_url: Option<&str>,
    truncated: &mut Vec<ArticleBound>,
) -> Result<Prose, String> {
    let body = converter(final_url)
        .convert(article_html)
        .map_err(|error| error.to_string())?
        .trim()
        .to_owned();
    let mut document = match title.and_then(heading) {
        Some(heading) => format!("{heading}\n\n{body}"),
        None => body.clone(),
    };
    if document.len() > MAX_MARKDOWN_BYTES {
        document.truncate(floor_char_boundary(&document, MAX_MARKDOWN_BYTES));
        truncated.push(ArticleBound::Markdown);
    }
    Ok(Prose { document, body })
}

fn converter(final_url: Option<&str>) -> HtmlToMarkdown {
    let base = final_url.and_then(|url| Url::parse(url).ok());
    let anchor_base = base.clone();
    HtmlToMarkdown::builder()
        .add_handler(
            vec!["a"],
            move |handlers: &dyn Handlers, element: Element| {
                anchor(handlers, element, anchor_base.as_ref())
            },
        )
        .add_handler(
            vec!["img"],
            move |_handlers: &dyn Handlers, element: Element| image(element, base.as_ref()),
        )
        .build()
}

fn anchor(handlers: &dyn Handlers, element: Element, base: Option<&Url>) -> Option<HandlerResult> {
    let href = attr(&element, "href");
    let Some(destination) = href
        .as_deref()
        .and_then(|href| readable_destination(href, base))
    else {
        return Some(handlers.walk_children(element.node));
    };
    let content = handlers.walk_children(element.node).content;
    let inline = inline_content(&content);
    if inline.is_empty() {
        return Some(inline_link(&inline, &destination, link_title(&element)).into());
    }
    // Whitespace at either edge moves outside the link rather than being dropped with the rest
    // of the trimming. An anchor padded by the markup around it, which indented HTML produces
    // constantly, otherwise loses the space that separated it from the word before or after,
    // and the stored prose reads with two words run together.
    let leading = if content.starts_with(char::is_whitespace) {
        " "
    } else {
        ""
    };
    let trailing = if content.ends_with(char::is_whitespace) {
        " "
    } else {
        ""
    };
    let link = inline_link(&inline, &destination, link_title(&element));
    Some(format!("{leading}{link}{trailing}").into())
}

/// An anchor's children, reduced to something the inline link spelling can hold.
///
/// A link is written `[text](destination)`, which is inline syntax, and an anchor is allowed to
/// wrap block content: a picture in its own container is the ordinary case and a whole card is
/// the ambitious one. Emitted as it stands, the block's own blank lines end the paragraph the
/// `[` opened, so a reader gets a paragraph holding a bare `[`, then the content, then a
/// paragraph holding `](destination)` as literal characters. The destination is then not a link
/// to anything, which also costs the export its one mechanism for turning a link between two
/// archived pages into a path between two notes.
///
/// Trimming is what the common case needs, since a container around one image leaves the image
/// alone once its surrounding blank lines are gone. Anything still spanning lines after that has
/// no inline spelling at all, so its whitespace is collapsed: the link survives, which loses the
/// block structure inside it and keeps both the text and the destination. Losing the arrangement
/// of something that was one link to begin with is the smaller loss.
///
/// The condition is one line and not the absence of a blank line, which is a weaker test that
/// lets through exactly the constructs that do the most damage. A list is spelled with a single
/// newline between its items, and a list item interrupts a paragraph, so the note gets a bare
/// opening bracket and then a list the page wrote into it. A fenced code block is worse: it can
/// interrupt a paragraph too, and its closing fence lands after the destination, so everything in
/// the note past that link becomes one unterminated code block, taking every later image and
/// cross-note destination out of the export's reach with it.
fn inline_content(content: &str) -> String {
    let trimmed = content.trim();
    if !trimmed.contains('\n') {
        return trimmed.to_owned();
    }
    trimmed.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn image(element: Element, base: Option<&Url>) -> Option<HandlerResult> {
    let destination = image_destination(&element, base);
    let description = attr(&element, "alt")
        .map(|alt| only_a_description(&alt))
        .unwrap_or_default();
    match destination {
        Some(destination) => Some(
            inline_image(
                &description,
                &destination,
                attr(&element, "title").as_deref(),
            )
            .into(),
        ),
        None if description.is_empty() => Some(String::new().into()),
        None => Some(markdown_text(&description).into()),
    }
}

/// The address worth writing into the note for an image, out of everything the element offers.
///
/// A page that lists the same picture at several sizes is offering a choice a browser makes
/// against a viewport, and an archive has none: what it has is a reader who wants the picture,
/// so the widest candidate wins. Taking `src` instead is how a note ends up showing a thumbnail
/// while the archive holds the full size file beside it.
///
/// `srcset` is consulted before `src` for a second reason that is not about quality. The
/// readability layer's own lazy-image repair copies an attribute whose value merely contains an
/// image extension over one of these two, choosing `srcset` when the value also looks like a
/// candidate list and `src` otherwise. A platform describing its pictures with a JSON descriptor
/// in a data attribute lands in the second case: the descriptor names a `.jpeg` inside itself,
/// replaces `src`, and becomes an address resolved against the page's own path that no server
/// answers, while the page's own `srcset` is left alone. Preferring `srcset` steps around that,
/// and does not pretend to survive the case where the repair overwrote `srcset` instead.
///
/// A candidate is taken only when it is already absolute, and the whole preference is abandoned
/// rather than defended when it does not produce a destination the archive keeps. The layer below
/// absolutizes what it can reach, so a candidate still relative here is one it could not, and
/// resolving it against the response address would invent an origin the page never named. And a
/// `srcset` holding nothing usable, an inline placeholder being the ordinary case, must not cost
/// the picture that `src` was pointing at all along.
fn image_destination(element: &Element<'_>, base: Option<&Url>) -> Option<String> {
    if let Some(srcset) = attr(element, "srcset")
        && let Some(candidate) = crate::srcset::widest(&srcset)
        && Url::parse(candidate).is_ok()
        && let Some(destination) = readable_destination(candidate, base)
    {
        return Some(destination);
    }
    attr(element, "src")
        .or_else(|| attr(element, "href"))
        .and_then(|destination| readable_destination(&destination, base))
}

fn attr(element: &Element<'_>, name: &str) -> Option<String> {
    element
        .attrs
        .iter()
        .find(|attr| attr.name.local.as_ref() == name)
        .map(|attr| attr.value.to_string())
}

fn inline_link(content: &str, destination: &str, title: Option<String>) -> String {
    let destination = markdown_destination(destination);
    match title {
        Some(title) => format!("[{content}]({destination} \"{title}\")"),
        None => format!("[{content}]({destination})"),
    }
}

fn inline_image(description: &str, destination: &str, title: Option<&str>) -> String {
    let destination = markdown_destination(destination);
    let title = title
        .map(only_a_description)
        .filter(|title| !title.is_empty())
        .map(|title| format!(" \"{}\"", title.replace('"', "\\\"")))
        .unwrap_or_default();
    format!("![{description}]({destination}{title})")
}

fn markdown_destination(destination: &str) -> String {
    let mut escaped = String::with_capacity(destination.len() + 2);
    let has_spaces = destination.contains(' ');
    if has_spaces {
        escaped.push('<');
    }
    for ch in destination.chars() {
        match ch {
            '(' => escaped.push_str("\\("),
            ')' => escaped.push_str("\\)"),
            _ => escaped.push(ch),
        }
    }
    if has_spaces {
        escaped.push('>');
    }
    escaped
}

fn link_title(element: &Element<'_>) -> Option<String> {
    let title = attr(element, "title")?;
    let title = only_a_description(&title).replace('"', "\\\"");
    (!title.is_empty()).then_some(title)
}

fn markdown_text(text: &str) -> String {
    let as_markup = format!("<span>{}</span>", escape_html_text(text));
    htmd::convert(&as_markup)
        .unwrap_or_else(|_| text.to_owned())
        .trim()
        .to_owned()
}

/// The title as a Markdown heading, or nothing when there is no title left after normalizing.
///
/// The title is the one string in this file that a captured page controls and that does not
/// pass through the converter, and a heading is a line: a newline in it ends the heading and
/// everything after it becomes document structure. A page serving a title with a line break
/// in it, which is legal inside an attribute value, could therefore write its own headings,
/// links and paragraphs into the archived article, indistinguishable from extracted prose.
///
/// So whitespace collapses to single spaces, and the result is escaped by the same converter
/// the body goes through rather than by rules written here. Two escapers would be two sets of
/// rules to keep in agreement, and this one is already the reason the converter is a
/// dependency at all.
fn heading(title: &str) -> Option<String> {
    let collapsed = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    let as_markup = format!("<h1>{}</h1>", escape_html_text(&collapsed));
    htmd::convert(&as_markup)
        .ok()
        .map(|heading| heading.trim().to_owned())
        .filter(|heading| !heading.is_empty())
}

/// The three characters that end text and begin markup. Escaping them is what lets the title
/// be handed to the converter as content rather than as tags it would then interpret.
fn escape_html_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Words, near enough. Splitting on whitespace is wrong for languages that do not use it,
/// which is why the record documents this as a rough figure rather than a measurement.
pub(super) fn word_count(markdown: &str) -> usize {
    markdown.split_whitespace().count()
}

/// The largest index at or below `at` that a `String` may be cut on.
///
/// `str::floor_char_boundary` is still unstable, and truncating a multi-byte character in
/// half panics. The ceiling above is reached by pages this project did not write, so the
/// character sitting on it is not one anybody chose.
pub(super) fn floor_char_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(article_html: &str, title: Option<&str>) -> Prose {
        render(
            article_html,
            title,
            Some("https://example.com/posts/one"),
            &mut Vec::new(),
        )
        .expect("converts")
    }

    #[test]
    fn the_title_becomes_the_documents_heading() {
        let mut truncated = Vec::new();
        let prose = render(
            "<p>Bread is patience.</p>",
            Some("  How to bake  "),
            Some("https://example.com/posts/one"),
            &mut truncated,
        )
        .expect("converts");
        assert_eq!(prose.document, "# How to bake\n\nBread is patience.");
        assert_eq!(prose.body, "Bread is patience.");
        assert!(truncated.is_empty());
    }

    /// A capture whose metadata found no title produces a document with no heading, which is
    /// honest. Inventing one from the markup is what the comment on `render` argues against.
    #[test]
    fn a_page_with_no_resolved_title_gets_no_heading() {
        let prose = rendered("<p>Bread is patience.</p>", None);
        assert_eq!(prose.document, "Bread is patience.");
    }

    /// The title is the one string here a captured page controls that does not pass through
    /// the converter, and a heading is a line. A line break in it, which is legal inside an
    /// attribute value, would end the heading and let the page write its own structure into
    /// the archived article: a section, a link and a paragraph no reader could tell from
    /// extracted prose.
    #[test]
    fn a_title_carrying_line_breaks_cannot_write_the_documents_structure() {
        let prose = rendered(
            "<p>Bread is patience.</p>",
            Some("Bread\n\n## Security notice\n\n[Reset your password](http://evil.example/)"),
        );
        let heading = prose.document.lines().next().expect("a heading");

        // One heading, and it is the one this program wrote. The injected `##` is still in the
        // text, inert, because a heading is only a heading at the start of a line.
        assert_eq!(
            prose
                .document
                .lines()
                .filter(|l| l.starts_with('#'))
                .count(),
            1
        );
        assert!(heading.starts_with("# Bread "), "{heading}");
        assert!(heading.contains(r"\[Reset your password\]"), "{heading}");
        assert!(!heading.contains("[Reset your password]("), "{heading}");
        assert_eq!(prose.document.lines().nth(1), Some(""));
    }

    /// The same string handed to the converter as content rather than as markup, so a title
    /// carrying tags cannot reopen the document as HTML either.
    #[test]
    fn a_title_carrying_markup_is_escaped_like_any_other_text() {
        let prose = rendered(
            "<p>Bread.</p>",
            Some("<img src=x onerror=alert(1)> and *emphasis* and [a link](http://evil.example/)"),
        );

        assert!(
            prose.document.contains(r"\<img src=x"),
            "{}",
            prose.document
        );
        // Not "the tag is absent": it is present as text. What matters is that no `<` in the
        // document is left standing as the start of markup.
        assert!(
            prose
                .document
                .match_indices('<')
                .all(|(at, _)| prose.document.as_bytes()[at.wrapping_sub(1)] == b'\\'),
            "{}",
            prose.document
        );
        assert!(
            prose.document.contains(r"\*emphasis\*"),
            "{}",
            prose.document
        );
        assert!(prose.document.contains(r"\[a link\]"), "{}", prose.document);
    }

    /// A title that is nothing but whitespace is no title, and must not leave a bare `#`
    /// heading standing at the top of the document.
    #[test]
    fn a_title_that_is_only_whitespace_produces_no_heading() {
        for blank in ["", "   ", "\n\n", "\t"] {
            let prose = rendered("<p>Bread.</p>", Some(blank));
            assert_eq!(prose.document, "Bread.", "for {blank:?}");
        }
    }

    /// The count is of the prose, so the title is not counted here and again in the metadata
    /// record beside it.
    #[test]
    fn the_word_count_is_of_the_prose_and_not_of_the_heading() {
        let prose = rendered("<p>one two three</p>", Some("a much longer title here"));
        assert_eq!(word_count(&prose.body), 3);
    }

    /// The characters that are operators in Markdown arrive from a page nobody here wrote,
    /// and an unescaped one turns prose into markup. This is the reason the conversion is a
    /// dependency rather than a loop over the tree.
    #[test]
    fn prose_that_would_read_as_markup_is_escaped() {
        let prose = rendered(
            "<p>It costs $5* per loaf, with _underscores_ and [brackets].</p>",
            None,
        );
        assert_eq!(
            prose.document,
            r"It costs $5\* per loaf, with \_underscores\_ and \[brackets\]."
        );
    }

    #[test]
    fn an_html_image_description_cannot_write_document_structure_or_a_destination() {
        for (src, alt) in [
            (
                "https://example.com/p.png",
                "x&#10;# Injected heading&#10;y",
            ),
            ("https://example.com/p.png", "a](javascript:alert(1))"),
            ("javascript:alert(1)", "# Injected heading"),
        ] {
            let prose = rendered(&format!(r#"<p><img src="{src}" alt="{alt}"></p>"#), None);

            assert!(
                !prose
                    .document
                    .lines()
                    .any(|line| line == "# Injected heading"),
                "{}",
                prose.document
            );
            assert!(
                !prose.document.contains("](javascript"),
                "{}",
                prose.document
            );
        }
    }

    #[test]
    fn an_html_destination_that_exists_to_run_loses_its_link_and_keeps_its_text() {
        for destination in [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            "vbscript:msgbox(1)",
            "data:text/html,<script>alert(1)</script>",
        ] {
            let prose = rendered(
                &format!(r#"<p>Start <a href="{destination}">click</a> now.</p>"#),
                None,
            );

            assert!(
                prose.document.contains("click"),
                "the text went with the link for {destination}: {}",
                prose.document
            );
            assert!(
                !prose.document.contains("]("),
                "{destination} survived as a link: {}",
                prose.document
            );
        }
    }

    #[test]
    fn an_html_readable_destination_survives_under_the_archive_policy() {
        for (destination, expected) in [
            ("#intro", "[click](#intro)"),
            ("../two?a=1#s", "[click](https://example.com/two?a=1#s)"),
            (
                "mailto:reader@example.com",
                "[click](mailto:reader@example.com)",
            ),
        ] {
            let prose = rendered(
                &format!(r#"<p>Start <a href="{destination}">click</a> now.</p>"#),
                None,
            );

            assert!(
                prose.document.contains(expected),
                "{destination} was not kept as {expected}: {}",
                prose.document
            );
        }
    }

    #[test]
    fn an_html_link_title_cannot_write_structure_or_a_destination() {
        let prose = rendered(
            r##"<p><a href="#intro " title="x\&quot; ) [y](javascript:alert(1)">click</a></p>"##,
            None,
        );

        assert!(
            prose.document.contains("[click](#intro "),
            "{}",
            prose.document
        );
        assert!(
            !prose.document.contains("](javascript"),
            "{}",
            prose.document
        );
        assert!(
            !prose.document.lines().any(|line| line.starts_with('#')),
            "{}",
            prose.document
        );
    }

    #[test]
    fn an_html_fragment_with_whitespace_loses_its_link_and_keeps_its_text() {
        let prose = rendered(
            r#"<p><a href="&#10;#top&#10;# Injected heading">click</a></p>"#,
            None,
        );

        assert_eq!(prose.document, "click");
    }

    /// An anchor around a picture in its own container used to produce a paragraph holding a
    /// bare `[`, then the image, then a paragraph holding `](destination)` as literal text. The
    /// destination was then not a link to anything, so the export could not turn it into a path
    /// to the note beside it either.
    #[test]
    fn an_anchor_wrapping_an_image_is_one_inline_link() {
        // Not wrapped in a paragraph: a `<div>` closes an open `<p>`, and the reconstructed
        // tree would then hold an empty anchor beside the real one, which is a fact about the
        // fixture rather than about the conversion.
        let prose = rendered(
            r#"<a href="https://example.com/two"><div><img src="https://example.com/p.png" alt="a loaf"></div></a>"#,
            None,
        );

        assert_eq!(
            prose.document,
            "[![a loaf](https://example.com/p.png)](https://example.com/two)"
        );
    }

    #[test]
    fn an_anchor_wrapping_an_image_and_text_keeps_both_inside_the_link() {
        let prose = rendered(
            r#"<a href="https://example.com/two"><div><img src="https://example.com/p.png" alt="a loaf"></div><div>Read on</div></a>"#,
            None,
        );

        assert_eq!(
            prose.document,
            "[![a loaf](https://example.com/p.png) Read on](https://example.com/two)"
        );
    }

    /// Content that is genuinely several blocks has no inline spelling, so the arrangement is
    /// what gives way. What must not happen is the destination surviving as literal characters:
    /// that is the broken syntax this pair of tests exists to keep out.
    #[test]
    fn an_anchor_wrapping_blocks_keeps_the_link_and_loses_the_arrangement() {
        let prose = rendered(
            r#"<a href="https://example.com/two"><h2>Bread</h2><p>Is patience.</p></a>"#,
            None,
        );

        assert!(!prose.document.contains("\n\n]("), "{}", prose.document);
        assert!(prose.document.contains("Bread"), "{}", prose.document);
        assert!(
            prose.document.contains("Is patience."),
            "{}",
            prose.document
        );
        assert!(
            prose.document.ends_with("](https://example.com/two)"),
            "{}",
            prose.document
        );
    }

    /// A page offering one picture at several widths is offering a choice made against a
    /// viewport the archive does not have. The reader wants the picture, so the widest wins,
    /// wherever the page happened to put it and whatever the small default in `src` says.
    #[test]
    fn an_image_is_written_at_the_widest_size_the_page_offered() {
        let prose = rendered(
            r#"<p><img src="https://example.com/small.png"
                      srcset="https://example.com/small.png 424w, https://example.com/large.png 1456w"
                      alt="a loaf"></p>"#,
            None,
        );

        assert_eq!(
            prose.document, "![a loaf](https://example.com/large.png)",
            "the note should carry the largest rendition the page listed"
        );
    }

    #[test]
    fn an_image_with_no_candidates_keeps_the_address_it_was_given() {
        let prose = rendered(
            r#"<p><img src="https://example.com/only.png" alt="a loaf"></p>"#,
            None,
        );

        assert_eq!(prose.document, "![a loaf](https://example.com/only.png)");
    }

    /// The readability layer's lazy-image repair copies an attribute whose value merely holds an
    /// image extension over `src`, and a platform describing its picture with a JSON descriptor
    /// in a data attribute trips it: what reached the note was that descriptor, resolved against
    /// the page's own path into an address no server answers.
    ///
    /// The repair lives in a dependency and is not what this pins. The descriptor is written into
    /// `src` here directly, so what is asserted is the half this file owns, that a usable
    /// candidate outranks whatever `src` turned out to hold.
    #[test]
    fn an_image_whose_source_was_replaced_by_a_descriptor_is_read_from_its_candidates() {
        let prose = rendered(
            r#"<p><img src="{&quot;src&quot;:&quot;https://cdn.example/one.jpeg&quot;,&quot;width&quot;:1192}"
                      srcset="https://cdn.example/small.jpeg 424w, https://cdn.example/large.jpeg 1456w"
                      alt="a loaf"></p>"#,
            None,
        );

        assert_eq!(prose.document, "![a loaf](https://cdn.example/large.jpeg)");
        assert!(
            !prose.document.contains("%7B%22src%22"),
            "the descriptor reached the note: {}",
            prose.document
        );
    }

    /// Content spanning lines is what breaks the inline spelling, and a blank line is only the
    /// loudest way to span them. A list is joined by single newlines and a list item interrupts a
    /// paragraph, so the weaker test let a page write a list into the note around a bare opening
    /// bracket. A fenced code block is worse: its closing fence lands after the destination and
    /// everything later in the note becomes one unterminated block, which takes every image and
    /// cross-note destination after it out of the export's reach.
    #[test]
    fn an_anchor_wrapping_a_list_or_a_code_block_still_yields_one_line() {
        for markup in [
            r#"<a href="https://example.com/two"><ul><li>one</li><li>two</li></ul></a>"#,
            r#"<a href="https://example.com/two"><pre><code>let x = 1;</code></pre></a>"#,
            r#"<a href="https://example.com/two"><table><tr><td>one</td></tr></table></a>"#,
        ] {
            let prose = rendered(markup, None);

            assert!(
                !prose.document.contains('\n'),
                "the link text still spans lines for {markup}: {}",
                prose.document
            );
            assert!(
                prose.document.ends_with("](https://example.com/two)"),
                "{}",
                prose.document
            );
        }
    }

    /// An anchor padded by the markup around it is what indented HTML produces constantly, and
    /// trimming that padding away rather than moving it outside the link ran the words on either
    /// side of it together in the stored prose.
    #[test]
    fn whitespace_around_an_anchor_survives_outside_the_link() {
        let prose = rendered(
            r#"<p>Read <a href="https://example.com/two">the recipe </a>and bake.</p>"#,
            None,
        );

        assert_eq!(
            prose.document,
            "Read [the recipe](https://example.com/two) and bake."
        );
    }

    /// A `srcset` holding nothing the archive keeps must not cost the picture `src` was pointing
    /// at. An inline placeholder beside a real address is an ordinary lazy-loading spelling, and
    /// the destination policy refuses it for the same reason it refuses one written in `src`.
    #[test]
    fn a_candidate_the_policy_refuses_falls_back_to_the_source_attribute() {
        let prose = rendered(
            r#"<p><img src="https://cdn.example/real.jpg"
                      srcset="data:image/gif;base64,R0lGODlhAQABAAAAACH5BAEKAAEALAAAAAABAAEAAAICTAEAOw=="
                      alt="a loaf"></p>"#,
            None,
        );

        assert_eq!(prose.document, "![a loaf](https://cdn.example/real.jpg)");
    }

    /// The layer below absolutizes what it can reach, so a candidate still relative here is one
    /// it could not, and resolving it against the response address would invent an origin the
    /// page never named. That is worse than falling back to the address it already resolved.
    #[test]
    fn a_relative_candidate_is_left_alone_rather_than_given_the_wrong_origin() {
        let prose = rendered(
            r#"<p><img src="https://cdn.example/assets/photo-800.jpg"
                      srcset="photo-800.jpg 800w,photo-1600.jpg 1600w"
                      alt="a loaf"></p>"#,
            None,
        );

        assert_eq!(
            prose.document,
            "![a loaf](https://cdn.example/assets/photo-800.jpg)"
        );
        assert!(
            !prose.document.contains("example.com/posts"),
            "a candidate was resolved against the response address: {}",
            prose.document
        );
    }

    #[test]
    fn structure_survives_the_conversion() {
        let prose = rendered(
            "<h2>Ingredients</h2><ul><li>flour</li><li>water</li></ul>\
             <pre><code>oven: 250C</code></pre><blockquote><p>Patience.</p></blockquote>",
            None,
        );

        assert!(prose.document.contains("## Ingredients"));
        assert!(prose.document.contains("flour"));
        assert!(prose.document.contains("```\noven: 250C\n```"));
        assert!(prose.document.contains("> Patience."));
    }

    #[test]
    fn an_article_over_the_ceiling_is_cut_and_says_so() {
        let mut truncated = Vec::new();
        let prose = render(
            &format!("<p>{}</p>", "word ".repeat(MAX_MARKDOWN_BYTES / 2)),
            None,
            Some("https://example.com/posts/one"),
            &mut truncated,
        )
        .expect("converts");

        assert!(prose.document.len() <= MAX_MARKDOWN_BYTES);
        assert_eq!(truncated, [ArticleBound::Markdown]);
    }

    /// Cutting a multi-byte character in half panics, and the character sitting on the
    /// ceiling belongs to a page this project did not write.
    ///
    /// The character is three bytes and the document carries a one-byte heading in front of
    /// it, so the ceiling lands strictly inside a character. A two-byte character against an
    /// even ceiling would sit on a boundary already and prove nothing: the guard could be
    /// deleted and the test would still pass.
    #[test]
    fn cutting_lands_on_a_character_and_not_inside_one() {
        let mut truncated = Vec::new();
        let prose = render(
            &format!("<p>xx{}</p>", "€".repeat(MAX_MARKDOWN_BYTES)),
            None,
            Some("https://example.com/posts/one"),
            &mut truncated,
        )
        .expect("converts");

        assert!(!prose.document.is_char_boundary(MAX_MARKDOWN_BYTES));
        assert!(prose.document.len() < MAX_MARKDOWN_BYTES);
        assert_eq!(truncated, [ArticleBound::Markdown]);
    }

    #[test]
    fn words_are_counted_across_the_whitespace_that_separates_them() {
        assert_eq!(word_count("one two\nthree\tfour  five"), 5);
        assert_eq!(word_count(""), 0);
    }
}
