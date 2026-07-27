//! Separating the prose of a captured page from the furniture around it.
//!
//! What this produces is derived: the response body in the content store stays authoritative,
//! and both files written here can be deleted and rebuilt from it without fetching anything.
//! The reasoning, the ceilings and what was left out are in `docs/readability.md`.
//!
//! An archived page is hostile input forever, not only while it is being fetched. The scoring
//! pass is cubic in nesting depth, so the guard that refuses a document too deep to score is
//! load-bearing rather than defensive: see `document.rs`.

mod document;
mod markdown;
mod markup_scan;
mod model;

use dom_smoothie::{Config, Readability, ReadabilityError};

pub use model::{
    AdmissionCost, Article, ArticleBound, ArticleRecord, EXTRACTOR_VERSION, ExtractionRules,
};

use crate::metadata::PageSource;

/// How many elements the scoring pass may look at.
///
/// This bounds a wide document, where the depth ceiling bounds a deep one. Neither substitutes
/// for the other: a page can have fifty thousand siblings and no nesting at all.
const MAX_ELEMENTS_TO_SCORE: usize = 50_000;

/// A page whose prose could not be read. It names the URL because the point of reporting it
/// is to go and look at the stored body, and a count would leave nothing to look at.
///
/// A page that simply is not an article does not produce this. Most of the web is not an
/// article, and reporting a listing page as a failure would bury the pages worth looking at.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{url} could not be read as an article: {reason}")]
pub struct UnreadableArticle {
    pub url: String,
    pub reason: String,
}

/// Extracts the prose of a captured page.
///
/// `Ok(None)` means there was no article to extract, which is the ordinary answer for most of
/// the web: a listing, a homepage, a shop, the shell of an application that renders itself in
/// the browser, and every capture that is not HTML at all.
///
/// `title` comes from the metadata record rather than from this page's markup, for the reason
/// on `markdown::render`.
pub fn extract(
    source: PageSource<'_>,
    title: Option<&str>,
) -> Result<Option<Article>, UnreadableArticle> {
    let Some(html) = crate::metadata::decoded_html(source) else {
        return Ok(None);
    };
    let refused = |reason: String| UnreadableArticle {
        url: source.final_url.to_owned(),
        reason,
    };

    let (document, measured) = document::build(&html).map_err(|cost| refused(cost.reason()))?;
    let mut readability = Readability::with_document(
        document,
        Some(source.final_url),
        Some(Config {
            max_elements_to_parse: MAX_ELEMENTS_TO_SCORE,
            ..Config::default()
        }),
    )
    .map_err(|error| refused(error.to_string()))?;

    // Before `parse`, not after. The probe is cheap and the scoring pass is not, and this is
    // what keeps the archive from filling up with empty records for pages that are navigation.
    if !readability.is_probably_readable() {
        return Ok(None);
    }
    let article = match readability.parse() {
        Ok(article) => article,
        // The scorer found nothing to keep. That is the same answer as the probe above,
        // reached one step later, and not a page anyone needs to go and look at.
        Err(ReadabilityError::GrabFailed) => return Ok(None),
        Err(error) => return Err(refused(error.to_string())),
    };

    let mut truncated = Vec::new();
    let prose = markdown::render(&article.content, title, &mut truncated).map_err(&refused)?;
    Ok(Some(Article {
        record: ArticleRecord {
            extractor_version: EXTRACTOR_VERSION,
            rules: ExtractionRules::Heuristic,
            // Counted on the prose alone. The heading is a title handed in from the metadata
            // record, so counting it here would report the same words twice across two files.
            word_count: markdown::word_count(&prose.body),
            excerpt: non_empty(article.excerpt.as_deref()),
            byline: non_empty(article.byline.as_deref()),
            truncated,
            cost: AdmissionCost {
                document_bytes: measured.byte_len,
                peak_open_elements: measured.peak_open_elements,
            },
        },
        markdown: prose.document,
    }))
}

