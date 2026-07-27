//! Building the tree the scorer works on, and refusing the ones that would cost too much.
//!
//! This is the file per-host extraction rules will hook into: the document exists here, in
//! one piece, before anything has scored it, which is the only moment at which "the article
//! is this subtree" and "these selectors are furniture" can still be said.

use dom_query::{Document, NodeRef};

/// How deeply markup may nest before the document is refused.
///
/// The scoring pass is cubic in nesting depth. Measured on nothing but nested `<div>`
/// elements around one paragraph: 256 deep costs 0.06 s, 1000 costs 2.5 s, 2000 costs 20 s
/// and 4000 costs 157 s, for a document that is 40 KB on the wire. None of the other
/// ceilings catch that, because a page built this way has few elements and few bytes.
///
/// 256 is far past what real markup reaches, framework-generated pages included, and it
/// holds the worst case to about sixty milliseconds. `docs/readability.md` has the numbers.
pub(super) const MAX_NESTING_DEPTH: usize = 256;

/// How large a decoded document may be before it is refused.
///
/// The tree is built before any ceiling on what it contains can be applied, so this is the
/// one that bounds the memory of the parse itself. It matches the ceiling metadata
/// extraction puts on its own parser, which is the same bet about how large a page gets.
pub(super) const MAX_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;

/// Why a document was refused before it was ever scored.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum TooExpensive {
    Bytes { byte_len: usize },
    Depth,
}

impl TooExpensive {
    pub(super) fn reason(&self) -> String {
        match self {
            Self::Bytes { byte_len } => format!(
                "the document is {byte_len} bytes, over the {MAX_DOCUMENT_BYTES} byte ceiling"
            ),
            Self::Depth => {
                format!("the markup nests deeper than {MAX_NESTING_DEPTH} elements")
            }
        }
    }
}

/// Parses a decoded page into a tree, refusing the documents that would cost too much to
/// score. Both refusals happen before the scoring pass, which is the expensive one.
pub(super) fn build(html: &str) -> Result<Document, TooExpensive> {
    if html.len() > MAX_DOCUMENT_BYTES {
        return Err(TooExpensive::Bytes {
            byte_len: html.len(),
        });
    }
    let document = Document::from(html);
    if nests_deeper_than(&document, MAX_NESTING_DEPTH) {
        return Err(TooExpensive::Depth);
    }
    Ok(document)
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
    fn ordinary_markup_is_nowhere_near_the_depth_ceiling() {
        let document = build(&nested(32)).expect("well within the ceiling");
        assert!(!nests_deeper_than(&document, MAX_NESTING_DEPTH));
    }

    /// The guard the cubic scoring pass depends on. The page is a few hundred kilobytes and
    /// would cost minutes of CPU if it reached the scorer, so the refusal is the whole point.
    #[test]
    fn markup_nested_deeply_enough_to_be_a_weapon_is_refused() {
        assert_eq!(build(&nested(5_000)).err(), Some(TooExpensive::Depth));
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

    /// Depth is a property of the tree and not of the markup, so markup that never closes a
    /// tag has to be measured after the parser has decided what it means.
    #[test]
    fn markup_that_never_closes_a_tag_still_has_its_depth_measured() {
        assert_eq!(
            build(&"<div>".repeat(5_000)).err(),
            Some(TooExpensive::Depth)
        );
    }
}
