//! Building the tree the scorer works on, refusing the ones that would cost too much, and
//! telling it where the article is on the sites it cannot work out.
//!
//! The rules hook in here because the document exists in one piece at this point and nothing
//! has scored it yet, which is the only moment at which "the article is this subtree" and
//! "these selectors are furniture" can still be said.

use dom_query::{Document, NodeId, NodeRef};

use super::markup_scan::peak_open_elements;
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
/// as the outer limit on how much work any of them can be asked to do. It is expected to come
/// down: `ArticleRecord` records what each page actually measured, so the ceiling can be
/// lowered against the distribution of real pages rather than against a guess.
///
/// It is not the counterpart of `MAX_PARSER_MEMORY_BYTES` in `metadata::scan`. That one is
/// what `lol_html` may buffer on top of the document, and it refuses nothing by size.
pub(super) const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;

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

/// What a host's rule made of the page.
///
/// Three answers rather than two, because only one of them is a positive statement about where
/// the prose lives. A caller weighs that statement against its own guesses; the other two leave
/// every guess exactly where it was.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Narrowed {
    /// A rule named where the article is and this page has it there. The tree is now that
    /// subtree, with whatever the rule calls furniture already taken out of it.
    ToTheArticleTheRuleNamed,
    /// A rule took furniture out and said nothing about where the article is, so the page is
    /// still a page and finding the prose in it is still the scorer's job.
    ToWhatIsLeftOfThePage,
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
    let Some(body) = body_of(document) else {
        return Narrowed::ToWhatIsLeftOfThePage;
    };
    // The body and what holds it. A selector matching one of these is either meaningless, as an
    // article that is the whole document, or destructive, as a strip that leaves no tree at all.
    let page: Vec<NodeId> = std::iter::once(body.id)
        .chain(body.ancestors(None).into_iter().map(|node| node.id))
        .collect();

    let named = !rule.body.is_empty();
    if named {
        let Some(article) = first_match(document, &rule.body, &page) else {
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

    for selector in &rule.strip {
        let Some(furniture) = document.try_select(selector) else {
            continue;
        };
        for node in furniture.nodes() {
            if page.contains(&node.id) {
                continue;
            }
            node.remove_from_parent();
        }
    }
    if named {
        Narrowed::ToTheArticleTheRuleNamed
    } else {
        Narrowed::ToWhatIsLeftOfThePage
    }
}

/// What the first selector that matches anything selected, with any match nested inside another
/// left out.
///
/// First-match and not every-match: a second selector is an alternative spelling of where the
/// article is, so a site that renamed its container keeps one rule instead of needing one per
/// spelling. Taking every selector's matches instead would silently glue two unrelated blocks
/// together on the pages where both happen to exist.
///
/// A match inside another match is dropped because both would be moved to the body, which would
/// pull the inner one out of the outer one and reorder the article. `Vec::contains` is the lookup
/// because a selector naming where an article lives matches a handful of nodes, not thousands.
fn first_match<'a>(
    document: &'a Document,
    selectors: &[String],
    page: &[NodeId],
) -> Option<Vec<NodeRef<'a>>> {
    for selector in selectors {
        let Some(selected) = document.try_select(selector) else {
            continue;
        };
        let matched: Vec<NodeRef<'a>> = selected
            .nodes()
            .iter()
            .filter(|node| !page.contains(&node.id))
            .copied()
            .collect();
        let ids: Vec<NodeId> = matched.iter().map(|node| node.id).collect();
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
        assert_eq!(answer, Narrowed::ToWhatIsLeftOfThePage);
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
    /// that names the page itself is either meaningless, as an article that is the whole
    /// document, or destructive, as a strip that leaves the scorer no tree at all.
    #[test]
    fn a_selector_naming_the_page_itself_is_passed_over() {
        let (answer, text) = text_after(PAGE, rule(&[], &["html", "body"]));
        assert_eq!(answer, Narrowed::ToWhatIsLeftOfThePage);
        assert!(text.contains("The dough."), "{text}");

        let (answer, text) = text_after(PAGE, rule(&["body"], &[]));
        assert_eq!(answer, Narrowed::NotAnArticleHere, "{text}");
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

    /// The refusal that has to happen before the parse rather than after it. This markup makes
    /// `html5ever` rescan its open-element stack for every closing tag, which is quadratic in
    /// the document's own size: at this size it is seconds, and at the byte ceiling it is over
    /// an hour. The depth guard below refuses the same page, and refuses it having paid.
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
