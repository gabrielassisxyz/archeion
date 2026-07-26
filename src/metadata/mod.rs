//! Reading a captured page for what it says about itself.
//!
//! Everything this produces is derived from bytes the archive already holds, and is stored
//! beside the capture rather than inside it. The reasoning, the precedence between the tags
//! that claim the same field, and the ceilings are in `docs/metadata-extraction.md`.
//!
//! An archived page is hostile input forever, not only while it is being fetched: the parse
//! runs on remote markup at capture time and again on every later pass over the archive, so
//! nothing here trusts a length, a URL or a JSON document because it was already stored.

mod decode;
mod model;
mod resolve;
mod scan;

pub use model::{
    AssetKind, Attributed, Bound, EXTRACTOR_VERSION, MetaTag, MetadataSource, OutboundLink,
    PageMetadata, PublicationDate, ReferencedAsset,
};

/// The media types this extractor reads. Anything else is a capture with no markup to read,
/// which is not a failure and does not produce an empty record.
const HTML_MEDIA_TYPES: [&str; 2] = ["text/html", "application/xhtml+xml"];

/// A page the parser gave up on. It names the URL because the point of reporting it is to
/// go and look at the stored body, and a count would leave nothing to look at.
///
/// Malformed markup does not produce this: the parse is deliberately forgiving and reads a
/// broken page as far as it goes. What is left is the parser's own ceilings, which is why
/// this is plumbed through as a report rather than asserted away: the input is remote, and
/// a parse of remote input that cannot fail is a parse that panics instead.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{url} could not be read as HTML: {reason}")]
pub struct UnreadablePage {
    pub url: String,
    pub reason: String,
}

/// One captured response, as the extractor needs to see it.
#[derive(Debug, Clone, Copy)]
pub struct PageSource<'a> {
    pub body: &'a [u8],
    /// The response's `Content-Type` verbatim, parameters and all. It is taken unsplit
    /// because the media type decides whether there is anything to read and the `charset`
    /// parameter decides how to read it, and both come from this one header.
    pub content_type: Option<&'a str>,
    /// The address the response came from, which is what relative URLs resolve against.
    pub final_url: &'a str,
}

/// Extracts what a captured page says about itself.
///
/// `Ok(None)` means the capture is not a page: an image or a PDF has nothing this reads,
/// and recording an empty result for it would fill the archive with files saying nothing.
pub fn extract(source: PageSource<'_>) -> Result<Option<PageMetadata>, UnreadablePage> {
    let (media_type, charset) = split_content_type(source.content_type);
    if !media_type.is_some_and(|media_type| HTML_MEDIA_TYPES.contains(&media_type.as_str())) {
        return Ok(None);
    }

    let html = decode::decode_html(source.body, charset.as_deref());
    let scanned = scan::scan(&html).map_err(|reason| UnreadablePage {
        url: source.final_url.to_owned(),
        reason: reason.to_string(),
    })?;
    Ok(Some(resolve::resolve(scanned, source.final_url)))
}

