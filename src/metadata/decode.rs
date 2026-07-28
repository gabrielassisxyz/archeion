//! Turning a stored body into text.
//!
//! An archive collects pages from every year the web has had, and a fair number of them are
//! not UTF-8. Guessing wrong writes a mangled title into a record that outlives the page, so
//! the encoding is taken from what the page and the response actually declared, in the order
//! the HTML standard gives them, and never from statistical detection: a wrong guess is
//! indistinguishable from a right one once it is stored.

use std::borrow::Cow;

use encoding_rs::{Encoding, UTF_8};

/// How far into the body a `<meta charset>` is looked for. The standard requires the
/// declaration to sit in the first kilobyte, and a scan without a bound would be reading
/// the whole page twice to honor a declaration that is not allowed to be there anyway.
const PRESCAN_BYTES: usize = 1024;

/// Decodes a page body to text.
///
/// The order is a byte order mark, then the `charset` the response declared, then a
/// `<meta charset>` near the top of the document, then UTF-8. Bytes that are not valid in
/// the chosen encoding become replacement characters rather than an error: a page with one
/// bad byte still has a title, and refusing the whole document over it would archive less
/// than the bytes support.
pub(crate) fn decode_html<'a>(bytes: &'a [u8], declared_charset: Option<&str>) -> Cow<'a, str> {
    let fallback = declared_charset.and_then(label_to_encoding).or_else(|| {
        prescan_meta_charset(bytes)
            .as_deref()
            .and_then(label_to_encoding)
    });
    decode_with(bytes, fallback)
}

/// Decodes a body that is not markup.
///
/// The same order minus the prescan, because there is no document to prescan: what looks like
/// a `<meta charset>` inside a Markdown file is a line of text, and reading it as a
/// declaration would let a document that writes about markup decide how it is read.
pub(crate) fn decode_text<'a>(bytes: &'a [u8], declared_charset: Option<&str>) -> Cow<'a, str> {
    decode_with(bytes, declared_charset.and_then(label_to_encoding))
}

fn decode_with<'a>(bytes: &'a [u8], fallback: Option<&'static Encoding>) -> Cow<'a, str> {
    // `decode` sniffs a byte order mark first and lets it override the fallback, which is
    // exactly the precedence wanted here and the reason it is preferred over `decode_with_bom_removal`.
    let (text, _encoding_used, _had_errors) = fallback.unwrap_or(UTF_8).decode(bytes);
    text
}

fn label_to_encoding(label: &str) -> Option<&'static Encoding> {
    Encoding::for_label(label.trim().as_bytes())
}

/// Looks for a `charset` declaration in the head of the document.
///
/// This is deliberately a substring scan and not a parse. The full standard prescan is a
/// tokenizer of its own, run before the encoding is known, and every case where the two
/// disagree is a page carrying the word `charset` inside markup that precedes its own
/// declaration.
///
/// Two things keep it from being fooled anyway, and both are needed. Every occurrence is
/// tried rather than the first, so one that names nothing cannot take the real declaration
/// below it out of reach. And an occurrence only counts inside a `<meta>` tag, because the
/// most common stray one names a perfectly valid encoding: a stylesheet linked as
/// `/s.css?charset=utf8` would otherwise decide the encoding of the whole document, and
/// trying further matches would not help, since that one parses.
fn prescan_meta_charset(bytes: &[u8]) -> Option<String> {
    let head = &bytes[..bytes.len().min(PRESCAN_BYTES)];
    let head = String::from_utf8_lossy(head).to_ascii_lowercase();
    head.match_indices("charset")
        .filter(|&(at, _)| is_inside_a_meta_tag(&head, at))
        .find_map(|(at, word)| charset_label_at(&head[at + word.len()..]))
}

/// Whether the byte at `at` sits inside an open `<meta` tag.
///
/// The nearest `<` before it has to open one, and nothing may have closed that tag in
/// between. It is a cheap stand-in for knowing the token, and it costs a backwards scan
/// over markup already in memory.
fn is_inside_a_meta_tag(head: &str, at: usize) -> bool {
    let Some(opening) = head[..at].rfind('<') else {
        return false;
    };
    if head[opening..at].contains('>') {
        return false;
    }
    head[opening + 1..]
        .strip_prefix("meta")
        .is_some_and(|after| after.starts_with([' ', '\t', '\n', '\r', '/', '>']))
}

