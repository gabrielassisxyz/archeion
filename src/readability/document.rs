//! Building the tree the scorer works on, refusing the ones that would cost too much, and
//! telling it where the article is on the sites it cannot work out.
//!
//! The rules hook in here because the document exists in one piece at this point and nothing
//! has scored it yet, which is the only moment at which "the article is this subtree" and
//! "these selectors are furniture" can still be said.

use std::collections::HashSet;

use dom_query::{Document, NodeId, NodeRef};
use url::Url;

use super::markup_scan::peak_open_elements;
use super::readable_markdown::readable_destination;
use super::rules::SiteRule;

/// How deeply markup may nest before the document is refused.
///
/// The scoring pass grows sharply with nesting depth. Measured on chains of `<div>` around
/// one paragraph, holding the element count near constant: 256 deep costs 0.06 s, 1000 costs
/// 2.5 s, 2000 costs 20 s and 4000 costs 157 s, for a document that is 40 KB on the wire.
/// Element count is the mild variable by comparison: 40 000 elements arranged as 160 separate
/// chains 253 deep costs 0.23 s, so a document at both ceilings stays in that range.
///
/// 256 is far past what real markup reaches, framework-generated pages included.
/// `docs/readability.md` has the measurements and how they were separated.
pub(super) const MAX_NESTING_DEPTH: usize = 256;

/// How many elements may be open at once before the document is refused.
///
/// This one bounds the parse rather than the scoring, and it is the only thing that does.
/// `html5ever` rescans its open-element stack for every end tag that searches a scope, so
/// markup that opens elements and never closes them costs time quadratic in its own size:
/// 132 KB takes 0.3 s and 528 KB takes 18 s. The byte ceiling below does not help, because
/// the cost is already minutes well under it, and the depth ceiling above cannot help,
/// because it is measured on a tree that by then has been built.
///
/// It is counted by reading bytes, in `markup_scan`, which stops at the first element past
/// this number. Well-formed markup peaks at its own nesting depth, so 2048 leaves eight times
/// the room the depth ceiling allows, and the shape this refuses needs tens of thousands.
pub(super) const MAX_OPEN_ELEMENTS: usize = 2048;

/// How large a decoded document may be before it is refused.
///
/// It bounds nothing the two ceilings above do not already bound, and it is deliberately kept
/// as the outer limit on how much work any of them can be asked to do. It is expected to move
/// against real pages: `ArticleRecord` records what each page actually measured, so the
/// ceiling can be calibrated against the distribution of articles rather than against a guess.
///
/// It is not the counterpart of `MAX_PARSER_MEMORY_BYTES` in `metadata::scan`. That one is
/// what `lol_html` may buffer on top of the document, and it refuses nothing by size.
pub(super) const MAX_DOCUMENT_BYTES: usize = 2 * 1024 * 1024;

/// What a document cost to admit, whether or not it was admitted.
///
/// Kept for every page and not only for the refused ones. A count of refusals says whether a
/// ceiling is firing; only the values the pages that passed actually reached can say whether
/// a lower ceiling would start refusing real articles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Measured {
    pub(super) byte_len: usize,
    pub(super) peak_open_elements: usize,
}

/// Why a document was refused before it was ever scored.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum TooExpensive {
    Bytes { byte_len: usize },
    OpenElements { peak: usize },
    Depth,
}

impl TooExpensive {
    pub(super) fn reason(&self) -> String {
        match self {
            Self::Bytes { byte_len } => format!(
                "the document is {byte_len} bytes, over the {MAX_DOCUMENT_BYTES} byte ceiling"
            ),
            Self::OpenElements { peak } => format!(
                "the markup leaves more than {MAX_OPEN_ELEMENTS} elements open at once, \
                 reaching {peak}"
            ),
            Self::Depth => {
                format!("the markup nests deeper than {MAX_NESTING_DEPTH} elements")
            }
        }
    }
}