/// Splits `text/html; charset=utf-8` into the media type and the charset, both lowercased.
fn split_content_type(content_type: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(content_type) = content_type else {
        return (None, None);
    };
    let mut parts = content_type.split(';');
    let media_type = parts
        .next()
        .map(|media_type| media_type.trim().to_ascii_lowercase())
        .filter(|media_type| !media_type.is_empty());
    let charset = parts.find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches('"').to_ascii_lowercase())
            .filter(|charset| !charset.is_empty())
    });
    (media_type, charset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract_html(html: &str) -> PageMetadata {
        extract(PageSource {
            body: html.as_bytes(),
            content_type: Some("text/html; charset=utf-8"),
            final_url: "https://example.com/posts/one",
        })
        .expect("a page this test wrote is readable")
        .expect("html is extracted")
    }

    fn value_of(field: Option<Attributed>) -> Option<(String, MetadataSource)> {
        field.map(|found| (found.value, found.source))
    }

    #[test]
    fn a_capture_with_no_markup_yields_no_record() {
        for content_type in [Some("image/png"), Some("application/pdf"), None, Some("")] {
            let extracted = extract(PageSource {
                body: b"<title>not a page</title>",
                content_type,
                final_url: "https://example.com/logo.png",
            });
            assert_eq!(extracted, Ok(None), "for {content_type:?}");
        }
    }

    #[test]
    fn the_media_type_is_read_past_its_parameters_and_its_case() {
        for content_type in [
            "TEXT/HTML",
            " text/html ; charset=UTF-8",
            "application/xhtml+xml",
        ] {
            let extracted = extract(PageSource {
                body: b"<html><head><title>a page</title></head></html>",
                content_type: Some(content_type),
                final_url: "https://example.com/",
            })
            .expect("readable");
            assert!(extracted.is_some(), "for {content_type}");
        }
    }

    #[test]
    fn the_document_title_is_read_when_nothing_richer_claims_it() {
        let page = extract_html("<html><head><title>  A page  </title></head><body></body></html>");
        assert_eq!(
            value_of(page.title),
            Some(("A page".to_owned(), MetadataSource::Html))
        );
    }

    #[test]
    fn opengraph_wins_over_the_document_title_and_schema_org_over_neither() {
        let page = extract_html(
            r#"<html><head>
                <title>the tab</title>
                <meta property="og:title" content="the share card">
                <meta name="twitter:title" content="the tweet">
                <script type="application/ld+json">{"headline":"the article"}</script>
            </head></html>"#,
        );
        assert_eq!(
            value_of(page.title),
            Some(("the share card".to_owned(), MetadataSource::OpenGraph))
        );

        let without_opengraph = extract_html(
            r#"<html><head>
                <title>the tab</title>
                <script type="application/ld+json">{"headline":"the article"}</script>
            </head></html>"#,
        );
        assert_eq!(
            value_of(without_opengraph.title),
            Some(("the article".to_owned(), MetadataSource::SchemaOrg))
        );
    }

    /// The parser matches a token stream and not a tree, so no element is ever implied. A
    /// selector written as `head > title` compiles, reads every page that spells its `<head>`
    /// out, and silently finds nothing on the many that leave it to the parser.
    #[test]
    fn a_page_that_leaves_out_the_tags_a_parser_would_imply_is_still_read() {
        let page = extract_html("<title>a page</title><a href=\"/one\">one</a>");
        assert_eq!(
            value_of(page.title),
            Some(("a page".to_owned(), MetadataSource::Html))
        );
        assert_eq!(page.links.len(), 1);
    }

    /// The graphic comes first on purpose. A logo sits in the header of most pages, so the
    /// icon's accessible name is what document order offers before the page's own title,
    /// and a test that puts the real title first proves only that first-wins works.
    #[test]
    fn a_title_inside_an_inline_svg_is_not_the_page_title() {
        let page = extract_html(
            "<body><header><svg><title>an icon</title></svg></header>\
             <title>the page</title></body>",
        );
        assert_eq!(
            value_of(page.title),
            Some(("the page".to_owned(), MetadataSource::Html))
        );
    }

    /// The case the rule above exists for: a page that has no title of its own, and an
    /// inline graphic that does. Nothing is better than the name of a logo.
    #[test]
    fn a_page_with_only_a_graphic_title_has_no_title() {
        let page = extract_html(
            r#"<head><meta name="description" content="An article about bread."></head>
               <body><header><svg><title>Recipes Weekly logo</title></svg></header>
               <h1>How to bake bread</h1></body>"#,
        );
        assert_eq!(value_of(page.title), None);
    }

    #[test]
    fn the_author_prefers_the_form_that_names_a_person() {
        let page = extract_html(
            r#"<html><head>
                <meta name="author" content="editorial-cms-user-3312">
                <meta property="article:author" content="https://example.com/authors/j">
                <script type="application/ld+json">
                    {"author":{"@type":"Person","name":"J. Writer"}}
                </script>
            </head></html>"#,
        );
        assert_eq!(
            value_of(page.author),
            Some(("J. Writer".to_owned(), MetadataSource::SchemaOrg))
        );
    }

    #[test]
    fn an_author_is_read_from_every_shape_schema_org_allows() {
        for written in [
            r#""J. Writer""#,
            r#"{"@type":"Person","name":"J. Writer"}"#,
            r#"[{"@type":"Person","name":"J. Writer"},{"name":"second"}]"#,
        ] {
            let page = extract_html(&format!(
                r#"<script type="application/ld+json">{{"author":{written}}}</script>"#
            ));
            assert_eq!(
                value_of(page.author),
                Some(("J. Writer".to_owned(), MetadataSource::SchemaOrg)),
                "for {written}"
            );
        }
    }

    /// The shape most sites emit: a graph whose first node describes the site. Every node
    /// in it answers to `name`, so reading the first one that has the field archives the
    /// name of the site, of a breadcrumb trail or of the author as the page's title.
    #[test]
    fn the_page_is_read_from_its_own_node_and_not_from_the_site_around_it() {
        let page = extract_html(
            r#"<title>How to bake bread</title>
               <script type="application/ld+json">{"@graph":[
                 {"@type":"WebSite","name":"Recipes Weekly"},
                 {"@type":"BreadcrumbList","name":"Breadcrumbs"},
                 {"@type":"Person","name":"J. Writer"},
                 {"@type":"Article","name":"How to bake bread, properly"}]}</script>"#,
        );
        assert_eq!(
            value_of(page.title),
            Some((
                "How to bake bread, properly".to_owned(),
                MetadataSource::SchemaOrg
            ))
        );
    }

    #[test]
    fn a_page_whose_only_structured_data_describes_the_site_still_reads_it() {
        let page = extract_html(
            r#"<script type="application/ld+json">
                 {"@type":"WebSite","name":"Recipes Weekly"}</script>"#,
        );
        assert_eq!(
            value_of(page.title),
            Some(("Recipes Weekly".to_owned(), MetadataSource::SchemaOrg))
        );
    }

    /// A node naming several types is structural if any of them is, and the page's own node
    /// is the one that survives.
    #[test]
    fn a_node_that_names_several_types_is_judged_by_all_of_them() {
        let page = extract_html(
            r#"<title>the tab</title>
               <script type="application/ld+json">{"@graph":[
                 {"@type":["CreativeWork","BreadcrumbList"],"name":"Breadcrumbs"},
                 {"@type":["Article","NewsArticle"],"name":"The article"}]}</script>"#,
        );
        assert_eq!(
            value_of(page.title),
            Some(("The article".to_owned(), MetadataSource::SchemaOrg))
        );
    }

    #[test]
    fn a_graph_is_looked_into_and_deeper_nesting_is_not() {
        let page = extract_html(
            r#"<script type="application/ld+json">
                {"@graph":[{"@type":"WebPage","description":"in the graph"}]}
            </script>"#,
        );
        assert_eq!(
            value_of(page.description),
            Some(("in the graph".to_owned(), MetadataSource::SchemaOrg))
        );

        let buried = extract_html(
            r#"<script type="application/ld+json">
                {"mainEntity":{"parts":{"description":"three levels down"}}}
            </script>"#,
        );
        assert_eq!(value_of(buried.description), None);
    }

    #[test]
    fn a_publication_date_keeps_what_the_page_said_and_what_was_made_of_it() {
        let with_offset = extract_html(
            r#"<meta property="article:published_time" content="2026-07-25T14:03:22+02:00">"#,
        )
        .published_at
        .expect("a date");
        assert_eq!(with_offset.raw, "2026-07-25T14:03:22+02:00");
        assert_eq!(
            with_offset.timestamp.map(|at| at.to_string()),
            Some("2026-07-25T12:03:22Z".to_owned())
        );

        let date_only =
            extract_html(r#"<meta property="article:published_time" content="2026-07-25">"#)
                .published_at
                .expect("a date");
        assert_eq!(
            date_only.timestamp.map(|at| at.to_string()),
            Some("2026-07-25T00:00:00Z".to_owned())
        );
    }

    #[test]
    fn a_date_this_build_cannot_read_is_kept_rather_than_dropped() {
        let published = extract_html(
            r#"<meta property="article:published_time" content="last Tuesday, probably">"#,
        )
        .published_at
        .expect("a date the page stated");

        assert_eq!(published.raw, "last Tuesday, probably");
        assert_eq!(published.timestamp, None);
    }

    #[test]
    fn relative_references_resolve_against_the_page_and_then_against_its_base() {
        let page = extract_html(r#"<a href="../two">two</a><a href="/three">three</a>"#);
        assert_eq!(
            page.links
                .iter()
                .map(|link| link.url.as_str())
                .collect::<Vec<_>>(),
            ["https://example.com/two", "https://example.com/three"]
        );

        let based = extract_html(
            r#"<head><base href="https://cdn.example.net/site/"></head><a href="two">two</a>"#,
        );
        assert_eq!(based.links[0].url, "https://cdn.example.net/site/two");
    }

    #[test]
    fn a_base_that_leads_nowhere_leaves_the_page_address_in_charge() {
        for base in ["javascript:void(0)", "::::", ""] {
            let page = extract_html(&format!(
                r#"<head><base href="{base}"></head><a href="/two">two</a>"#
            ));
            assert_eq!(page.links[0].url, "https://example.com/two", "for {base:?}");
        }
    }

    #[test]
    fn a_reference_no_capture_could_ever_fetch_is_dropped() {
        let page = extract_html(
            r#"<a href="javascript:steal()">x</a>
               <a href="mailto:a@example.com">x</a>
               <a href="data:text/html,<script>x</script>">x</a>
               <a href="ftp://example.com/f">x</a>
               <a href="/kept">x</a>"#,
        );
        assert_eq!(page.links.len(), 1);
        assert_eq!(page.links[0].url, "https://example.com/kept");
    }

    #[test]
    fn links_are_deduplicated_and_the_fragment_is_not_an_identity() {
        let page = extract_html(
            r#"<a href="/a#one">x</a><a href="/a#two">x</a><a href="/a">x</a>
               <a rel="  NOFOLLOW me " href="https://other.example/b">x</a>"#,
        );
        assert_eq!(page.links.len(), 2);
        assert_eq!(page.links[0].url, "https://example.com/a");
        assert!(page.links[0].same_host);
        assert!(!page.links[1].same_host);
        assert_eq!(page.links[1].rel.as_deref(), Some("nofollow me"));
    }

    #[test]
    fn a_subresource_is_recorded_with_the_role_it_was_referenced_in() {
        let page = extract_html(
            r#"<head>
                 <link rel="stylesheet" href="/style.css">
                 <link rel="apple-touch-icon" href="/icon.png">
                 <link rel="preload" as="font" href="/f.woff2">
                 <link rel="alternate" type="application/rss+xml" href="/feed.xml">
               </head>
               <body>
                 <img src="/a.png" srcset="/a-2x.png 2x, /a-3x.png 3x">
                 <script src="/app.js"></script>
                 <video src="/v.mp4" poster="/still.jpg"></video>
                 <audio src="/a.mp3"></audio>
                 <picture><source srcset="/b.avif"></picture>
               </body>"#,
        );
        let assets: Vec<(&str, AssetKind)> = page
            .assets
            .iter()
            .map(|asset| (asset.url.as_str(), asset.kind))
            .collect();

        assert_eq!(
            assets,
            [
                ("https://example.com/style.css", AssetKind::Stylesheet),
                ("https://example.com/icon.png", AssetKind::Icon),
                ("https://example.com/a.png", AssetKind::Image),
                ("https://example.com/a-2x.png", AssetKind::Image),
                ("https://example.com/a-3x.png", AssetKind::Image),
                ("https://example.com/app.js", AssetKind::Script),
                ("https://example.com/v.mp4", AssetKind::Media),
                ("https://example.com/still.jpg", AssetKind::Image),
                ("https://example.com/a.mp3", AssetKind::Media),
                ("https://example.com/b.avif", AssetKind::Image),
            ]
        );
    }

    #[test]
    fn the_address_a_page_claims_for_itself_is_recorded_and_nothing_more() {
        let page = extract_html(r#"<link rel="canonical" href="/posts/one?utm_source=x">"#);
        assert_eq!(
            page.declared_canonical_url.as_deref(),
            Some("https://example.com/posts/one?utm_source=x")
        );
    }

    #[test]
    fn every_meta_tag_survives_whichever_spelling_it_used() {
        let page = extract_html(
            r#"<meta property="og:title" content="shared">
               <meta name="theme-color" content="dark">
               <meta charset="utf-8">
               <meta http-equiv="refresh" content="0;url=/elsewhere">
               <meta name="">"#,
        );
        assert_eq!(
            page.meta,
            [
                MetaTag {
                    name: "og:title".to_owned(),
                    content: "shared".to_owned()
                },
                MetaTag {
                    name: "theme-color".to_owned(),
                    content: "dark".to_owned()
                },
            ]
        );
    }

    #[test]
    fn a_page_that_says_the_same_thing_twice_is_read_as_saying_it_once() {
        let page = extract_html(
            r#"<meta property="og:title" content="first">
               <meta property="og:title" content="second">"#,
        );
        assert_eq!(
            value_of(page.title),
            Some(("first".to_owned(), MetadataSource::OpenGraph))
        );
    }

    #[test]
    fn json_ld_that_is_not_json_is_dropped_and_the_rest_of_the_page_still_reads() {
        let page = extract_html(
            r#"<head><title>still here</title>
                 <script type="application/ld+json">{ not json at all </script>
                 <script type="APPLICATION/LD+JSON">{"description":"read anyway"}</script>
                 <script>var x = {"description":"not ld+json"};</script>
               </head>"#,
        );
        assert_eq!(page.json_ld.len(), 1);
        assert_eq!(
            value_of(page.description),
            Some(("read anyway".to_owned(), MetadataSource::SchemaOrg))
        );
    }

    #[test]
    fn markup_that_never_closes_is_read_as_far_as_it_goes() {
        let page =
            extract_html("<html><title>a page</title><body><p>one<a href=/one>one<a href=/two>two");
        assert_eq!(
            value_of(page.title),
            Some(("a page".to_owned(), MetadataSource::Html))
        );
        assert_eq!(page.links.len(), 2);
    }

    /// A `<title>` that is never closed swallows the rest of the document, because its
    /// content is raw text until the closing tag and a browser reads it the same way. The
    /// ceiling on the field is what keeps that from putting a whole page in the record.
    #[test]
    fn an_unterminated_title_takes_the_document_with_it_and_stops_at_the_ceiling() {
        let page = extract_html(&format!(
            "<title>unterminated{}<a href=/one>one",
            "x".repeat(8192)
        ));

        let title = page.title.expect("a title");
        assert!(title.value.starts_with("unterminated"));
        assert!(title.value.len() <= 4 * 1024);
        assert_eq!(page.truncated, [Bound::Title]);
        assert!(page.links.is_empty());
    }

    #[test]
    fn a_page_built_to_be_unbounded_is_bounded() {
        let flood: String = (0..5000)
            .map(|n| format!(r#"<a href="/link-{n}">x</a><img src="/img-{n}.png">"#))
            .collect();
        let page = extract_html(&format!(
            "<title>{}</title>{}{}",
            "t".repeat(100_000),
            "<meta name=\"pad\" content=\"x\">".repeat(500),
            flood
        ));

        assert!(page.title.expect("a title").value.len() <= 4 * 1024);
        assert_eq!(page.meta.len(), 256);
        assert_eq!(page.links.len(), 2048);
        assert_eq!(page.assets.len(), 2048);
        assert_eq!(
            page.truncated,
            [Bound::Title, Bound::MetaTags, Bound::Links, Bound::Assets]
        );
    }

    #[test]
    fn a_page_that_fits_says_nothing_about_ceilings() {
        let page = extract_html("<title>small</title><a href=\"/one\">one</a>");
        assert!(page.truncated.is_empty());
        assert_eq!(page.extractor_version, EXTRACTOR_VERSION);
    }

    #[test]
    fn a_page_in_a_legacy_encoding_keeps_its_title_readable() {
        let mut body = Vec::from(*b"<html><head><title>caf\xe9</title>");
        body.extend_from_slice(b"</head></html>");

        let page = extract(PageSource {
            body: &body,
            content_type: Some("text/html; charset=windows-1252"),
            final_url: "https://example.com/",
        })
        .expect("readable")
        .expect("html");

        assert_eq!(page.title.expect("a title").value, "café");
    }

    #[test]
    fn a_url_long_enough_to_be_a_payload_is_not_a_link() {
        let payload = "p".repeat(4096);
        let page = extract_html(&format!(
            r#"<a href="/{payload}">x</a><a href="/short">x</a><img src="/{payload}.png">"#
        ));

        assert_eq!(page.links.len(), 1);
        assert_eq!(page.links[0].url, "https://example.com/short");
        assert!(page.assets.is_empty());
        // Dropped, so the record must not read as holding everything the page linked.
        assert_eq!(page.truncated, [Bound::Links, Bound::Assets]);
    }

    #[test]
    fn a_meta_tag_too_long_to_keep_whole_says_so() {
        let page = extract_html(&format!(
            r#"<meta property="og:description" content="{}">"#,
            "c".repeat(9000)
        ));

        assert_eq!(page.meta.len(), 1);
        assert_eq!(page.meta[0].content.len(), 4 * 1024);
        assert_eq!(page.truncated, [Bound::MetaContent]);
        assert_eq!(
            page.description.expect("a description").value.len(),
            4 * 1024
        );
    }

    /// Malformed markup a template that ran twice produces, and a hostile page can produce
    /// on purpose. A browser ignores the second tag's attributes.
    #[test]
    fn a_second_html_tag_neither_replaces_the_language_nor_erases_it() {
        let replaced = extract_html("<html lang=\"en\"><body>x<html lang=\"zz\">");
        let erased = extract_html("<html lang=\"en\"><body>x<html>");

        assert_eq!(
            value_of(replaced.language),
            Some(("en".to_owned(), MetadataSource::Html))
        );
        assert_eq!(
            value_of(erased.language),
            Some(("en".to_owned(), MetadataSource::Html))
        );
    }
}
