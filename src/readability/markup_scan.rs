//! Counting open elements without building anything.
//!
//! This runs before the tree parser, and the only reason it exists is that the tree parser
//! cannot be trusted with the input first. `html5ever` rescans its whole open-element stack
//! for every end tag that has to search a scope, so markup that opens elements and never
//! closes them costs time quadratic in its own size: 132 KB takes 0.3 s, 528 KB takes 18 s,
//! and a document near the byte ceiling would take several minutes. Refusing it afterwards,
//! which is what the depth guard does, refuses it having already paid.
//!
//! So this reads bytes and no more. It is not a parser and cannot become one: it never
//! allocates, it never looks back, and it stops at the first element past the ceiling.
//!
//! It is deliberately more careful than counting angle brackets, because every shortcut here
//! turns into a legitimate page being refused. Void elements never close and would otherwise
//! accumulate, so an image gallery would be read as unbalanced markup. Attribute values hold
//! `<` in ordinary prose. Script and style bodies hold it in every comparison anyone writes.

/// Elements that have no closing tag. Counting them as open would make a page of images look
/// like a page of unclosed markup, which is the shape this guard exists to refuse.
const VOID_ELEMENTS: [&[u8]; 14] = [
    b"area", b"base", b"br", b"col", b"embed", b"hr", b"img", b"input", b"link", b"meta", b"param",
    b"source", b"track", b"wbr",
];

/// Elements whose content is raw text rather than markup, so a `<` inside them is not a tag.
const RAW_TEXT_ELEMENTS: [&[u8]; 2] = [b"script", b"style"];

/// The greatest number of elements open at one time, or `ceiling + 1` if the markup went past
/// it. Stopping early is the point: the documents this refuses are the ones where reading to
/// the end is itself the cost being avoided.
pub(super) fn peak_open_elements(html: &str, ceiling: usize) -> usize {
    let bytes = html.as_bytes();
    let (mut open, mut peak, mut at) = (0usize, 0usize, 0usize);

    while at < bytes.len() {
        if bytes[at] != b'<' {
            at += 1;
            continue;
        }
        let after = at + 1;
        if after >= bytes.len() {
            break;
        }
        if bytes[after] == b'/' {
            open = open.saturating_sub(1);
            at = end_of_tag(bytes, after).0;
            continue;
        }
        if bytes[after] == b'!' || bytes[after] == b'?' {
            // A comment, a doctype or a processing instruction. None of them opens an element,
            // and a comment has to be walked past rather than stepped over: the markup inside
            // one is text, and counting it would let a page be refused for what it commented
            // out.
            at = past_bracketed(bytes, at);
            continue;
        }
        if !bytes[after].is_ascii_alphabetic() {
            // A stray `<` in text, which must not be counted as if it opened something.
            at += 1;
            continue;
        }

        let name_end = after + name_length(&bytes[after..]);
        let name = &bytes[after..name_end];
        let (past_tag, self_closing) = end_of_tag(bytes, name_end);
        at = past_tag;

        if matches_any(name, &RAW_TEXT_ELEMENTS) {
            at = past_raw_text(bytes, at, name);
            continue;
        }
        if self_closing || matches_any(name, &VOID_ELEMENTS) {
            continue;
        }
        open += 1;
        if open > peak {
            peak = open;
            if peak > ceiling {
                return peak;
            }
        }
    }
    peak
}

/// Walks past a comment, a doctype or a processing instruction, given the index of its `<`.
///
/// A comment ends at `-->` and nothing else, so it cannot be treated as a tag: `<!-- a > b -->`
/// holds a `>` that ends nothing, and stopping there would resume inside the comment.
fn past_bracketed(bytes: &[u8], from: usize) -> usize {
    if bytes[from..].starts_with(b"<!--") {
        let mut at = from + 4;
        while at + 2 < bytes.len() {
            if &bytes[at..at + 3] == b"-->" {
                return at + 3;
            }
            at += 1;
        }
        return bytes.len();
    }
    end_of_tag(bytes, from + 1).0
}

fn name_length(from_name: &[u8]) -> usize {
    from_name
        .iter()
        .take_while(|byte| byte.is_ascii_alphanumeric())
        .count()
}

/// Walks to just past the `>` that ends a tag, and says whether it closed itself.
///
/// Quoting is honored because an attribute value is the one place inside a tag where `>` and
/// `<` appear as ordinary characters, and a scan that stopped at the first `>` would resume
/// in the middle of a value and read its text as markup.
fn end_of_tag(bytes: &[u8], from: usize) -> (usize, bool) {
    let mut at = from;
    let mut quote: Option<u8> = None;
    let mut previous = b' ';
    while at < bytes.len() {
        let byte = bytes[at];
        match quote {
            Some(open) if byte == open => quote = None,
            Some(_) => {}
            None if byte == b'"' || byte == b'\'' => quote = Some(byte),
            None if byte == b'>' => return (at + 1, previous == b'/'),
            None => {}
        }
        previous = byte;
        at += 1;
    }
    (bytes.len(), false)
}