/// Parses a decoded page into a tree, refusing the documents that would cost too much.
///
/// The order is the whole design. Size is free to check, the open-element count costs one
/// linear pass over bytes, and only then is a tree built. Each of the three would be too late
/// if it ran after the next one: the parse is quadratic on markup the byte scan refuses, and
/// the scoring pass is far worse on markup the depth check refuses.
pub(super) fn build(html: &str) -> Result<(Document, Measured), TooExpensive> {
    if html.len() > MAX_DOCUMENT_BYTES {
        return Err(TooExpensive::Bytes {
            byte_len: html.len(),
        });
    }
    let peak = peak_open_elements(html, MAX_OPEN_ELEMENTS);
    if peak > MAX_OPEN_ELEMENTS {
        return Err(TooExpensive::OpenElements { peak });
    }

    let document = Document::from(html);
    if nests_deeper_than(&document, MAX_NESTING_DEPTH) {
        return Err(TooExpensive::Depth);
    }
    Ok((
        document,
        Measured {
            byte_len: html.len(),
            peak_open_elements: peak,
        },
    ))
}

/// Leaves a link where an embedded document was, before the scorer ever sees it.
///
/// The scoring library's own cleaning pass removes every `iframe`, `object` and `embed`
/// outright unless one of its attributes names a domain on a short, hardcoded list of video
/// hosts: `www.youtube-nocookie.com` is on it, `open.spotify.com` and
/// `embed.podcasts.apple.com` are not, and a host that never sent that list a page never will
/// be. So an `iframe` reaches the converter at all only if it stops being one before the
/// cleaning pass runs: this rewrites it into an anchor carrying the same destination, labelled
/// with its host, which is a shape the cleaning pass already knows to leave alone. The anchor
/// handling in `markdown.rs` does the rest, exactly as it already does for a video a page wrote
/// as a plain link.
///
/// An `iframe` whose `src` is absent, does not resolve to a readable destination, or resolves
/// to one with no host to label it with, is left as an `iframe`: the cleaning pass then decides
/// its fate on its own terms, which is removal for everything this project does not also
/// recognise, rather than an empty link.
pub(super) fn link_embedded_documents(document: &Document, final_url: &str) {
    let base = Url::parse(final_url).ok();
    let Some(iframes) = document.try_select("iframe[src]") else {
        return;
    };
    for iframe in iframes.nodes() {
        let Some(src) = iframe.attr("src") else {
            continue;
        };
        let Some(destination) = readable_destination(&src, base.as_ref()) else {
            continue;
        };
        let Some(host) = Url::parse(&destination)
            .ok()
            .and_then(|url| display_host(&url))
        else {
            continue;
        };
        let anchor = document.tree.new_element("a");
        anchor.set_attr("href", &destination);
        anchor.set_text(host);
        iframe.replace_with(&anchor);
    }
}

/// The host of a resolved address, without the `www.` prefix a reader's browser already hides.
/// `www.com` keeps its prefix: stripped, `www` would not be a prefix of anything, it would be
/// the whole registrable name.
fn display_host(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    Some(
        host.strip_prefix("www.")
            .filter(|rest| rest.contains('.'))
            .unwrap_or(host)
            .to_owned(),
    )
}

/// What a host's rule made of the page.
///
/// Four answers, because the caller reads two different things out of them. Only the first is a
/// positive statement about where the prose lives, which is what a rule may weigh against the
/// scorer's own guesses. And only the last three say whether the rule reached this page at all,
/// which is what the record beside the article has to name honestly: a host whose rule is written
/// for its article pages also has listings and index pages the rule never touches, and marking
/// those as extracted under a rule would take them out of the calibration the field exists for.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Narrowed {
    /// A rule named where the article is and this page has it there. The tree is now that
    /// subtree, with whatever the rule calls furniture already taken out of it.
    ToTheArticleTheRuleNamed,
    /// A rule took furniture out of the page and said nothing about where the article is, so
    /// finding the prose in what is left is still the scorer's job.
    WithFurnitureTakenOut,
    /// The host has a rule and nothing in it matched this page, which is therefore the page the
    /// scorer would have been handed anyway.
    Untouched,
    /// The rule says where the article is on this host, and this page does not have it there.
    NotAnArticleHere,
}

