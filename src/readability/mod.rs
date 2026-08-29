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
mod readable_markdown;
mod rules;
mod served;

use dom_smoothie::{Config, Readability, ReadabilityError};

pub use model::{
    AdmissionCost, Article, ArticleBound, ArticleRecord, EXTRACTOR_VERSION, Extraction,
    ExtractionRules, NonArticle, ProseShare, RefusedExtraction,
};
pub use rules::{SiteRule, SiteRules, UnusedRules};

use crate::metadata::PageSource;

/// How many elements the scoring pass may look at.
///
/// This bounds a wide document, where the depth ceiling bounds a deep one. Neither substitutes
/// for the other: a page can have fifty thousand siblings and no nesting at all.
const MAX_ELEMENTS_TO_SCORE: usize = 50_000;

/// How much text an extraction may hold before the page around it stops having to account for
/// it, and how small a share of that page's text it may be. Both, never either: this refuses a
/// sliver of a page that mostly said something else, and a sliver is the two things at once.
///
/// A site's front page is the shape this exists for. It carries a tagline, a description and a
/// footer blurb around a list of links, which is more prose than an imagined listing has, so
/// the readability probe admits it and the scorer then returns whichever of those blocks scored
/// best: a home page reduced to a corpus fixture yields 137 characters of boilerplate, against
/// a median of about 2000 words for the real articles of the same site.
///
/// The obvious instrument is not the one used here. Link density is what a listing is made of,
/// and it cannot see this: the list is dropped as furniture before the article is formed, so
/// what gets stored is genuine prose carrying no links at all. Nor is either number reachable
/// through the library's configuration. `readable_min_score` and `readable_min_content_length`
/// weigh text length alone, and `char_threshold` refuses nothing, since the grab loop falls
/// back to its best attempt when no attempt reaches it.
///
/// Each number alone would refuse pages that are articles, and each covers the other's mistake:
///
/// | page | characters | share | kept by |
/// |---|---|---|---|
/// | a site's front page | 137 | 0.12 | nothing, which is the point |
/// | a short post on a plain page | 281 | 0.78 | the share |
/// | a short post under a sidebar of thirty-three | 388 | 0.20 | the floor |
/// | a news article under its related links and comments | 1231 | 0.23 | the floor |
/// | the rest of the corpus articles | 501 to 1231 | 0.71 to 0.93 | both |
///
/// Every row is a page in `tests/fixtures/readability`, so each of these numbers can be
/// measured again rather than taken on trust.
///
/// A floor alone would discard the short post; a share alone would discard it as soon as its
/// page grew a sidebar, which is most pages. The last two rows are why the floor cannot simply
/// be raised: an article's share falls as far as its furniture goes, and furniture has no
/// bound.
///
/// The floor is where an observation put it, not an argument. The library's own numbers bracket
/// it without settling it: below 140 characters it stops counting a block as content at all,
/// and at 500 it stops looking for more content in a page. What decides it is that the front
/// pages seen measured 137 and about 250, and 300 sits above both without reaching the length
/// of the articles being kept.
///
/// What it costs is exact: an extraction of 299 characters or fewer survives only by being more
/// than a quarter of its page, so on a busy enough page it does not survive at all. That
/// includes the 281-character note in the corpus, which is kept because its page is plain. The
/// refusal is recorded rather than silent, which is what makes a number this blunt tolerable
/// while it is still a first guess.
///
/// That is a number from few origins, which is not enough to settle it, so every article
/// records what it measured and every refusal is written beside its capture. Both are meant to
/// move against that material rather than stay where a first guess put them, on the same terms
/// as the ceilings above.
const MIN_ARTICLE_CHARS: usize = 300;
const MAX_EXCERPT_BYTES: usize = 4 * 1024;
const MAX_BYLINE_BYTES: usize = 4 * 1024;

