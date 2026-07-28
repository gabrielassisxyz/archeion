//! Policy for strings that become Markdown links or image descriptions.

use url::Url;

/// The schemes an archived document may still point at.
///
/// The list is short because a link destination is the one thing in an article that a reader
/// may act on, and it is written by whoever wrote the page. `mailto` is here because it is
/// ordinary in prose and inert; everything absent, `data` and `vbscript` and `javascript`
/// among them, is a destination that exists to run rather than to be read.
const READABLE_SCHEMES: [&str; 3] = ["http", "https", "mailto"];

/// A destination worth keeping, spelled absolutely, or nothing when the link has to go.
pub(super) fn readable_destination(destination: &str, base: Option<&Url>) -> Option<String> {
    let destination = destination.trim();
    if destination.starts_with('#') {
        return (!destination.chars().any(char::is_whitespace)).then(|| destination.to_owned());
    }
    let absolute = match Url::parse(destination) {
        Ok(absolute) => absolute,
        Err(url::ParseError::RelativeUrlWithoutBase) => base?.join(destination).ok()?,
        Err(_) => return None,
    };
    READABLE_SCHEMES
        .contains(&absolute.scheme())
        .then(|| absolute.to_string())
}

/// An image's description, reduced to something that can only be a description.
///
/// It is written into `![...](...)` by a converter that escapes almost nothing there, so four
/// things have to leave: `]` ends the description early and hands the rest of the line to a
/// destination nothing here screened, `[` opens another, a trailing `\` escapes the `]` that
/// would have ended it, and a line break ends the line the whole construct sits on and lets a
/// description write the document's own structure. That last one is exactly what collapsing
/// whitespace in the title was for, and a description is the same kind of string: page
/// controlled, not prose, and with one job that survives being flattened.
pub(super) fn only_a_description(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(['[', ']', '\\'], "")
}