/// Applies a host's rule to the tree, before anything has scored it.
///
/// The order is `body` and then `strip`, which is the order the pipeline reads in: narrow to the
/// article, then take the furniture out of it. It also makes a `strip` selector cheaper to write,
/// since by the time one runs it only has to describe what is left rather than the whole page.
///
/// Nothing is serialized and re-parsed. The subtree is moved inside the tree it is already in, so
/// a rule costs a walk rather than a second parse of a document that has already passed the
/// ceilings above.
pub(super) fn narrow(document: &Document, rule: &SiteRule) -> Narrowed {
    let named = !rule.body.is_empty();
    let Some(body) = body_of(document) else {
        // A document with no body is a `<frameset>`, and it has no article anywhere in it. A host
        // that said where its articles are has therefore already answered for this page, and one
        // that only named furniture has nothing here to take out.
        return if named {
            Narrowed::NotAnArticleHere
        } else {
            Narrowed::Untouched
        };
    };

    if named {
        let Some(article) = first_match(document, &rule.body, body) else {
            return Narrowed::NotAnArticleHere;
        };
        // Detached before the body is emptied, because emptying it first would take the article
        // down with everything else.
        for node in &article {
            node.remove_from_parent();
        }
        body.remove_children();
        for node in &article {
            body.append_child(node);
        }
    }

    let mut swept = false;
    for selector in &rule.strip {
        let Some(furniture) = document.try_select(selector) else {
            continue;
        };
        for node in furniture.nodes() {
            if !is_inside(*node, body) {
                continue;
            }
            node.remove_from_parent();
            swept = true;
        }
    }
    match (named, swept) {
        (true, _) => Narrowed::ToTheArticleTheRuleNamed,
        (false, true) => Narrowed::WithFurnitureTakenOut,
        (false, false) => Narrowed::Untouched,
    }
}

/// Whether a node is somewhere under the body, which is the only place a rule may reach.
///
/// A rule names content, and content lives in the body. Everything else a selector can reach is
/// either meaningless, as an article that is the whole page, or destructive: a strip that leaves
/// the scorer no tree, or a `*` that reparents the head into the body and hands the scorer a
/// document without one, whose title text then counts as text the page said.
///
/// The walk is bounded by the depth ceiling that has already refused anything deeper.
fn is_inside(node: NodeRef<'_>, body: NodeRef<'_>) -> bool {
    node.ancestors(None)
        .iter()
        .any(|ancestor| ancestor.id == body.id)
}

/// What the first selector that matches anything inside the body selected, with any match nested
/// inside another left out.
///
/// First-match and not every-match: a second selector is an alternative spelling of where the
/// article is, so a site that renamed its container keeps one rule instead of needing one per
/// spelling. Taking every selector's matches instead would silently glue two unrelated blocks
/// together on the pages where both happen to exist.
///
/// A match inside another match is dropped because both would be moved to the body, which would
/// pull the inner one out of the outer one and reorder the article.
///
/// The set is hashed rather than scanned, and that is not a micro-optimization. A selector is
/// written by an operator and how many nodes it matches is chosen by the page: a broad one such
/// as `div`, against a page of a hundred thousand siblings that clears all three ceilings above
/// because it is neither deep nor unbalanced, turns a linear scan per ancestor per match into the
/// superlinear cost those ceilings exist to refuse.
fn first_match<'a>(
    document: &'a Document,
    selectors: &[String],
    body: NodeRef<'a>,
) -> Option<Vec<NodeRef<'a>>> {
    for selector in selectors {
        let Some(selected) = document.try_select(selector) else {
            continue;
        };
        let matched: Vec<NodeRef<'a>> = selected
            .nodes()
            .iter()
            .filter(|node| is_inside(**node, body))
            .copied()
            .collect();
        let ids: HashSet<NodeId> = matched.iter().map(|node| node.id).collect();
        let outermost: Vec<NodeRef<'a>> = matched
            .into_iter()
            .filter(|node| {
                !node
                    .ancestors(None)
                    .iter()
                    .any(|ancestor| ids.contains(&ancestor.id))
            })
            .collect();
        if !outermost.is_empty() {
            return Some(outermost);
        }
    }
    None
}

fn body_of(document: &Document) -> Option<NodeRef<'_>> {
    document.select("body").nodes().first().copied()
}

/// The subtrees whose text is not the page's prose. A page carrying a few kilobytes of inline
/// JSON-LD or a framework's serialized state would otherwise count them as text it holds, and
/// the article inside it would look like a sliver of a much larger document.
const NOT_PROSE: [&str; 4] = ["script", "style", "noscript", "template"];