/// The share is a quarter, compared by multiplying out. Integer arithmetic has no rounding to
/// reason about, and it answers "keep" for a page holding no text rather than dividing by it.
const SHARE_DENOMINATOR: usize = 4;

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
/// `Extraction::Nothing` is the ordinary answer for most of the web: a listing, a shop, the
/// shell of an application that renders itself in the browser, and every capture that is not
/// HTML at all. `Extraction::Refused` is the narrower answer for a page that did produce prose
/// and lost it to the sliver rule above.
///
/// `title` comes from the metadata record rather than from this page's markup, for the reason
/// on `markdown::render`. `accessible_for_free` is the same kind of value, handed in already
/// resolved rather than derived here from raw JSON-LD, so this function reasons about one
/// three-valued declaration and not about the shapes a page might have spelled it in.
///
/// `rules` is the escape hatch for the sites the scorer cannot read. It is consulted before the
/// page is scored and its host is recorded on whatever comes out, so an extraction always says
/// whether it was worked out or told.
///
/// A response that already is the prose is answered before any of that. Nothing below can beat
/// a site's own separation of its article from its furniture, and none of the machinery below
/// has anything to do on a document with no markup in it to score. `rules` is part of what it
/// skips, and it cannot be otherwise: a rule names a subtree of a tree nothing built here, so a
/// host that has been told where its articles live is still told nothing about the Markdown
/// copies it serves beside them. `accessible_for_free` is not part of what it skips: a served
/// document has no metadata record and therefore nothing to hand in, which is the same absence
/// `title` already answers on that path.
pub fn extract(
    source: PageSource<'_>,
    title: Option<&str>,
    accessible_for_free: Option<bool>,
    rules: &SiteRules,
) -> Result<Extraction, UnreadableArticle> {
    let refused = |reason: String| UnreadableArticle {
        url: source.final_url.to_owned(),
        reason,
    };
    if let Some(document) = crate::metadata::decoded_markdown(source) {
        return served::read(&document, source.final_url).map_err(refused);
    }
    let Some(html) = crate::metadata::decoded_html(source) else {
        return Ok(Extraction::Nothing);
    };

    let (document, measured) = document::build(&html).map_err(|cost| refused(cost.reason()))?;
    // Before anything else touches the tree, and before a host's own rule might strip the
    // element away entirely: this decides what happens to an embed, and rules and scoring
    // both run on whatever it leaves behind.
    document::link_embedded_documents(&document, source.final_url);
    let matched = rules.for_url(source.final_url);
    let narrowed = match matched {
        Some(matched) => document::narrow(&document, matched.rule),
        None => document::Narrowed::Untouched,
    };
    let not_article = |rules| {
        Extraction::NotArticle(NonArticle {
            extractor_version: EXTRACTOR_VERSION,
            rules,
        })
    };
    if narrowed == document::Narrowed::NotAnArticleHere {
        let rules = matched
            .map(|matched| ExtractionRules::Site(matched.host.to_owned()))
            .unwrap_or(ExtractionRules::Heuristic);
        return Ok(not_article(rules));
    }
    // A rule that named where the article is answered two questions this function otherwise
    // guesses at: whether the page is an article, and whether what came out of it is a sliver of
    // something else. Both guesses were calibrated against a handful of sites, and neither
    // overrules an operator who looked at this host's markup and said where its prose lives.
    let told_where_the_article_is = narrowed == document::Narrowed::ToTheArticleTheRuleNamed;
    // Named after what happened to this page rather than after what the host declared. A rule
    // written for a site's articles leaves its listings and its index pages untouched, and those
    // extractions are the heuristic's: saying otherwise would take the majority of a host's
    // records out of the calibration `rules` exists to make possible.
    let applied = match (matched, narrowed == document::Narrowed::Untouched) {
        (Some(matched), false) => ExtractionRules::Site(matched.host.to_owned()),
        _ => ExtractionRules::Heuristic,
    };
    // After the rule and not before it, so that both sides of the sliver rule below count the
    // same document. A rule that narrowed the page to the article says the operator already
    // answered the question the sliver rule asks, and the record names the rule that answered it
    // so a calibration against real pages can leave these rows out.
    let page_chars = document::page_text_chars(&document);
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
    //
    // It is skipped for a page whose article was named, and skipping it is not a shortcut: the
    // probe weighs a document, a rule leaves behind a document that is only the article, and a
    // short post is then refused for being what the rule cut it down to. A forum topic of two
    // paragraphs under a thread of thirty is the shape that showed it.
    if !told_where_the_article_is && !readability.is_probably_readable() {
        return Ok(not_article(applied));
    }
    let article = match readability.parse() {
        Ok(article) => article,
        // The scorer found nothing to keep. That is the same answer as the probe above,
        // reached one step later, and not a page anyone needs to go and look at.
        Err(ReadabilityError::GrabFailed) => return Ok(not_article(applied)),
        Err(error) => return Err(refused(error.to_string())),
    };

    let mut truncated = Vec::new();
    let excerpt = bounded_excerpt(article.excerpt.as_deref(), &mut truncated);
    // Measured on the extracted text rather than on the Markdown below, so that both sides of
    // the rule count the same thing. Markdown carries link destinations and list markers that
    // the page never showed a reader, which is enough to put a short article's share above one.
    let share = ProseShare {
        article_chars: document::visible_chars(&article.text_content),
        page_chars,
    };

    if !told_where_the_article_is && share.is_a_sliver() {
        return Ok(Extraction::Refused(RefusedExtraction {
            extractor_version: EXTRACTOR_VERSION,
            rules: applied,
            share,
            excerpt,
            truncated,
        }));
    }
    let byline = bounded_byline(article.byline.as_deref(), &mut truncated);
    let prose = markdown::render(
        &article.content,
        title,
        Some(source.final_url),
        &mut truncated,
    )
    .map_err(&refused)?;
    Ok(Extraction::Article(Article {
        record: ArticleRecord {
            extractor_version: EXTRACTOR_VERSION,
            rules: applied,
            // Counted on the prose alone. The heading is a title handed in from the metadata
            // record, so counting it here would report the same words twice across two files.
            word_count: markdown::word_count(&prose.body),
            share: Some(share),
            excerpt,
            byline,
            accessible_for_free,
            truncated,
            cost: AdmissionCost {
                document_bytes: measured.byte_len,
                peak_open_elements: measured.peak_open_elements,
            },
        },
        markdown: prose.document,
    }))
}

fn bounded_excerpt(excerpt: Option<&str>, truncated: &mut Vec<ArticleBound>) -> Option<String> {
    let excerpt = non_empty(excerpt)?;
    if excerpt.len() <= MAX_EXCERPT_BYTES {
        return Some(excerpt);
    }
    let end = markdown::floor_char_boundary(&excerpt, MAX_EXCERPT_BYTES);
    truncated.push(ArticleBound::Excerpt);
    Some(excerpt[..end].to_owned())
}

fn bounded_byline(byline: Option<&str>, truncated: &mut Vec<ArticleBound>) -> Option<String> {
    let byline = non_empty(byline)?;
    if byline.len() <= MAX_BYLINE_BYTES {
        return Some(byline);
    }
    truncated.push(ArticleBound::Byline);
    None
}

impl ProseShare {
    /// Whether what was extracted is a sliver of a page that mostly said something else.
    fn is_a_sliver(&self) -> bool {
        self.article_chars < MIN_ARTICLE_CHARS
            && self.article_chars * SHARE_DENOMINATOR < self.page_chars
    }
}

/// A field the algorithm reports as present but blank is absent, since a record saying a page
/// has an empty byline claims something the page never said.
fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Reads whether a page declared itself accessible without payment, from the JSON-LD blocks
/// metadata extraction already parsed.
///
/// schema.org defines `isAccessibleForFree` on `CreativeWork`, so this is not a guess about one
/// publisher's markup: it is the field a page uses to say the content in front of a reader
/// either is or is not behind a wall, and the paywalled post that motivated this reads exactly
/// as a healthy article by every other instrument this extractor has.
///
/// The blocks arrive as hostile input. Only a literal JSON boolean is read as a declaration;
/// a string, a number or anything else found under the key is silently not one, on the same
/// terms `named_entity` in the metadata module already reads a malformed author as none rather
/// than as a guess at what it meant.
///
/// Nodes that disagree are answered toward the paywall: a page that anywhere says its content
/// is not free is treated as having said so, even when another block claims otherwise, because
/// missing a real wall defeats the reason this field exists and a spurious one costs a reader
/// nothing worse than a second look at a note that turns out whole.
///
/// Exposed so a caller can resolve it from the metadata record before calling `extract`, on the
/// same terms it already resolves `title` there.
pub fn declared_accessible_for_free(json_ld: &[serde_json::Value]) -> Option<bool> {
    let mut declared = None;
    for node in schema_org_nodes(json_ld) {
        match node
            .get("isAccessibleForFree")
            .and_then(serde_json::Value::as_bool)
        {
            Some(false) => return Some(false),
            // Only from the node that is the document itself. A page describes several works
            // in one block, its publisher and its site among them, and a site being free says
            // nothing about the post on it: taking that as the article's own declaration lets
            // a page make the archive claim a truncated note is whole, which is worse than the
            // archive saying nothing. A refusal is not held to the same test, because a page
            // has no reason to declare a wall it does not have.
            Some(true) if describes_the_document(node) => declared = Some(true),
            _ => {}
        }
    }
    declared
}

