//! Building the tree the scorer works on, and refusing the ones that would cost too much.
//!
//! This is the file per-host extraction rules will hook into: the document exists here, in
//! one piece, before anything has scored it, which is the only moment at which "the article
//! is this subtree" and "these selectors are furniture" can still be said.

use dom_query::{Document, NodeRef};

use super::markup_scan::peak_open_elements;

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

/// How many words the whole page holds, as the denominator of the sliver rule in `mod.rs`.
///
/// Counted here because this is the last moment the tree exists in one piece, before the
/// scorer takes it and decides which part of it is the article.
///
/// The bodies of scripts and styles are subtracted rather than left in. They are text nodes
/// like any other, so a page carrying a few kilobytes of inline JSON-LD or a framework's
/// serialized state would count them as prose it holds, and the article inside it would then
/// look like a sliver of a much larger document. Refusing real articles for having a big
/// script tag is the exact failure this rule is supposed to avoid, in reverse.
pub(super) fn page_word_count(document: &Document) -> usize {
    let body = document.select("body");
    let words = |text: &str| text.split_whitespace().count();
    // Disjoint subtrees, so the counts subtract cleanly.
    words(&body.text()).saturating_sub(words(&body.select("script, style, noscript").text()))
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