/// How much text the whole page holds, as the denominator of the sliver rule in `mod.rs`.
///
/// Counted here because this is the last moment the tree exists in one piece, before the
/// scorer takes it and decides which part of it is the article.
///
/// It is a walk and not the difference between two selections. Differencing looks equivalent
/// and is not: an element in `NOT_PROSE` may contain another, since a document parsed with
/// scripting disabled keeps the contents of `<noscript>` as real elements, and a selection
/// sums each match's whole subtree. The inner text would then be subtracted twice and the
/// denominator could reach zero, which answers "keep" and hands the page being judged a way
/// to switch the rule off.
///
/// Characters and not words, because words are whitespace-separated only in some languages:
/// counting them would score a Chinese or Japanese article at its paragraph count while
/// scoring the furniture around it, whose tokens are separated by the markup's own
/// whitespace, exactly as it scores an English page. Whitespace itself is not counted, so
/// that indented markup does not read as more text than the same page minified.
pub(super) fn page_text_chars(document: &Document) -> usize {
    let Some(body) = body_of(document) else {
        return 0;
    };
    let mut total = 0;
    let mut pending = vec![body];
    while let Some(node) = pending.pop() {
        if node.is_text() {
            total += visible_chars(&node.text());
            continue;
        }
        let skipped = node
            .node_name()
            .is_some_and(|name| NOT_PROSE.contains(&name.as_ref()));
        if !skipped {
            pending.extend(node.children());
        }
    }
    total
}

/// Text as it would be read, which is what both sides of the sliver rule are counted in.
pub(super) fn visible_chars(text: &str) -> usize {
    text.chars().filter(|c| !c.is_whitespace()).count()
}