/// The schema.org types a page uses for the document being read, rather than for the site
/// around it or the organization behind it.
///
/// Kept to the types that mean an article, and deliberately not to `WebPage`, which a site
/// puts on the wrapper that holds everything else and which would give back exactly the
/// looseness this list exists to remove.
const DOCUMENT_TYPES: [&str; 6] = [
    "Article",
    "NewsArticle",
    "BlogPosting",
    "Report",
    "ScholarlyArticle",
    "TechArticle",
];

/// Whether a node describes the document this extraction is reading.
///
/// `@type` may be a string or a list of them, since one work can be several types at once.
fn describes_the_document(node: &serde_json::Value) -> bool {
    use serde_json::Value;
    let names = match node.get("@type") {
        Some(Value::String(name)) => return DOCUMENT_TYPES.contains(&name.as_str()),
        Some(Value::Array(names)) => names,
        _ => return false,
    };
    names
        .iter()
        .filter_map(Value::as_str)
        .any(|name| DOCUMENT_TYPES.contains(&name))
}

/// Flattens the shapes a JSON-LD block arrives in into the objects that might carry a field: a
/// bare object, a list of them, or a `@graph` holding the list.
///
/// This mirrors the flattening metadata resolution already does over the same blocks, and it
/// is kept local rather than shared: interpreting `isAccessibleForFree` is a decision this
/// module owns about what an extracted article is, not a metadata field metadata resolution
/// has any reason to know about.
fn schema_org_nodes(blocks: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    use serde_json::Value;
    let mut nodes = Vec::new();
    for block in blocks {
        match block {
            // A list and a `@graph` compose: a block may be a list of objects, any of which
            // holds a graph of its own. Taking the list alone reads the wrapper and misses
            // every node in it, which is a shape real pages ship.
            Value::Array(entries) => {
                for entry in entries {
                    push_node_and_its_graph(&mut nodes, entry);
                }
            }
            Value::Object(_) => push_node_and_its_graph(&mut nodes, block),
            _ => {}
        }
    }
    nodes
}

