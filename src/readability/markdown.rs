//! Turning the markup the scorer kept into the document that gets written.
//!
//! The one file that knows which HTML-to-Markdown converter this project uses. Nothing here
//! decides what the article is; it has already been decided by the time this runs.

use super::model::ArticleBound;

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
    truncated: &mut Vec<ArticleBound>,
) -> Result<Prose, String> {
    let body = htmd::convert(article_html)
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
fn floor_char_boundary(text: &str, at: usize) -> usize {
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
        render(article_html, title, &mut Vec::new()).expect("converts")
    }

    #[test]
    fn the_title_becomes_the_documents_heading() {
        let mut truncated = Vec::new();
        let prose = render(
            "<p>Bread is patience.</p>",
            Some("  How to bake  "),
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