/// Whether any element in the tree sits deeper than `ceiling`.
///
/// It answers the question rather than measuring the depth so that it can stop at the first
/// element past the ceiling: the documents this exists to refuse are the ones where walking
/// the whole tree is itself the cost being avoided.
fn nests_deeper_than(document: &Document, ceiling: usize) -> bool {
    let mut pending: Vec<(NodeRef<'_>, usize)> = vec![(document.root(), 0)];
    while let Some((node, depth)) = pending.pop() {
        if depth > ceiling {
            return true;
        }
        pending.extend(
            node.element_children()
                .into_iter()
                .map(|el| (el, depth + 1)),
        );
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_chars(html: &str) -> usize {
        let (document, _) = build(html).expect("well within every ceiling");
        page_text_chars(&document)
    }

    /// Whether `dom_smoothie`'s own cleaning pass would have kept the element on its own,
    /// which is what decides whether this file has anything to do at all. Established against
    /// the crate's real matching rules rather than assumed: `open.spotify.com` names no domain
    /// on `dom_smoothie`'s hardcoded video whitelist, so an `iframe` pointing at it is removed
    /// outright by `clean()` in `prep_article.rs` before a converter ever sees it, and that is
    /// true regardless of anything this project does downstream. `www.youtube-nocookie.com` is
    /// on that list, so a page embedding one survives `clean()` unaided. Both are pinned so a
    /// future bump of the dependency that changes either answer is caught here rather than
    /// read as a fact this project can otherwise not observe.
    #[test]
    fn the_scoring_library_keeps_only_a_hardcoded_whitelist_of_video_iframes_on_its_own() {
        for (host, kept_by_the_library_alone) in [
            ("www.youtube-nocookie.com", true),
            ("open.spotify.com", false),
            ("embed.podcasts.apple.com", false),
        ] {
            let html = format!(
                "<html><body><article><p>Watch this.</p>\
                 <iframe src=\"https://{host}/embed/one\"></iframe>\
                 <p>{}</p></article></body></html>",
                "Bread is mostly patience, and the dough will tell you when it is ready. "
                    .repeat(20)
            );
            let (document, _) = build(&html).expect("well within every ceiling");
            let mut readability = dom_smoothie::Readability::with_document(
                document,
                Some("https://example.com/posts/one"),
                None,
            )
            .expect("a page this test wrote is readable");
            let article = readability.parse().expect("enough prose to score");

            assert_eq!(
                article.content.contains("iframe"),
                kept_by_the_library_alone,
                "{host}: {}",
                article.content
            );
        }
    }

    #[test]
    fn an_iframe_with_an_absolute_src_becomes_a_link_labelled_with_its_host() {
        let (document, _) = build(
            r#"<html><body><iframe src="https://www.youtube-nocookie.com/embed/one"></iframe></body></html>"#,
        )
        .expect("well within every ceiling");

        link_embedded_documents(&document, "https://example.com/posts/one");

        assert!(!document.select("iframe").exists());
        let anchor = document.select("a");
        assert_eq!(anchor.length(), 1);
        assert_eq!(
            anchor.attr("href").as_deref(),
            Some("https://www.youtube-nocookie.com/embed/one")
        );
        assert_eq!(anchor.text().as_ref(), "youtube-nocookie.com");
    }

    /// The rule this file exists for is not keyed on a host at all: nothing here consults a
    /// whitelist, so a host absent from every corpus this project has read is covered on the
    /// same terms as one that is in it.
    #[test]
    fn a_host_absent_from_the_corpus_is_covered_on_the_same_terms() {
        let (document, _) =
            build(r#"<html><body><iframe src="https://player.example.net/watch/1"></iframe></body></html>"#)
                .expect("well within every ceiling");

        link_embedded_documents(&document, "https://example.com/posts/one");

        assert_eq!(document.select("a").text().as_ref(), "player.example.net");
    }

    #[test]
    fn a_relative_src_is_resolved_against_the_pages_own_base() {
        let (document, _) =
            build(r#"<html><body><iframe src="/embed/one"></iframe></body></html>"#)
                .expect("well within every ceiling");

        link_embedded_documents(&document, "https://example.com/posts/one");

        assert_eq!(
            document.select("a").attr("href").as_deref(),
            Some("https://example.com/embed/one")
        );
        assert_eq!(document.select("a").text().as_ref(), "example.com");
    }

    /// Nothing rather than an empty link, for both ways an iframe can fail to name an address:
    /// missing entirely, and present but not a readable destination.
    #[test]
    fn an_iframe_with_no_readable_address_is_left_for_the_scorer_to_remove() {
        for html in [
            r#"<html><body><iframe></iframe></body></html>"#,
            r#"<html><body><iframe src="javascript:alert(1)"></iframe></body></html>"#,
            // A scheme the destination policy refuses but that still resolves to a host,
            // which is the case a check reading only "does it have a host" would miss.
            r#"<html><body><iframe src="ftp://example.com/embed"></iframe></body></html>"#,
        ] {
            let (document, _) = build(html).expect("well within every ceiling");

            link_embedded_documents(&document, "https://example.com/posts/one");

            assert!(!document.select("a").exists(), "{html}");
            assert!(document.select("iframe").exists(), "{html}");
        }
    }

    /// The denominator is what a reader would have seen, so what a page carries for machines
    /// does not count as the page saying something. A framework's serialized state is easily
    /// larger than the article beside it, and counting it would make every article on such a
    /// page look like a sliver of a much larger document.
    #[test]
    fn what_a_page_carries_for_machines_is_not_the_page_saying_something() {
        let prose = "<p>Bread is patience.</p>";
        assert_eq!(page_chars(prose), 16);
        assert_eq!(
            page_chars(&format!(
                "{prose}<script>var state = {{ a: 1, b: 2, c: 3 }};</script>\
                 <style>.article {{ color: rebeccapurple }}</style>"
            )),
            16
        );
    }

    /// The regression that made this a walk rather than the difference between two selections.
    /// A document parsed with scripting disabled keeps what is inside `<noscript>` as real
    /// elements, so a `<style>` in one is matched twice by a selection that sums whole
    /// subtrees: once on its own and once inside the `<noscript>`. Differencing then subtracts
    /// it twice, and a page carrying enough of it drives the count to zero.
    ///
    /// Zero is the answer that keeps an extraction whatever its size, so the page being judged
    /// would have been handed a way to switch the rule off and be archived as an article again.
    #[test]
    fn a_style_inside_a_noscript_is_not_subtracted_twice() {
        let padding = ".rule { color: rebeccapurple; background: white }".repeat(20);
        let counted = page_chars(&format!(
            "<p>Bread is patience.</p><noscript><style>{padding}</style></noscript>"
        ));

        assert_eq!(counted, 16);
    }

    /// A document with no body is not a document whose text was measured. It answers zero, and
    /// zero has to mean "keep" wherever the rule reads it.
    #[test]
    fn a_document_with_no_body_measures_nothing() {
        let (document, _) =
            build("<frameset><frame src=\"a.html\"></frameset>").expect("within every ceiling");
        assert_eq!(page_text_chars(&document), 0);
    }

    /// Whitespace is not text. Markup indented by its author would otherwise hold more of it
    /// than the same page minified, and the share would depend on how the page was formatted.
    #[test]
    fn how_a_page_is_indented_does_not_change_how_much_it_says() {
        assert_eq!(
            page_chars("<div><p>Bread is patience.</p></div>"),
            page_chars("<div>\n    <p>\n        Bread is patience.\n    </p>\n</div>")
        );
    }

    /// The tree as text, so a test about what a rule kept can say so in the page's own words.
    fn text_after(html: &str, rule: SiteRule) -> (Narrowed, String) {
        let (document, _) = build(html).expect("well within every ceiling");
        let answer = narrow(&document, &rule);
        (answer, document.select("body").text().trim().to_owned())
    }

    fn rule(body: &[&str], strip: &[&str]) -> SiteRule {
        SiteRule {
            why: None,
            body: body.iter().map(|s| (*s).to_owned()).collect(),
            strip: strip.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    const PAGE: &str = "<html><body><nav>Home</nav>\
         <div class=\"post\"><p>The dough.</p><aside class=\"ad\">Buy this</aside></div>\
         <footer>Subscribe</footer></body></html>";

    #[test]
    fn the_article_a_rule_names_becomes_the_document() {
        let (answer, text) = text_after(PAGE, rule(&["div.post"], &[]));

        assert_eq!(answer, Narrowed::ToTheArticleTheRuleNamed);
        assert_eq!(text, "The dough.Buy this");
    }

    #[test]
    fn what_a_rule_calls_furniture_is_gone_before_anything_scores_it() {
        let (answer, text) = text_after(PAGE, rule(&[], &["nav", "aside.ad", "footer"]));

        // A strip says nothing about where the article is, so nothing here overrules a guess.
        assert_eq!(answer, Narrowed::WithFurnitureTakenOut);
        assert_eq!(text, "The dough.");
    }

    /// The order the two are applied in, which is the order the pipeline reads in. A strip
    /// running first would have to describe the whole page, and a selector that matched
    /// something outside the article would take the article's own furniture with it only by
    /// accident.
    #[test]
    fn the_article_is_taken_out_first_and_swept_afterwards() {
        let (_, text) = text_after(PAGE, rule(&["div.post"], &["aside.ad", "footer"]));

        assert_eq!(text, "The dough.");
    }

    /// A rule saying where the article is says something about every page of the host, and the
    /// pages that do not have it are the listings and the shop fronts the scorer would otherwise
    /// find enough prose in to file beside the articles.
    #[test]
    fn a_page_without_the_article_a_rule_names_is_not_an_article() {
        let (answer, text) = text_after(PAGE, rule(&["article.story"], &["nav"]));

        assert_eq!(answer, Narrowed::NotAnArticleHere);
        // Refused before the strip ran, since nothing is being scored.
        assert!(text.contains("Home"), "{text}");
    }

    /// One selector is where the article is, and a second is another spelling of the same thing.
    /// Taking every selector's matches would glue two unrelated blocks together on the pages
    /// where a site's old and new markup both happen to appear.
    #[test]
    fn only_the_first_selector_that_matches_anything_is_used() {
        let (_, text) = text_after(
            "<html><body><div class=\"old\">Before</div><div class=\"new\">After</div></body></html>",
            rule(&["div.missing", "div.new", "div.old"], &[]),
        );

        assert_eq!(text, "After");
    }

    /// A selector like `div` matches an article and everything inside it. Moving both would pull
    /// the inner one out of the outer one, so the article would come back reordered and with its
    /// own text repeated at the end.
    #[test]
    fn a_match_inside_another_match_does_not_get_moved_out_of_it() {
        let (_, text) = text_after(
            "<html><body><div><p>One.</p><div><p>Two.</p></div></div></body></html>",
            rule(&["div"], &[]),
        );

        assert_eq!(text, "One.Two.");
    }

    /// A rule is written by a person and read by a parser, and neither is careful. A selector
    /// reaching the frame of the document is either meaningless, as an article that is the whole
    /// page, or destructive: a strip that leaves the scorer no tree, or a match that reparents
    /// the head into the body and leaves the document without one.
    #[test]
    fn a_selector_naming_the_frame_of_the_document_is_passed_over() {
        let framed = "<html><head><title>t</title></head><body><nav>Home</nav>\
             <div class=\"post\"><p>The dough.</p></div></body></html>";

        for selector in ["html", "body", "head"] {
            let (document, _) = build(framed).expect("well within every ceiling");
            assert_eq!(
                narrow(&document, &rule(&[], &[selector])),
                Narrowed::Untouched
            );
            assert!(
                document.select("head title").exists(),
                "{selector} took the head"
            );
            assert!(
                document.select("div.post").exists(),
                "{selector} took the article"
            );

            let (document, _) = build(framed).expect("well within every ceiling");
            assert_eq!(
                narrow(&document, &rule(&[selector], &[])),
                Narrowed::NotAnArticleHere
            );
        }
    }

    /// The head is a sibling of the body rather than an ancestor of it, so a rule reaching for
    /// everything finds it and would move it inside the body. The scorer then reads a document
    /// with no head at all, and the title text starts counting as page text.
    #[test]
    fn a_rule_that_matches_everything_does_not_move_the_head_into_the_body() {
        let (document, _) = build(
            "<html><head><title>The oven</title></head><body>\
             <div class=\"post\"><p>The dough.</p></div></body></html>",
        )
        .expect("well within every ceiling");

        assert_eq!(
            narrow(&document, &rule(&["*"], &[])),
            Narrowed::ToTheArticleTheRuleNamed
        );
        assert!(document.select("head title").exists());
        assert_eq!(page_text_chars(&document), "Thedough.".len());
    }

    /// A `<frameset>` page has no body and no article in it either. A host that said where its
    /// articles are has therefore already answered for this page, and handing it to the scorer
    /// would be the heuristic taking over exactly where the rule switched it off.
    #[test]
    fn a_document_with_no_body_is_answered_by_the_rule_that_named_an_article() {
        let frameset = "<frameset><frame src=\"a.html\"></frameset>";
        let (document, _) = build(frameset).expect("within every ceiling");
        assert_eq!(
            narrow(&document, &rule(&["div.post"], &[])),
            Narrowed::NotAnArticleHere
        );

        let (document, _) = build(frameset).expect("within every ceiling");
        assert_eq!(narrow(&document, &rule(&[], &["nav"])), Narrowed::Untouched);
    }

    /// A rule reaches a host and not every page of it. Recording a page the rule missed as one
    /// the rule made would take a host's listings and index pages out of the calibration that
    /// naming the rule exists to make possible.
    #[test]
    fn a_rule_whose_selectors_all_missed_leaves_the_page_untouched() {
        let (answer, text) = text_after(PAGE, rule(&[], &["aside.nothing-here"]));

        assert_eq!(answer, Narrowed::Untouched);
        assert!(text.contains("The dough."), "{text}");
    }

    /// The tree is moved rather than serialized and parsed again, and this is what says so: the
    /// depth guard has already run by the time a rule is applied, so a second parse would be a
    /// second chance for the page to cost what the guard refused.
    #[test]
    fn narrowing_a_page_leaves_the_element_it_kept_intact() {
        let (document, _) = build(
            "<html><body><div class=\"post\"><p>One.</p><p><em>Two.</em></p></div>\
                   <footer>Gone</footer></body></html>",
        )
        .expect("within every ceiling");

        assert_eq!(
            narrow(&document, &rule(&["div.post"], &[])),
            Narrowed::ToTheArticleTheRuleNamed
        );
        assert_eq!(document.select("div.post em").text().as_ref(), "Two.");
        assert_eq!(page_text_chars(&document), "One.Two.".len());
    }

    fn nested(depth: usize) -> String {
        format!(
            "{}<p>buried</p>{}",
            "<div>".repeat(depth),
            "</div>".repeat(depth)
        )
    }

    #[test]
    fn ordinary_markup_is_admitted_and_measured() {
        let markup = nested(32);
        let (document, measured) = build(&markup).expect("well within every ceiling");

        assert!(!nests_deeper_than(&document, MAX_NESTING_DEPTH));
        assert_eq!(measured.byte_len, markup.len());
        // The paragraph at the bottom of the chain is the deepest element, not the chain.
        assert_eq!(measured.peak_open_elements, 33);
    }

    /// The ceiling is a number, and a test that only proves a guard exists lets the number
    /// move by three orders of magnitude without noticing. These two pin it to within one.
    ///
    /// The tree is what the depth is measured on, and the parser implies `<html>` and `<body>`
    /// around the markup, so the allowance for the markup itself is three short of the
    /// constant. That is a property of the parser and not a rounding error, so it is asserted
    /// rather than absorbed by picking a slack number.
    #[test]
    fn the_depth_ceiling_is_where_the_constant_says_it_is() {
        assert!(build(&nested(MAX_NESTING_DEPTH - 3)).is_ok());
        assert_eq!(
            build(&nested(MAX_NESTING_DEPTH - 2)).err(),
            Some(TooExpensive::Depth)
        );
    }

    /// The guard the cubic scoring pass depends on. The page is a few hundred kilobytes and
    /// would cost minutes of CPU if it reached the scorer, so the refusal is the whole point.
    #[test]
    fn markup_nested_deeply_enough_to_be_a_weapon_is_refused() {
        // Under the open-element ceiling, so this reaches the depth check rather than being
        // refused one step earlier. Both refuse it; this pins which one, and why.
        assert_eq!(build(&nested(1_000)).err(), Some(TooExpensive::Depth));
    }

    #[test]
    fn a_document_larger_than_the_ceiling_is_refused_before_it_is_parsed() {
        let huge = format!("<p>{}</p>", "x".repeat(MAX_DOCUMENT_BYTES));
        assert_eq!(
            build(&huge).err(),
            Some(TooExpensive::Bytes {
                byte_len: huge.len()
            })
        );
    }

    #[test]
    fn the_document_byte_ceiling_is_the_measured_limit() {
        assert_eq!(MAX_DOCUMENT_BYTES, 2 * 1024 * 1024);

        let frame = "<p></p>";
        let at_the_ceiling = format!("<p>{}</p>", "x".repeat(MAX_DOCUMENT_BYTES - frame.len()));
        let over_the_ceiling = format!(
            "<p>{}</p>",
            "x".repeat(MAX_DOCUMENT_BYTES - frame.len() + 1)
        );

        assert_eq!(at_the_ceiling.len(), MAX_DOCUMENT_BYTES);
        assert!(build(&at_the_ceiling).is_ok());
        assert_eq!(
            build(&over_the_ceiling).err(),
            Some(TooExpensive::Bytes {
                byte_len: MAX_DOCUMENT_BYTES + 1
            })
        );
    }

    #[test]
    fn a_news_sized_document_from_the_distribution_is_admitted() {
        let observed_news_article_bytes = 1_428_771;
        let frame = "<html><body><article><p></p></article></body></html>";
        let prose_bytes = observed_news_article_bytes - frame.len();
        let article = format!(
            "<html><body><article><p>{}</p></article></body></html>",
            "x".repeat(prose_bytes)
        );

        let (_, measured) = build(&article).expect("within the real article distribution");

        assert_eq!(measured.byte_len, observed_news_article_bytes);
    }

    /// The refusal that has to happen before the parse rather than after it. This markup makes
    /// `html5ever` rescan its open-element stack for every closing tag, which is quadratic in
    /// the document's own size: at this size it is seconds, and near the byte ceiling it would
    /// be several minutes. The depth guard below refuses the same page, and refuses it having
    /// paid.
    #[test]
    fn markup_that_only_opens_elements_is_refused_before_a_tree_is_built() {
        let flood = format!("{}{}", "<div>".repeat(80_000), "</p>".repeat(32_000));
        assert_eq!(
            build(&flood).err(),
            Some(TooExpensive::OpenElements { peak: 2049 })
        );
    }

    #[test]
    fn the_open_element_ceiling_is_where_the_constant_says_it_is() {
        let balanced =
            |count: usize| format!("{}{}", "<div>".repeat(count), "</div>".repeat(count));
        assert!(
            peak_open_elements(&balanced(MAX_OPEN_ELEMENTS), MAX_OPEN_ELEMENTS)
                <= MAX_OPEN_ELEMENTS
        );
        assert_eq!(
            build(&balanced(MAX_OPEN_ELEMENTS + 1)).err(),
            Some(TooExpensive::OpenElements {
                peak: MAX_OPEN_ELEMENTS + 1
            })
        );
    }

    /// Depth is a property of the tree and not of the markup, so markup that never closes a
    /// tag has to be measured after the parser has decided what it means.
    #[test]
    fn markup_that_never_closes_a_tag_still_has_its_depth_measured() {
        assert_eq!(
            build(&"<div>".repeat(1_000)).err(),
            Some(TooExpensive::Depth)
        );
    }
}