/// Walks past the body of a raw-text element to its closing tag. Everything in between is
/// text however much it looks like markup, which is why `a<b` in a script is not an element.
fn past_raw_text(bytes: &[u8], from: usize, name: &[u8]) -> usize {
    let mut at = from;
    while at + 1 < bytes.len() {
        if bytes[at] == b'<' && bytes[at + 1] == b'/' {
            let candidate = at + 2;
            let candidate_end = candidate + name_length(&bytes[candidate..]);
            if bytes[candidate..candidate_end].eq_ignore_ascii_case(name) {
                return end_of_tag(bytes, candidate_end).0;
            }
        }
        at += 1;
    }
    bytes.len()
}

fn matches_any(name: &[u8], names: &[&[u8]]) -> bool {
    names.iter().any(|known| name.eq_ignore_ascii_case(known))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CEILING: usize = 2048;

    #[test]
    fn well_formed_markup_peaks_at_its_own_nesting() {
        assert_eq!(
            peak_open_elements("<html><body><div><p>text</p></div></body></html>", CEILING),
            4
        );
        assert_eq!(peak_open_elements("", CEILING), 0);
        assert_eq!(peak_open_elements("no markup at all", CEILING), 0);
    }

    /// The shape the guard exists for: elements that open and never close, which is what makes
    /// the tree parser quadratic in its own input.
    #[test]
    fn markup_that_only_ever_opens_is_counted_as_open() {
        let peak = peak_open_elements(&"<div>".repeat(5_000), CEILING);
        assert!(peak > CEILING, "counted {peak}");
    }

    #[test]
    fn counting_stops_at_the_ceiling_instead_of_reading_to_the_end() {
        // Past the ceiling early, then a great deal more. A scan that read it all would be
        // doing the work the ceiling exists to refuse.
        let markup = format!(
            "{}{}",
            "<div>".repeat(CEILING + 1),
            "<span>".repeat(500_000)
        );
        assert_eq!(peak_open_elements(&markup, CEILING), CEILING + 1);
    }

    /// A page of images is not unbalanced markup. Void elements have no closing tag, so
    /// counting them would make an ordinary gallery look like an attack.
    #[test]
    fn elements_that_never_close_are_not_counted_as_open() {
        let gallery = format!("<div>{}</div>", "<img src=\"/a.png\"><br>".repeat(5_000));
        assert_eq!(peak_open_elements(&gallery, CEILING), 1);
    }

    /// Self-closing syntax is how inline graphics are written, and an icon set would otherwise
    /// accumulate one phantom open element per path.
    #[test]
    fn a_tag_that_closes_itself_is_not_left_open() {
        let icons = format!("<div>{}</div>", "<path d=\"M0 0\"/>".repeat(5_000));
        assert_eq!(peak_open_elements(&icons, CEILING), 1);
    }

    /// `<` is an operator in every language a page embeds, and a comparison is not a tag.
    #[test]
    fn markup_inside_a_script_or_a_style_is_text() {
        let script = format!(
            "<div><script>{}</script></div>",
            "if (a<b && c<d) { x(); }".repeat(5_000)
        );
        assert_eq!(peak_open_elements(&script, CEILING), 1);

        let style = format!("<div><style>{}</style></div>", "a<b{}".repeat(5_000));
        assert_eq!(peak_open_elements(&style, CEILING), 1);
    }

    /// An attribute value is the one place inside a tag where `<` and `>` are ordinary text.
    /// A scan that stopped at the first `>` would resume inside the value and read it as markup.
    #[test]
    fn angle_brackets_inside_an_attribute_are_not_markup() {
        let markup = "<div title=\"a > b and <div> too\"><p alt='x<y'>text</p></div>";
        assert_eq!(peak_open_elements(markup, CEILING), 2);
    }

    #[test]
    fn a_comment_or_a_doctype_opens_nothing() {
        let markup = "<!doctype html><!-- <div><div><div> --><p>text</p>";
        assert_eq!(peak_open_elements(markup, CEILING), 1);
    }

    /// Prose that was never escaped. It is invalid markup, a browser reads most of it as text,
    /// and the guard counts it as elements: the error is upward, so such a page is refused
    /// rather than let through, which is the safe direction for a guard to be wrong in.
    #[test]
    fn unescaped_prose_is_over_counted_rather_than_under_counted() {
        let peak = peak_open_elements("<p>use <section> when you mean a section</p>", CEILING);
        assert!(peak >= 2, "counted {peak}");
    }

    /// Markup that stops mid-tag. Nothing may read past the end of the document; a tag that
    /// never terminated still counts as open, which is the upward error a guard should make.
    #[test]
    fn a_tag_that_never_ends_does_not_run_past_the_document() {
        assert_eq!(peak_open_elements("<div", CEILING), 1);
        assert_eq!(peak_open_elements("<div attr=\"unterminated", CEILING), 1);
        assert_eq!(peak_open_elements("<script>never closed", CEILING), 0);
        assert_eq!(peak_open_elements("<!-- never closed", CEILING), 0);
        assert_eq!(peak_open_elements("<", CEILING), 0);
    }

    #[test]
    fn closing_more_than_was_opened_does_not_wrap_around() {
        assert_eq!(peak_open_elements(&"</p>".repeat(1_000), CEILING), 0);
        assert_eq!(
            peak_open_elements(&format!("{}<div>", "</p>".repeat(1_000)), CEILING),
            1
        );
    }
}
