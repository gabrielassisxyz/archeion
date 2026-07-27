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
) -> Result<String, String> {
    let body = htmd::convert(article_html).map_err(|error| error.to_string())?;
    let mut markdown = match title {
        Some(title) => format!("# {}\n\n{}", title.trim(), body.trim_start()),
        None => body,
    };
    if markdown.len() > MAX_MARKDOWN_BYTES {
        markdown.truncate(floor_char_boundary(&markdown, MAX_MARKDOWN_BYTES));
        truncated.push(ArticleBound::Markdown);
    }
    Ok(markdown.trim().to_owned())
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

    #[test]
    fn the_title_becomes_the_documents_heading() {
        let mut truncated = Vec::new();
        let markdown = render(
            "<p>Bread is patience.</p>",
            Some("  How to bake  "),
            &mut truncated,
        )
        .expect("converts");
        assert_eq!(markdown, "# How to bake\n\nBread is patience.");
        assert!(truncated.is_empty());
    }

    /// A capture whose metadata found no title produces a document with no heading, which is
    /// honest. Inventing one from the markup is what the comment on `render` argues against.
    #[test]
    fn a_page_with_no_resolved_title_gets_no_heading() {
        let mut truncated = Vec::new();
        let markdown = render("<p>Bread is patience.</p>", None, &mut truncated).expect("converts");
        assert_eq!(markdown, "Bread is patience.");
    }

    /// The characters that are operators in Markdown arrive from a page nobody here wrote,
    /// and an unescaped one turns prose into markup. This is the reason the conversion is a
    /// dependency rather than a loop over the tree.
    #[test]
    fn prose_that_would_read_as_markup_is_escaped() {
        let mut truncated = Vec::new();
        let markdown = render(
            "<p>It costs $5* per loaf, with _underscores_ and [brackets].</p>",
            None,
            &mut truncated,
        )
        .expect("converts");
        assert_eq!(
            markdown,
            r"It costs $5\* per loaf, with \_underscores\_ and \[brackets\]."
        );
    }

    #[test]
    fn structure_survives_the_conversion() {
        let mut truncated = Vec::new();
        let markdown = render(
            "<h2>Ingredients</h2><ul><li>flour</li><li>water</li></ul>\
             <pre><code>oven: 250C</code></pre><blockquote><p>Patience.</p></blockquote>",
            None,
            &mut truncated,
        )
        .expect("converts");

        assert!(markdown.contains("## Ingredients"));
        assert!(markdown.contains("flour"));
        assert!(markdown.contains("```\noven: 250C\n```"));
        assert!(markdown.contains("> Patience."));
    }

    #[test]
    fn an_article_over_the_ceiling_is_cut_and_says_so() {
        let mut truncated = Vec::new();
        let markdown = render(
            &format!("<p>{}</p>", "word ".repeat(MAX_MARKDOWN_BYTES / 2)),
            None,
            &mut truncated,
        )
        .expect("converts");

        assert!(markdown.len() <= MAX_MARKDOWN_BYTES);
        assert_eq!(truncated, [ArticleBound::Markdown]);
    }

    /// Cutting a multi-byte character in half panics, and the character sitting on the
    /// ceiling belongs to a page this project did not write.
    #[test]
    fn cutting_lands_on_a_character_and_not_inside_one() {
        let mut truncated = Vec::new();
        let markdown = render(
            &format!("<p>{}</p>", "é".repeat(MAX_MARKDOWN_BYTES)),
            None,
            &mut truncated,
        )
        .expect("converts");

        assert!(markdown.len() <= MAX_MARKDOWN_BYTES);
        assert_eq!(truncated, [ArticleBound::Markdown]);
    }

    #[test]
    fn words_are_counted_across_the_whitespace_that_separates_them() {
        assert_eq!(word_count("one two\nthree\tfour  five"), 5);
        assert_eq!(word_count(""), 0);
    }
}