/// A field the algorithm reports as present but blank is absent, since a record saying a page
/// has an empty byline claims something the page never said.
fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Enough prose that the scorer treats it as an article, which is the point: every test
    /// here that expects an article needs a page that is really one.
    fn article_page(body: &str) -> String {
        format!(
            "<html><head><title>t</title></head><body><article>{body}{}</article></body></html>",
            "<p>Bread is mostly patience, and the dough will tell you when it is ready.</p>"
                .repeat(8)
        )
    }

    fn extract_html(html: &str, title: Option<&str>) -> Option<Article> {
        extract(
            PageSource {
                body: html.as_bytes(),
                content_type: Some("text/html; charset=utf-8"),
                final_url: "https://example.com/posts/one",
            },
            title,
        )
        .expect("a page this test wrote is readable")
    }

    #[test]
    fn a_capture_with_no_markup_yields_no_article() {
        for content_type in [Some("image/png"), Some("application/pdf"), None, Some("")] {
            let extracted = extract(
                PageSource {
                    body: b"<article><p>not a page</p></article>",
                    content_type,
                    final_url: "https://example.com/logo.png",
                },
                None,
            );
            assert_eq!(extracted, Ok(None), "for {content_type:?}");
        }
    }

    #[test]
    fn an_article_keeps_its_prose_and_drops_the_furniture() {
        let article = extract_html(
            &format!(
                "<html><body><nav><a href=\"/\">Home</a></nav>\
                 <div id=\"cookie\">Accept all cookies</div>\
                 <article><h1>How to bake bread</h1>{}\
                 <h2>The method</h2><p>Mix everything, then wait for the dough.</p></article>\
                 <aside>Most read this week</aside><footer>Subscribe to our newsletter</footer>\
                 </body></html>",
                "<p>Bread is mostly patience, and the dough will tell you when it is ready.</p>"
                    .repeat(8)
            ),
            Some("How to bake bread"),
        )
        .expect("an article");

        assert!(article.markdown.starts_with("# How to bake bread\n"));
        assert!(article.markdown.contains("Bread is mostly patience"));
        assert!(article.markdown.contains("## The method"));
        for furniture in [
            "Accept all cookies",
            "Most read this week",
            "Subscribe to our newsletter",
        ] {
            assert!(
                !article.markdown.contains(furniture),
                "{furniture} survived into the article"
            );
        }
    }

    /// Most of the web. Writing an empty record for each of these would fill the archive with
    /// files that say nothing, which is why absence is the answer rather than an empty article.
    #[test]
    fn a_page_that_is_not_an_article_produces_nothing() {
        let listing = "<html><body><h1>Recipes</h1><ul>\
             <li><a href=\"/a\">Ten pasta shapes</a></li><li><a href=\"/b\">Bread</a></li>\
             <li><a href=\"/c\">Soup</a></li><li><a href=\"/d\">Cake</a></li></ul></body></html>";
        let spa_shell =
            "<html><body><div id=\"root\"></div><script src=\"/app.js\"></script></body></html>";

        assert_eq!(extract_html(listing, Some("Recipes")), None);
        assert_eq!(extract_html(spa_shell, None), None);
    }

    #[test]
    fn the_record_carries_what_produced_it() {
        let article = extract_html(&article_page(""), Some("t")).expect("an article");

        assert_eq!(article.record.extractor_version, EXTRACTOR_VERSION);
        assert_eq!(article.record.rules, ExtractionRules::Heuristic);
        assert!(article.record.word_count > 0);
        assert!(article.record.truncated.is_empty());
    }

    #[test]
    fn the_byline_is_the_one_the_page_carried() {
        let article = extract_html(
            &article_page("<p class=\"byline\">By J. Writer</p>"),
            Some("t"),
        )
        .expect("an article");
        assert_eq!(article.record.byline.as_deref(), Some("By J. Writer"));

        let anonymous = extract_html(&article_page(""), Some("t")).expect("an article");
        assert_eq!(anonymous.record.byline, None);
    }

    /// The guard on the parse, reached through the public entry point rather than only in
    /// `document.rs`, because what matters is that a run keeps going and says which page it
    /// was rather than spending minutes of CPU on one document.
    #[test]
    fn a_page_built_to_be_expensive_is_refused_and_named() {
        let deep = format!(
            "{}<p>buried</p>{}",
            "<div>".repeat(5_000),
            "</div>".repeat(5_000)
        );
        let refused = extract(
            PageSource {
                body: deep.as_bytes(),
                content_type: Some("text/html"),
                final_url: "https://example.com/deep",
            },
            None,
        )
        .expect_err("refused");

        assert_eq!(refused.url, "https://example.com/deep");
        assert!(
            refused.reason.contains("elements open at once"),
            "{refused}"
        );
    }

    #[test]
    fn a_page_in_a_legacy_encoding_keeps_its_prose_readable() {
        let page =
            article_page("<p>Un caf\u{e9} et du pain, voil\u{e0} le petit d\u{e9}jeuner.</p>");
        let (windows_1252, _, _) = encoding_rs::WINDOWS_1252.encode(&page);

        let article = extract(
            PageSource {
                body: &windows_1252,
                content_type: Some("text/html; charset=windows-1252"),
                final_url: "https://example.com/",
            },
            None,
        )
        .expect("readable")
        .expect("an article");

        assert!(article.markdown.contains("café"), "{}", article.markdown);
    }
}