/// Reads the label out of what follows a `charset` occurrence, or refuses it.
fn charset_label_at(rest: &str) -> Option<String> {
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let label: String = match rest.chars().next()? {
        quote @ ('"' | '\'') => rest[1..].chars().take_while(|&c| c != quote).collect(),
        _ => rest
            .chars()
            .take_while(|c| !c.is_whitespace() && !matches!(c, ';' | '"' | '\'' | '/' | '>'))
            .collect(),
    };
    // Validated here rather than by the caller, so an occurrence that names no encoding
    // leaves the scan looking at the ones after it instead of ending it.
    label_to_encoding(&label).map(|_| label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_declared_charset_beats_the_utf8_default() {
        // 0xE9 is `é` in windows-1252 and not valid UTF-8 at all.
        let body = b"<title>caf\xe9</title>";

        assert_eq!(
            decode_html(body, Some("windows-1252")),
            "<title>café</title>"
        );
        assert!(decode_html(body, None).contains('\u{fffd}'));
    }

    #[test]
    fn a_charset_parameter_is_read_the_way_a_header_writes_it() {
        let body = b"caf\xe9";
        for label in ["windows-1252", " WINDOWS-1252 ", "cp1252"] {
            assert_eq!(decode_html(body, Some(label)), "café");
        }
    }

    #[test]
    fn the_page_declares_its_own_encoding_when_the_response_did_not() {
        let body = b"<head><meta charset=\"windows-1252\"><title>caf\xe9</title></head>";
        assert!(decode_html(body, None).contains("café"));

        let quoteless = b"<head><meta charset=iso-8859-1><title>caf\xe9</title>";
        assert!(decode_html(quoteless, None).contains("café"));

        let in_content_type =
            b"<meta http-equiv=\"content-type\" content=\"text/html; charset=windows-1252\"><p>caf\xe9";
        assert!(decode_html(in_content_type, None).contains("café"));
    }

    #[test]
    fn what_the_response_declared_wins_over_what_the_page_declares() {
        // The header is the outer statement and the standard gives it precedence, so a page
        // whose own tag disagrees is decoded the way it was served.
        let body = b"<meta charset=\"utf-8\">caf\xe9";
        assert_eq!(
            decode_html(body, Some("windows-1252")),
            "<meta charset=\"utf-8\">café"
        );
    }

    #[test]
    fn a_byte_order_mark_wins_over_every_declaration() {
        let mut body = vec![0xef, 0xbb, 0xbf];
        body.extend_from_slice("café".as_bytes());
        assert_eq!(decode_html(&body, Some("windows-1252")), "café");
    }

    /// Every one of these carries the word before the real declaration, and all of them
    /// occur on pages nobody wrote to be hostile.
    #[test]
    fn a_stray_mention_of_the_word_does_not_decide_the_encoding() {
        for ahead in [
            "<link rel=stylesheet href=\"/s.css?charset=utf8\">",
            "<!-- charset -->",
            "<meta name=description content=\"charset explained\">",
            "<meta charset=\"not-an-encoding\">",
        ] {
            let mut body = ahead.as_bytes().to_vec();
            body.extend_from_slice(b"<meta charset=\"windows-1252\"><title>caf\xe9</title>");
            assert!(decode_html(&body, None).contains("café"), "after {ahead}");
        }
    }

    #[test]
    fn a_declaration_that_names_nothing_falls_through_rather_than_failing() {
        assert_eq!(decode_html(b"hello", Some("not-an-encoding")), "hello");
        assert_eq!(
            decode_html(b"<meta charset=\"\">caf\xc3\xa9", None),
            "<meta charset=\"\">café"
        );
        assert_eq!(decode_html(b"charset", None), "charset");
        assert_eq!(decode_html(b"", None), "");
    }

    #[test]
    fn a_declaration_past_the_prescan_window_is_not_looked_for() {
        let mut body = vec![b' '; PRESCAN_BYTES];
        body.extend_from_slice(b"<meta charset=\"windows-1252\">caf\xe9");
        assert!(decode_html(&body, None).contains('\u{fffd}'));
    }
}