/// Adds one node and whatever its `@graph` holds. The graph is not walked further than this,
/// since a graph inside a graph is not a shape schema.org describes and following it would
/// give a hostile page a depth to choose.
fn push_node_and_its_graph<'a>(
    nodes: &mut Vec<&'a serde_json::Value>,
    node: &'a serde_json::Value,
) {
    if let Some(serde_json::Value::Array(graph)) = node.get("@graph") {
        nodes.extend(graph.iter());
    }
    nodes.push(node);
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

    fn extract_html(html: &str, title: Option<&str>) -> Extraction {
        extract_with(html, title, &SiteRules::default())
    }

    fn extract_with(html: &str, title: Option<&str>, rules: &SiteRules) -> Extraction {
        extract_declaring(html, title, None, rules)
    }

    /// The one helper here that exposes `accessible_for_free`, for the tests whose subject is
    /// the declaration itself. Every other helper passes `None`, since the shape of the page
    /// they build has nothing to do with what a paywall would have declared about it.
    fn extract_declaring(
        html: &str,
        title: Option<&str>,
        accessible_for_free: Option<bool>,
        rules: &SiteRules,
    ) -> Extraction {
        extract(
            PageSource {
                body: html.as_bytes(),
                content_type: Some("text/html; charset=utf-8"),
                final_url: "https://example.com/posts/one",
            },
            title,
            accessible_for_free,
            rules,
        )
        .expect("a page this test wrote is readable")
    }

    /// The rules a test declares for `example.com`, which is the host every page here is served
    /// from. Parsed rather than constructed, so a test states its rule in the file format an
    /// operator writes and not in a shape only this module can build.
    fn rules_for_example(declared: &str) -> SiteRules {
        let (rules, unused) = SiteRules::parse(
            &format!("{{\"hosts\": {{\"example.com\": {declared}}}}}"),
            "a test",
        );
        assert!(unused.is_empty(), "{unused:?}");
        rules
    }

    /// For the tests whose subject is the prose rather than the decision about it.
    fn article_from(html: &str, title: Option<&str>) -> Article {
        match extract_html(html, title) {
            Extraction::Article(article) => article,
            other => panic!("expected an article, got {other:?}"),
        }
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
                None,
                &SiteRules::default(),
            );
            assert_eq!(extracted, Ok(Extraction::Nothing), "for {content_type:?}");
        }
    }

    #[test]
    fn an_article_keeps_its_prose_and_drops_the_furniture() {
        let article = article_from(
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
        );

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

    /// A site's front page, which is the shape the sliver rule exists for: a heading, a
    /// description and a footer blurb around a list of links. The prose around the list is
    /// what makes the readability probe admit it, and the block the scorer then hands back is
    /// the footer, so what would be stored is the site's boilerplate under the page's title.
    fn front_page() -> String {
        format!(
            "<html><body><header><h1>The Slow Kitchen</h1>\
             <p>Notes on bread, patience and the things that take longer than the recipe \
             says they will.</p></header><main>\
             <p>This is where I write down what I have learned about baking at home, one loaf \
             at a time. Everything here is written slowly and revised often, so nothing is ever \
             quite finished, and most of it is wrong in some way I have not noticed yet. If you \
             came here for a recipe you can follow in an afternoon, the archive below is not \
             going to help you very much, and I would rather say so at the top than have you \
             find it out four paragraphs down.</p><ul>{}</ul></main>\
             <footer><p>Written by hand, published from a laptop on a kitchen table. There is \
             no newsletter, no tracking and no comment section, which suits everyone involved \
             rather well.</p></footer></body></html>",
            "<li><a href=\"/p\">Keeping a sourdough starter alive through a cold winter</a></li>"
                .repeat(12)
        )
    }

    /// The defect this rule was written for. The page passes the probe, the scorer returns a
    /// block of boilerplate, and what would land in the archive is a front page filed as an
    /// article beside the real ones.
    ///
    /// It is refused rather than passed over, because a page that produced prose and lost it
    /// to two numbers chosen against a handful of sites is the only evidence either number has.
    #[test]
    fn a_sliver_of_a_page_that_mostly_said_something_else_is_refused_and_recorded() {
        let extracted = extract_html(&front_page(), Some("The Slow Kitchen"));

        let Extraction::Refused(refused) = extracted else {
            panic!("a front page was not refused: {extracted:?}");
        };
        assert_eq!(refused.extractor_version, EXTRACTOR_VERSION);
        assert_eq!(refused.rules, ExtractionRules::Heuristic);
        // What the record describes, rather than the comparison the rule already made to get
        // here: the excerpt names the boilerplate that would otherwise have become the article,
        // and the page count is the whole page and not the block that was taken out of it.
        assert!(
            refused
                .excerpt
                .as_deref()
                .is_some_and(|excerpt| excerpt.contains("Written by hand")),
            "{refused:?}"
        );
        assert_eq!(refused.share.article_chars, 137, "{refused:?}");
        assert!(refused.share.page_chars > 1_000, "{refused:?}");
    }

    #[test]
    fn an_article_records_when_its_excerpt_was_cut() {
        let excerpt = "A".repeat(MAX_EXCERPT_BYTES - 1) + "é beyond the ceiling";
        let article = article_from(
            &format!(
                "<html><head><meta name=\"description\" content=\"{excerpt}\"></head>\
                 <body><article><h1>How to bake bread</h1>{}</article></body></html>",
                "<p>Bread is mostly patience, and the dough will tell you when it is ready.</p>"
                    .repeat(8)
            ),
            Some("How to bake bread"),
        );

        let stored = article.record.excerpt.expect("excerpt");
        assert_eq!(stored.len(), MAX_EXCERPT_BYTES - 1);
        assert!(excerpt.starts_with(&stored));
        assert_eq!(article.record.truncated, [ArticleBound::Excerpt]);
    }

    #[test]
    fn an_article_drops_a_byline_too_large_to_claim_as_attribution() {
        let byline = "A".repeat(MAX_BYLINE_BYTES + 1);
        let article = article_from(
            &format!(
                "<html><head><meta name=\"author\" content=\"{byline}\"></head>\
                 <body><article><h1>How to bake bread</h1>{}</article></body></html>",
                "<p>Bread is mostly patience, and the dough will tell you when it is ready.</p>"
                    .repeat(8)
            ),
            Some("How to bake bread"),
        );

        assert_eq!(article.record.byline, None);
        assert_eq!(article.record.truncated, [ArticleBound::Byline]);
    }

    #[test]
    fn a_refusal_records_when_its_excerpt_was_cut() {
        let excerpt = "A".repeat(MAX_EXCERPT_BYTES - 1) + "é beyond the ceiling";
        let page = front_page().replacen(
            "<html>",
            &format!("<html><head><meta name=\"description\" content=\"{excerpt}\"></head>"),
            1,
        );

        let extracted = extract_html(&page, Some("The Slow Kitchen"));
        let Extraction::Refused(refused) = extracted else {
            panic!("a front page was not refused: {extracted:?}");
        };

        let stored = refused.excerpt.expect("excerpt");
        assert_eq!(stored.len(), MAX_EXCERPT_BYTES - 1);
        assert!(excerpt.starts_with(&stored));
        assert_eq!(refused.truncated, [ArticleBound::Excerpt]);
    }

    #[test]
    fn a_refusal_does_not_record_a_byline_it_does_not_carry() {
        let byline = "A".repeat(MAX_BYLINE_BYTES + 1);
        let page = front_page().replacen(
            "<html>",
            &format!("<html><head><meta name=\"author\" content=\"{byline}\"></head>"),
            1,
        );

        let extracted = extract_html(&page, Some("The Slow Kitchen"));
        let Extraction::Refused(refused) = extracted else {
            panic!("a front page was not refused: {extracted:?}");
        };

        assert!(refused.truncated.is_empty());
    }

    #[test]
    fn an_article_inside_a_news_sized_wrapper_is_extracted() {
        let article_page = article_page("<h1>How to bake bread</h1>");
        let observed_news_article_bytes = 1_428_771;
        let portal_unit = "<span data-portal=\"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"></span>";
        let portal_wrapper = portal_unit.repeat(20_000);
        let filler_bytes = observed_news_article_bytes - article_page.len() - portal_wrapper.len();
        let filler = format!("<!--{}-->", "x".repeat(filler_bytes - 7));
        let page = article_page.replacen("<body>", &format!("<body>{portal_wrapper}"), 1);
        let page = page.replacen("</body>", &format!("{filler}</body>"), 1);

        let article = article_from(&page, Some("How to bake bread"));

        assert_eq!(
            article.record.cost.document_bytes,
            observed_news_article_bytes
        );
        assert!(article.markdown.contains("Bread is mostly patience"));
    }

    /// The other half of the rule, and the reason it is not a floor on length alone. A note
    /// of a few dozen words is a page an archive has as much reason to keep as any other, and
    /// what separates it from the front page above is not its size but that it is what its
    /// page is about.
    #[test]
    fn a_short_post_that_is_most_of_its_own_page_is_still_an_article() {
        let article = article_from(
            "<html><body><nav><a href=\"/\">Home</a></nav><article><h1>The oven is fixed</h1>\
             <p>The element went in this morning and the first loaf since March came out of it \
             an hour ago. It is not a good loaf. The thermostat reads twenty degrees low and I \
             have not compensated for it yet, so the crust is pale and the crumb is tighter \
             than it should be. But the oven works.</p></article></body></html>",
            Some("The oven is fixed"),
        );

        let share = article
            .record
            .share
            .expect("a version 2 record measures its share");
        assert!(share.article_chars < MIN_ARTICLE_CHARS, "{article:?}");
        assert!(
            article
                .markdown
                .contains("The element went in this morning")
        );
    }

    /// Both numbers, never either. A test that only proved the rule fires would let each of
    /// them move by an order of magnitude, and a rule that fired on one of them would refuse
    /// the two pages the other one keeps: the short note above, and a long article whose page
    /// carries more comments than prose.
    #[test]
    fn the_sliver_rule_is_where_the_constants_say_it_is() {
        let share = |article_chars, page_chars| ProseShare {
            article_chars,
            page_chars,
        };

        assert!(share(MIN_ARTICLE_CHARS - 1, MIN_ARTICLE_CHARS * 4).is_a_sliver());
        // One character longer, everything else equal.
        assert!(!share(MIN_ARTICLE_CHARS, MIN_ARTICLE_CHARS * 4).is_a_sliver());
        // Exactly the share, which is not under it.
        assert!(!share(MIN_ARTICLE_CHARS - 1, (MIN_ARTICLE_CHARS - 1) * 4).is_a_sliver());
        // A page holding nothing else is not a page an article is a small part of, and the
        // comparison must answer that rather than divide by it.
        assert!(!share(1, 0).is_a_sliver());
    }

    /// Most of the web. These are not empty articles: they are pages the extractor read and
    /// declined to call articles.
    #[test]
    fn a_page_that_is_not_an_article_says_so() {
        let listing = "<html><body><h1>Recipes</h1><ul>\
             <li><a href=\"/a\">Ten pasta shapes</a></li><li><a href=\"/b\">Bread</a></li>\
             <li><a href=\"/c\">Soup</a></li><li><a href=\"/d\">Cake</a></li></ul></body></html>";
        let spa_shell =
            "<html><body><div id=\"root\"></div><script src=\"/app.js\"></script></body></html>";

        for extracted in [
            extract_html(listing, Some("Recipes")),
            extract_html(spa_shell, None),
        ] {
            let Extraction::NotArticle(non_article) = extracted else {
                panic!("expected a non-article marker, got {extracted:?}");
            };
            assert_eq!(non_article.extractor_version, EXTRACTOR_VERSION);
            assert_eq!(non_article.rules, ExtractionRules::Heuristic);
        }
    }

    #[test]
    fn the_record_carries_what_produced_it() {
        let article = article_from(&article_page(""), Some("t"));

        assert_eq!(article.record.extractor_version, EXTRACTOR_VERSION);
        assert_eq!(article.record.rules, ExtractionRules::Heuristic);
        assert!(article.record.word_count > 0);
        assert!(article.record.truncated.is_empty());
    }

    /// A record's absence of a link is not evidence there was nothing to link, once an
    /// `iframe` can produce one: pinned as a literal so a repass keeps rereading articles
    /// extracted before this, rather than reading their absence of a link as `EXTRACTOR_VERSION`
    /// simply moving again for something else.
    #[test]
    fn the_version_moved_for_the_iframe_link_rule() {
        assert_eq!(EXTRACTOR_VERSION, 5);
    }

    #[test]
    fn the_byline_is_the_one_the_page_carried() {
        let article = article_from(
            &article_page("<p class=\"byline\">By J. Writer</p>"),
            Some("t"),
        );
        assert_eq!(article.record.byline.as_deref(), Some("By J. Writer"));

        let anonymous = article_from(&article_page(""), Some("t"));
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
            None,
            &SiteRules::default(),
        )
        .expect_err("refused");

        assert_eq!(refused.url, "https://example.com/deep");
        assert!(
            refused.reason.contains("elements open at once"),
            "{refused}"
        );
    }

    /// The defect this rule exists to fix, at the smallest markup that reproduces it: a
    /// sentence introducing a video, the video itself, and the sentence that follows it. The
    /// share is one, since the article is the whole page, so the sliver rule cannot be what
    /// keeps this article whether or not the link survives; what is asserted is only the link,
    /// in the place the embed was.
    #[test]
    fn a_paragraph_then_an_iframe_then_a_paragraph_extracts_with_a_link_between_them() {
        let filler =
            "Bread is mostly patience, and the dough will tell you when it is ready. ".repeat(3);
        let first_paragraph = format!("{filler}Watch the video below for the shaping steps.");
        let second_paragraph =
            format!("Once you have watched it, follow the timing table beneath. {filler}");
        let article = article_from(
            &format!(
                "<html><body><article>\
                 <p>{first_paragraph}</p>\
                 <iframe src=\"https://www.youtube-nocookie.com/embed/rJ6RZ2YzaLc\"></iframe>\
                 <p>{second_paragraph}</p>\
                 </article></body></html>"
            ),
            Some("t"),
        );

        let before = article
            .markdown
            .find("Watch the video below")
            .expect("the first paragraph survived");
        let link = article
            .markdown
            .find("[youtube-nocookie.com](https://www.youtube-nocookie.com/embed/rJ6RZ2YzaLc)")
            .unwrap_or_else(|| panic!("no link where the embed was: {}", article.markdown));
        let after = article
            .markdown
            .find("Once you have watched it")
            .expect("the second paragraph survived");
        assert!(before < link && link < after, "{}", article.markdown);
    }

    /// `dom_smoothie`'s own cleaning pass keeps an `iframe` only when one of its attributes
    /// names a domain on its hardcoded video whitelist, pinned directly in `document.rs`. Two
    /// of these three hosts are not on it, and the fourth is not on any list this project has
    /// read from a real archive at all. Producing a link on the same terms for all four is
    /// what proves the rule reads the element rather than a table of known platforms.
    #[test]
    fn an_iframe_produces_a_link_regardless_of_its_host() {
        for (src, label) in [
            (
                "https://www.youtube-nocookie.com/embed/rJ6RZ2YzaLc",
                "youtube-nocookie.com",
            ),
            (
                "https://open.spotify.com/embed/episode/abc123",
                "open.spotify.com",
            ),
            (
                "https://embed.podcasts.apple.com/us/podcast/one/id1",
                "embed.podcasts.apple.com",
            ),
            ("https://player.example.net/watch/1", "player.example.net"),
        ] {
            let article = article_from(
                &article_page(&format!("<iframe src=\"{src}\"></iframe>")),
                Some("t"),
            );
            let expected = format!("[{label}]({src})");
            assert!(
                article.markdown.contains(&expected),
                "{src}: expected {expected:?} in {}",
                article.markdown
            );
            // The label is the host, not the address the reader would already see written out
            // in the destination right beside it.
            assert!(!article.markdown.contains(&format!("[{src}]")), "{src}");
        }
    }

    #[test]
    fn a_relative_iframe_src_is_resolved_against_the_pages_own_base() {
        let article = article_from(
            &article_page("<iframe src=\"/embed/one\"></iframe>"),
            Some("t"),
        );

        assert!(
            article
                .markdown
                .contains("[example.com](https://example.com/embed/one)"),
            "{}",
            article.markdown
        );
    }

    /// Nothing rather than an empty link, on both ways an `iframe` can fail to name an
    /// address: no `src` at all, and a `src` the destination policy refuses.
    #[test]
    fn an_iframe_with_no_readable_address_produces_nothing() {
        for embed in [
            "<iframe></iframe>",
            "<iframe src=\"javascript:alert(1)\"></iframe>",
        ] {
            let article = article_from(&article_page(embed), Some("t"));
            assert!(
                !article.markdown.contains("]("),
                "{embed}: {}",
                article.markdown
            );
            assert!(
                !article.markdown.to_lowercase().contains("iframe"),
                "{embed}: {}",
                article.markdown
            );
        }
    }

    /// The regression this bead names explicitly: a video a page already wrote as a plain
    /// anchor survived before this rule existed, and this rule has no reason to touch an
    /// element it does not match. Asserted by counting the address once, since a rule keyed on
    /// content rather than on the element name could fire twice.
    #[test]
    fn a_video_written_as_a_plain_anchor_still_comes_through_whole_with_no_duplicate() {
        let article = article_from(
            &article_page("<p><a href=\"https://youtu.be/2qLXgmcv104\">HERE</a></p>"),
            Some("t"),
        );

        assert_eq!(
            article.markdown.matches("youtu.be/2qLXgmcv104").count(),
            1,
            "{}",
            article.markdown
        );
        assert!(
            article
                .markdown
                .contains("[HERE](https://youtu.be/2qLXgmcv104)"),
            "{}",
            article.markdown
        );
    }

    /// What the `rules` field is for. Two extractions of the same shape differ, and a reader a
    /// year later can tell which one was worked out and which one was told, without which the
    /// field would only ever hold one value and say nothing.
    #[test]
    fn an_extraction_records_the_host_that_was_told() {
        let page = article_page("<aside class=\"promo\">Subscribe to the newsletter</aside>");

        let alone = article_from(&page, Some("t"));
        assert_eq!(alone.record.rules, ExtractionRules::Heuristic);

        let told = extract_with(
            &page,
            Some("t"),
            &rules_for_example(r#"{"strip": ["aside.promo"]}"#),
        );
        let Extraction::Article(told) = told else {
            panic!("expected an article, got {told:?}");
        };
        assert_eq!(
            told.record.rules,
            ExtractionRules::Site("example.com".to_owned())
        );
        assert!(!told.markdown.contains("Subscribe"), "{}", told.markdown);
    }

    /// A refusal carries the rule too. A share measured on a document a rule narrowed is not
    /// comparable with one measured on a whole page, and the two numbers only stay usable for
    /// calibration if a reader can tell them apart.
    #[test]
    fn a_refusal_names_the_rule_that_was_in_force() {
        let refused = extract_with(
            &front_page(),
            Some("The Slow Kitchen"),
            &rules_for_example(r#"{"strip": ["header"]}"#),
        );

        let Extraction::Refused(refused) = refused else {
            panic!("a front page was not refused: {refused:?}");
        };
        assert_eq!(
            refused.rules,
            ExtractionRules::Site("example.com".to_owned())
        );
    }

    /// The sliver rule reads the document the scorer was given, so a rule that narrows the page
    /// to the article answers the question the sliver rule asks. The operator said where the
    /// prose is; a floor chosen against a handful of sites does not get to overrule that.
    #[test]
    fn a_rule_keeps_the_post_the_page_around_it_would_have_won() {
        let page = format!(
            "<html><body><div class=\"post\"><p>The element went in this morning and the first \
             loaf since March came out of it an hour ago. It is not a good loaf. The thermostat \
             reads twenty degrees low and I have not compensated for it yet, so the crust is \
             pale and the crumb is tighter than it should be. But the oven works, which it has \
             not done since the spring.</p></div>\
             <aside class=\"archive\"><h2>Everything else</h2>{}</aside></body></html>",
            "<p>Keeping a sourdough starter alive through a long cold winter, and what it costs \
             you in flour to do it, is a question with more than one honest answer.</p>"
                .repeat(20)
        );

        let alone = extract_html(&page, Some("The oven is fixed"));
        let Extraction::Article(alone) = alone else {
            panic!("expected an article, got {alone:?}");
        };
        assert!(
            alone.markdown.contains("Keeping a sourdough starter alive"),
            "the heuristic already got this page right, so the rule below proves nothing:\n{}",
            alone.markdown
        );

        let told = extract_with(
            &page,
            Some("The oven is fixed"),
            &rules_for_example(r#"{"body": ["div.post"]}"#),
        );
        let Extraction::Article(told) = told else {
            panic!("the rule did not rescue the post: {told:?}");
        };
        assert!(told.markdown.contains("The element went in this morning"));
        assert!(
            !told.markdown.contains("Keeping a sourdough starter alive"),
            "{}",
            told.markdown
        );
        // The share is measured on what the scorer was handed, which the rule narrowed, so the
        // post is not a sliver of a page it is no longer part of.
        let share = told
            .record
            .share
            .expect("a version 2 record measures its share");
        assert_eq!(share.article_chars, share.page_chars, "{share:?}");
    }

    /// The sliver rule is skipped and not merely outrun. Narrowing makes the page the article,
    /// so the comparison looks settled, and it is not: `article_chars` is what the scorer kept
    /// and `page_chars` is what it was handed, and the scorer takes blocks out of the very
    /// container a rule named. What is left is then a sliver of a document the rule itself
    /// assembled, and the page the rule exists to rescue is refused.
    #[test]
    fn a_page_the_rule_named_is_not_refused_for_what_the_scorer_dropped_out_of_it() {
        let page = format!(
            "<html><body><div class=\"story\">\
             <p>The element went in this morning and the first loaf came out an hour ago.</p>\
             <form>{}</form></div></body></html>",
            "<button>Sign up for the daily newsletter and never miss another story about bread \
             or ovens</button>"
                .repeat(5)
        );

        let extracted = extract_with(
            &page,
            Some("The oven is fixed"),
            &rules_for_example(r#"{"body": ["div.story"]}"#),
        );

        let Extraction::Article(article) = extracted else {
            panic!("the page the rule named was not kept: {extracted:?}");
        };
        assert!(
            article
                .markdown
                .contains("The element went in this morning")
        );
        // The comparison the rule overruled, asserted so that a page that stopped exercising it
        // fails here rather than passing as if the skip were still doing something.
        let share = article.record.share.expect("a record measures its share");
        assert!(share.is_a_sliver(), "{share:?}");
    }

    /// A page with no body is a `<frameset>`, and a host that said where its articles are has
    /// answered for it. Passing it to the scorer instead would be the heuristic taking over on a
    /// page the rule was written to speak for, which is the fallback the rule switches off.
    #[test]
    fn a_page_with_no_body_at_all_is_answered_by_the_rule_and_not_by_the_scorer() {
        let frameset = "<html><frameset><frame src=\"a.html\"></frameset></html>";

        assert_eq!(
            extract_with(
                frameset,
                None,
                &rules_for_example(r#"{"body": ["div.story"]}"#)
            ),
            Extraction::NotArticle(NonArticle {
                extractor_version: EXTRACTOR_VERSION,
                rules: ExtractionRules::Site("example.com".to_owned()),
            })
        );
    }

    /// A host whose rule is written for its articles also serves listings and index pages the
    /// rule never touches, and those extractions are the heuristic's. Recording them as made
    /// under a rule would take the majority of a host's records out of the calibration the field
    /// exists to make possible.
    #[test]
    fn a_rule_that_matched_nothing_on_a_page_is_not_recorded_as_having_made_it() {
        let article = extract_with(
            &article_page(""),
            Some("t"),
            &rules_for_example(r#"{"strip": ["aside.promo"]}"#),
        );

        let Extraction::Article(article) = article else {
            panic!("expected an article, got {article:?}");
        };
        assert_eq!(article.record.rules, ExtractionRules::Heuristic);
    }

    /// A rule that names where the article is says something about every page of the host, and
    /// the ones that do not have it are the listings the sliver rule cannot catch, because a
    /// listing carrying the opening paragraph of each entry is genuine prose.
    #[test]
    fn a_page_that_does_not_have_the_article_a_rule_names_is_passed_over() {
        let extracted = extract_with(
            &article_page(""),
            Some("t"),
            &rules_for_example(r#"{"body": ["article.story"]}"#),
        );

        assert_eq!(
            extracted,
            Extraction::NotArticle(NonArticle {
                extractor_version: EXTRACTOR_VERSION,
                rules: ExtractionRules::Site("example.com".to_owned()),
            })
        );
    }

    /// A rule reaches the host it names and no other, checked through the whole entry point
    /// rather than only where the lookup happens: a rule that silently applied to every page
    /// would strip a selector out of sites that never asked for it.
    #[test]
    fn a_rule_for_another_host_leaves_this_page_alone() {
        let (rules, unused) = SiteRules::parse(
            r#"{"hosts": {"other.example": {"strip": ["p"]}}}"#,
            "a test",
        );
        assert!(unused.is_empty(), "{unused:?}");

        let extracted = extract_with(&article_page(""), Some("t"), &rules);
        let Extraction::Article(article) = extracted else {
            panic!("expected an article, got {extracted:?}");
        };
        assert_eq!(article.record.rules, ExtractionRules::Heuristic);
        assert!(article.markdown.contains("Bread is mostly patience"));
    }

    /// The field stays one string across both variants. Every reader that filters on it compares
    /// it to a string, `jq` at a prompt included, and turning it into an object for the second
    /// variant would break all of them.
    #[test]
    fn the_rule_a_record_names_survives_a_round_trip_as_one_string() {
        for (rules, spelled) in [
            (ExtractionRules::Heuristic, "\"heuristic\""),
            (
                ExtractionRules::Site("lwn.net".to_owned()),
                "\"site:lwn.net\"",
            ),
            (ExtractionRules::Served, "\"served\""),
        ] {
            let written = serde_json::to_string(&rules).expect("a record is writable");
            assert_eq!(written, spelled);
            assert_eq!(
                serde_json::from_str::<ExtractionRules>(&written).expect("readable"),
                rules
            );
        }

        // A record naming rules this extractor cannot account for is refused rather than read as
        // the heuristic, which would claim the page was read with nothing said about it.
        for unreadable in ["\"site:\"", "\"\"", "\"site\"", "\"whatever\"", "{}"] {
            assert!(
                serde_json::from_str::<ExtractionRules>(unreadable).is_err(),
                "{unreadable} was read as an extraction rule"
            );
        }
    }

    /// Which media types reach the served reader, checked at the entry point rather than only
    /// where the list is written. `text/plain` is what many servers answer with for a `.md`
    /// path, and admitting it would turn every changelog and log in an archive into an article.
    #[test]
    fn only_a_response_that_said_it_is_markdown_is_read_as_the_prose_it_carries() {
        let document = b"# The oven is fixed\n\nThe element went in this morning.\n";
        let served = |content_type| {
            extract(
                PageSource {
                    body: document,
                    content_type,
                    final_url: "https://example.com/posts/one.md",
                },
                None,
                None,
                &SiteRules::default(),
            )
            .expect("a document this test wrote")
        };

        for content_type in [
            Some("text/markdown"),
            Some("text/markdown; charset=utf-8"),
            Some("Text/Markdown"),
            Some("text/x-markdown"),
        ] {
            let Extraction::Article(article) = served(content_type) else {
                panic!("{content_type:?} did not produce an article");
            };
            assert_eq!(article.record.rules, ExtractionRules::Served);
        }

        for content_type in [Some("text/plain"), Some("application/octet-stream"), None] {
            assert_eq!(
                served(content_type),
                Extraction::Nothing,
                "for {content_type:?}"
            );
        }
    }

    /// The response is what says how the bytes are read, and it is the only thing that does: a
    /// Markdown document has no tag to declare an encoding in, and a line of prose that merely
    /// looks like one must not decide how the document around it is decoded.
    #[test]
    fn a_served_document_in_a_legacy_encoding_keeps_its_prose_readable() {
        let (windows_1252, _, _) =
            encoding_rs::WINDOWS_1252.encode("# Petit d\u{e9}jeuner\n\nUn caf\u{e9}.\n");

        let extracted = extract(
            PageSource {
                body: &windows_1252,
                content_type: Some("text/markdown; charset=windows-1252"),
                final_url: "https://example.com/posts/one.md",
            },
            None,
            None,
            &SiteRules::default(),
        )
        .expect("readable");
        let Extraction::Article(article) = extracted else {
            panic!("expected an article, got {extracted:?}");
        };

        assert!(article.markdown.contains("café"), "{}", article.markdown);
    }

    #[test]
    fn a_page_in_a_legacy_encoding_keeps_its_prose_readable() {
        let page =
            article_page("<p>Un caf\u{e9} et du pain, voil\u{e0} le petit d\u{e9}jeuner.</p>");
        let (windows_1252, _, _) = encoding_rs::WINDOWS_1252.encode(&page);

        let extracted = extract(
            PageSource {
                body: &windows_1252,
                content_type: Some("text/html; charset=windows-1252"),
                final_url: "https://example.com/",
            },
            None,
            None,
            &SiteRules::default(),
        )
        .expect("readable");
        let Extraction::Article(article) = extracted else {
            panic!("expected an article, got {extracted:?}");
        };

        assert!(article.markdown.contains("café"), "{}", article.markdown);
    }

    /// The defect this field exists to catch: every other instrument on the record reads a
    /// paywalled teaser as a healthy article, and only the page's own declaration says
    /// otherwise.
    #[test]
    fn an_article_declared_not_free_is_marked_partial() {
        let article = article_from_declaring(&article_page(""), Some("t"), Some(false));

        assert_eq!(article.record.accessible_for_free, Some(false));
    }

    #[test]
    fn an_article_declared_free_is_marked_complete() {
        let article = article_from_declaring(&article_page(""), Some("t"), Some(true));

        assert_eq!(article.record.accessible_for_free, Some(true));
    }

    /// The third state, distinct from both. A reader that read this the same as "declared
    /// free" would treat an ordinary page, which is most of the web, as one that was vouched
    /// for by a declaration nobody made.
    #[test]
    fn an_article_with_no_declaration_says_nothing_was_declared() {
        let article = article_from_declaring(&article_page(""), Some("t"), None);

        assert_eq!(article.record.accessible_for_free, None);
    }

    fn article_from_declaring(
        html: &str,
        title: Option<&str>,
        accessible_for_free: Option<bool>,
    ) -> Article {
        match extract_declaring(html, title, accessible_for_free, &SiteRules::default()) {
            Extraction::Article(article) => article,
            other => panic!("expected an article, got {other:?}"),
        }
    }

    mod declared_accessible_for_free_tests {
        use super::declared_accessible_for_free;
        use serde_json::json;

        #[test]
        fn a_page_declaring_it_false_is_read_as_false() {
            let blocks = [json!({"@type": "Article", "isAccessibleForFree": false})];
            assert_eq!(declared_accessible_for_free(&blocks), Some(false));
        }

        /// A page describes several works in one block, and a free site says nothing about the
        /// post on it. Taking that as the article's own word would let a page make the archive
        /// claim a truncated note is whole, which is worse than the archive saying nothing.
        #[test]
        fn a_claim_of_free_access_by_something_that_is_not_the_document_is_not_the_document_s() {
            let blocks = [json!([
                {"@type": "WebSite", "isAccessibleForFree": true},
                {"@type": "Article", "headline": "a post that said nothing either way"},
            ])];
            assert_eq!(declared_accessible_for_free(&blocks), None);
        }

        /// The refusal is not held to that test. A page has no reason to declare a wall it does
        /// not have, so the conservative answer stays reachable from anywhere in the block.
        #[test]
        fn a_refusal_anywhere_in_the_block_is_still_read_as_a_refusal() {
            let blocks = [json!([
                {"@type": "WebSite", "isAccessibleForFree": false},
                {"@type": "Article", "headline": "a post behind whatever that was"},
            ])];
            assert_eq!(declared_accessible_for_free(&blocks), Some(false));
        }

        /// One work can be several types at once, and a page writes that as a list.
        #[test]
        fn a_node_typed_as_several_things_counts_if_one_of_them_is_the_document() {
            let blocks = [json!({
                "@type": ["CreativeWork", "BlogPosting"],
                "isAccessibleForFree": true,
            })];
            assert_eq!(declared_accessible_for_free(&blocks), Some(true));
        }

        /// A list and a graph compose, which is a shape real pages ship: a block is a list whose
        /// entries hold graphs of their own. Reading only the outer list finds the wrappers and
        /// none of the nodes.
        #[test]
        fn a_graph_nested_inside_a_list_is_still_read() {
            let blocks = [json!([
                {"@graph": [{"@type": "Article", "isAccessibleForFree": false}]},
            ])];
            assert_eq!(declared_accessible_for_free(&blocks), Some(false));
        }

        #[test]
        fn a_page_declaring_it_true_is_read_as_true() {
            let blocks = [json!({"@type": "Article", "isAccessibleForFree": true})];
            assert_eq!(declared_accessible_for_free(&blocks), Some(true));
        }

        #[test]
        fn a_page_declaring_nothing_reads_as_no_declaration() {
            assert_eq!(declared_accessible_for_free(&[]), None);

            let blocks = [json!({"@type": "Article", "headline": "A page"})];
            assert_eq!(declared_accessible_for_free(&blocks), None);
        }

        /// Hostile shapes: the property schema.org types as a boolean, spelled by a page as
        /// something else. Neither is read as a declaration, on the same terms a malformed
        /// author elsewhere in this archive is read as none rather than as a guess.
        #[test]
        fn a_declaration_spelled_as_the_wrong_type_is_not_read_as_one() {
            for hostile in [
                json!({"isAccessibleForFree": "false"}),
                json!({"isAccessibleForFree": "true"}),
                json!({"isAccessibleForFree": 0}),
                json!({"isAccessibleForFree": 1}),
                json!({"isAccessibleForFree": null}),
                json!({"isAccessibleForFree": ["false"]}),
                json!({"isAccessibleForFree": {}}),
            ] {
                assert_eq!(
                    declared_accessible_for_free(std::slice::from_ref(&hostile)),
                    None,
                    "{hostile}"
                );
            }
        }

        /// A block that is a bare array of nodes, and a block that nests its nodes inside
        /// `@graph`, which is the shape a page attaches its own type to alongside the site's.
        #[test]
        fn a_declaration_nested_inside_an_array_or_a_graph_is_still_read() {
            let array_block = json!([
                {"@type": "Organization", "name": "A Site"},
                {"@type": "Article", "isAccessibleForFree": false},
            ]);
            assert_eq!(declared_accessible_for_free(&[array_block]), Some(false));

            let graph_block = json!({
                "@context": "https://schema.org",
                "@graph": [
                    {"@type": "WebSite", "name": "A Site"},
                    {"@type": "Article", "isAccessibleForFree": false},
                ],
            });
            assert_eq!(declared_accessible_for_free(&[graph_block]), Some(false));
        }

        /// Repeated and conflicting. The conservative reading is the one that keeps a real wall
        /// from being missed: a page that anywhere says its content is not free is read as
        /// having said so, whatever another block claims.
        #[test]
        fn conflicting_declarations_resolve_toward_the_paywall() {
            let blocks = [
                json!({"@type": "Article", "isAccessibleForFree": true}),
                json!({"@type": "Article", "isAccessibleForFree": false}),
            ];
            assert_eq!(declared_accessible_for_free(&blocks), Some(false));

            // Order does not decide it: the same pair, read in the other order, still resolves
            // to the paywall.
            let reversed = [blocks[1].clone(), blocks[0].clone()];
            assert_eq!(declared_accessible_for_free(&reversed), Some(false));
        }
    }
}
