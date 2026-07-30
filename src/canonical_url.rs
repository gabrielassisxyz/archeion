//! The canonical form of a URL: the address the archive keys an item on, and the rules
//! that reduce every spelling of a page to that one address.
//!
//! The rules and the reasoning behind each of them are in `docs/canonicalization.md`.

use std::fmt;

use serde::{Deserialize, Serialize};
use url::{Host, Url};

/// A URL that has been through the canonicalization rules, which is the only way one can
/// be built. Two spellings of the same page produce equal values here, and that equality
/// is what makes an item one item: everything downstream addresses by this type rather
/// than by a string, so no call site can accidentally key on a raw URL.
///
/// It settles identity, not what gets fetched. A fetch uses the URL as it was found, and
/// the capture record keeps both the requested and the final URL, so a rule that rewrites
/// the address here can never make a page unreachable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CanonicalUrl {
    url: Url,
    host_dir: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InvalidCanonicalUrl {
    #[error("{url} cannot be parsed as a URL: {reason}")]
    Unparseable { url: String, reason: String },
    #[error("{url} is a {scheme} URL, and the archive captures over http and https")]
    UnsupportedScheme { url: String, scheme: String },
    #[error("{url} has no host, so it names nothing that can be archived")]
    Hostless { url: String },
    #[error("{host} is not a host this archive will create a directory for")]
    UnsafeHost { host: String },
}

/// Query parameters that identify a campaign or a click and never the resource. They are
/// dropped so that the same page shared through five channels stays one item. The list is
/// deliberately short and hardcoded: every entry is unambiguous, and a configurable list
/// would be a setting with no second user to set it.
const TRACKING_PARAMETERS: &[&str] = &[
    "fbclid", "gclid", "dclid", "msclkid", "twclid", "igshid", "mc_cid", "mc_eid",
];

/// What an escaped ampersand leaves glued to the front of the parameter behind it. An
/// attribute value spells `&` as `&amp;`, which is what the HTML standard asks a page for,
/// and nothing between the href and here decodes it; the second spelling is the same link
/// with its semicolon percent-encoded on top of the escape. Both come off real sites.
///
/// Matched case insensitively, since `&AMP;` is a reference the standard defines as well and
/// the hex digits of a percent-encoded byte carry no case.
const ESCAPED_AMPERSAND_TAILS: &[&str] = &["amp;", "amp%3b"];

impl CanonicalUrl {
    pub fn parse(url: &str) -> Result<Self, InvalidCanonicalUrl> {
        let mut parsed = Url::parse(url).map_err(|reason| InvalidCanonicalUrl::Unparseable {
            url: url.to_owned(),
            reason: reason.to_string(),
        })?;

        // Anything else is either a link this archive cannot capture or a scheme that only
        // reads the local machine, and both arrive from remote pages by the thousand.
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(InvalidCanonicalUrl::UnsupportedScheme {
                url: url.to_owned(),
                scheme: parsed.scheme().to_owned(),
            });
        }

        rewrite_host(&mut parsed)?;
        // Credentials name a requester, not a resource, and an archive is the last place a
        // password should be pinned. Neither call can fail for a URL with a host.
        let _ = parsed.set_username("");
        let _ = parsed.set_password(None);
        // A fragment is resolved by the client against bytes the server already sent, so
        // two URLs differing only there are one fetch and one stored response.
        parsed.set_fragment(None);
        drop_tracking_parameters(&mut parsed);

        let host_dir = match parsed.host() {
            Some(host) => host_directory(&host)?,
            // An http or https URL cannot parse without a host, so this branch guards the
            // invariant rather than reporting a case a caller can produce today.
            None => {
                return Err(InvalidCanonicalUrl::Hostless {
                    url: url.to_owned(),
                });
            }
        };
        Ok(Self {
            url: parsed,
            host_dir,
        })
    }

    pub fn as_str(&self) -> &str {
        self.url.as_str()
    }

    /// The directory the archive groups this item under, so the tree stays readable by
    /// domain instead of being a flat field of hashes.
    pub fn host_dir(&self) -> &str {
        &self.host_dir
    }
}

/// Normalizes the host in place. The scheme, the case of the host, the default port and
/// the dot segments of the path are already settled by `Url::parse`; what it leaves alone
/// is the DNS root dot and the `www` prefix, and both make a second directory for a site
/// that is one site.
fn rewrite_host(url: &mut Url) -> Result<(), InvalidCanonicalUrl> {
    // An address literal is already in its one spelling, and trimming dots off it would
    // be trimming the address itself.
    let Some(Host::Domain(name)) = url.host() else {
        return Ok(());
    };

    let name = name.to_owned();
    let mut stripped = name.trim_end_matches('.');
    // Stripping one label and stopping there is not canonicalization but one step of it:
    // `www.www.example.com` would reduce to a different address on a second pass, and
    // every record read back off disk goes through these rules again. Repeating until the
    // prefix is gone is what makes the result a fixed point.
    while let Some(rest) = stripped.strip_prefix("www.") {
        // Without a dot left, `www` was the site and not a prefix of it: `www.com` is a
        // registrable name, and reducing it to `com` would key on a different host.
        if !rest.contains('.') {
            break;
        }
        stripped = rest;
    }
    if stripped == name {
        return Ok(());
    }

    let stripped = stripped.to_owned();
    url.set_host(Some(&stripped))
        .map_err(|_| InvalidCanonicalUrl::UnsafeHost { host: name })
}

/// A host reaches the filesystem as a directory name, which makes it the one place remote
/// data could climb out of the archive root. Anything outside a conservative set is
/// refused rather than escaped, because an archive has no use for a host that needs it.
fn host_directory(host: &Host<&str>) -> Result<String, InvalidCanonicalUrl> {
    match host {
        Host::Domain(name) => {
            let lowered = name.to_ascii_lowercase();
            let safe = !lowered.is_empty()
                && !lowered.starts_with('.')
                && lowered
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
            if safe {
                Ok(lowered)
            } else {
                Err(InvalidCanonicalUrl::UnsafeHost {
                    host: name.to_string(),
                })
            }
        }
        Host::Ipv4(addr) => Ok(addr.to_string()),
        // Colons are illegal in a filename on Windows and awkward everywhere else, and the
        // bracketed URL form buys nothing in a directory name.
        Host::Ipv6(addr) => Ok(addr.to_string().replace(':', "-")),
    }
}

/// Drops the tracking parameters and sorts what survives.
///
/// The parameters are otherwise kept as the raw text they arrived as rather than decoded and
/// re-encoded. A round trip through key-value pairs rewrites the escaping and turns a
/// valueless `?print` into `?print=`, which would canonicalize a URL into one the server
/// was never asked for. The escaped separator is the one exception, and it is undone before
/// anything reads a name, since until it is the names are not the page's.
fn drop_tracking_parameters(url: &mut Url) {
    let rebuilt = {
        let Some(query) = url.query() else {
            return;
        };
        let mut kept: Vec<&str> = query
            .split('&')
            .map(undo_escaped_ampersand)
            .filter(|parameter| !parameter.is_empty())
            .filter(|parameter| !is_tracking(parameter_name(parameter)))
            .collect();
        // Stable, so repeated names keep the order they arrived in: a server reading both
        // values of `?a=1&a=2` is reading a different request from `?a=2&a=1`.
        kept.sort_by(|left, right| parameter_name(left).cmp(parameter_name(right)));
        (!kept.is_empty()).then(|| kept.join("&"))
    };
    url.set_query(rebuilt.as_deref());
}

/// Removes what an escaped ampersand left on the front of a parameter.
///
/// A query splits on the literal `&`, so a page that wrote its separator the way HTML
/// requires hands every parameter after the first a name it never meant: `amp;utm_medium`
/// rather than `utm_medium`. That name defeats the tracking rules below, and what survives
/// them is sorted and stored as a second address for a page the archive already holds, which
/// is the one thing canonicalization exists to prevent.
///
/// Undone here, at the address, rather than at the href it came off: an address also arrives
/// from a sitemap and from an operator's command line, and a rule about what a URL means
/// belongs where every source of one passes. It stays wrong on the wire either way, since the
/// request is aimed at the URL as it was found and not at this one.
///
/// Stripped in a loop rather than once, because these rules have to be a fixed point: a name
/// escaped twice would shed one layer per pass and name a different address every time a
/// stored record was read back.
fn undo_escaped_ampersand(parameter: &str) -> &str {
    let mut name = parameter;
    loop {
        let stripped = ESCAPED_AMPERSAND_TAILS
            .iter()
            .find_map(|tail| strip_prefix_ignoring_case(name, tail));
        match stripped {
            Some(rest) => name = rest,
            None => return name,
        }
    }
}

fn strip_prefix_ignoring_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

fn parameter_name(parameter: &str) -> &str {
    parameter
        .split_once('=')
        .map_or(parameter, |(name, _)| name)
}

fn is_tracking(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("utm_") || TRACKING_PARAMETERS.contains(&name.as_str())
}

impl TryFrom<String> for CanonicalUrl {
    type Error = InvalidCanonicalUrl;

    fn try_from(url: String) -> Result<Self, Self::Error> {
        Self::parse(&url)
    }
}

impl From<CanonicalUrl> for String {
    fn from(url: CanonicalUrl) -> Self {
        url.url.into()
    }
}

impl fmt::Display for CanonicalUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical(url: &str) -> String {
        CanonicalUrl::parse(url)
            .unwrap_or_else(|error| panic!("{url} should canonicalize: {error}"))
            .as_str()
            .to_owned()
    }

    /// One row per rule, because a rule with no row is a rule nobody can tell was dropped.
    #[test]
    fn every_rule_reduces_a_spelling_to_the_one_address() {
        let rules: &[(&str, &str, &str)] = &[
            (
                "the scheme and the host are compared in lowercase",
                "HTTPS://Example.COM/a",
                "https://example.com/a",
            ),
            (
                "the DNS root dot is not a second host",
                "https://example.com./a",
                "https://example.com/a",
            ),
            (
                "www is the apex under another name",
                "https://www.rust-lang.org/learn",
                "https://rust-lang.org/learn",
            ),
            (
                "a www that is the whole registrable name stays",
                "https://www.com/a",
                "https://www.com/a",
            ),
            (
                "only the www prefix goes, not any subdomain",
                "https://m.example.com/a",
                "https://m.example.com/a",
            ),
            (
                "an internationalized host has one ASCII spelling",
                "https://bücher.example/a",
                "https://xn--bcher-kva.example/a",
            ),
            (
                "the default port is the port",
                "https://example.com:443/a",
                "https://example.com/a",
            ),
            (
                "a port that is not the default is part of the address",
                "https://example.com:8443/a",
                "https://example.com:8443/a",
            ),
            (
                "an empty path is the root path",
                "https://example.com",
                "https://example.com/",
            ),
            (
                "dot segments are resolved, not stored",
                "https://example.com/a/b/../c",
                "https://example.com/a/c",
            ),
            (
                "a trailing slash below the root is the server's business, not ours",
                "https://example.com/a/",
                "https://example.com/a/",
            ),
            (
                "a fragment is resolved by the client, not fetched",
                "https://example.com/a#section-2",
                "https://example.com/a",
            ),
            (
                "credentials name a requester, not a resource",
                "https://someone:secret@example.com/a",
                "https://example.com/a",
            ),
            (
                "tracking parameters identify a campaign, not a page",
                "https://example.com/a?utm_source=news&id=7&fbclid=xyz",
                "https://example.com/a?id=7",
            ),
            (
                "a query that was only tracking leaves no question mark",
                "https://example.com/a?utm_source=news",
                "https://example.com/a",
            ),
            (
                "an empty query is no query",
                "https://example.com/a?",
                "https://example.com/a",
            ),
            (
                "parameter order is not part of the address",
                "https://example.com/a?b=2&a=1",
                "https://example.com/a?a=1&b=2",
            ),
            (
                "an ampersand a page had to escape is a separator, not part of the next name",
                "https://example.com/a?id=7&amp;utm_source=news",
                "https://example.com/a?id=7",
            ),
            (
                "a valueless parameter keeps its shape",
                "https://example.com/a?print",
                "https://example.com/a?print",
            ),
            (
                "http and https are different addresses on purpose",
                "http://example.com/a",
                "http://example.com/a",
            ),
        ];

        for (rule, input, expected) in rules {
            assert_eq!(canonical(input), *expected, "{rule}");
        }
    }

    /// A record on disk is re-canonicalized when it is read back, so a rule that reaches a
    /// different address on the second pass writes a record this program cannot read, or
    /// one whose stored URL no longer names the directory it sits in.
    #[test]
    fn canonicalizing_a_canonical_url_changes_nothing() {
        for url in [
            "https://www.example.com/a?utm_source=news&b=2&a=1#top",
            "https://example.com./",
            "http://[2001:db8::1]:8080/a",
            "https://www.www.example.com/page",
            "https://www.www.www.co.uk./a?b=2&utm_id=1&a=1#top",
            "https://www.www.com/a",
            "https://example.com/a?id=7&amp;utm_source=news",
            "https://example.com/a?amp%3Bamp%3Bid=7",
        ] {
            let once = canonical(url);
            assert_eq!(canonical(&once), once, "{url}");
        }
    }

    /// A repeated prefix is the case that broke the fixed point: the host must come out of
    /// one pass in the form a second pass would leave it in.
    #[test]
    fn a_repeated_www_is_stripped_all_the_way_down() {
        assert_eq!(
            canonical("https://www.www.www.example.com/a"),
            "https://example.com/a"
        );
        // An empty label behind the prefix leaves a host no directory can be named after,
        // and it is refused here rather than stored as a record nothing can read back.
        assert!(matches!(
            CanonicalUrl::parse("https://www.www..example.com/page"),
            Err(InvalidCanonicalUrl::UnsafeHost { .. })
        ));
    }

    /// The spelling the HTML standard asks an attribute for, which reached these rules
    /// undecoded and filed a second item for a post already archived. Every parameter behind
    /// the escape arrived named `amp;utm_medium` rather than `utm_medium`, so the rules that
    /// drop a campaign never matched and what they left was an address nobody would type.
    #[test]
    fn an_escaped_ampersand_between_parameters_is_still_a_separator() {
        let post = "https://example.substack.com/p/an-essay";
        let bare = CanonicalUrl::parse(&format!(
            "{post}?utm_source=substack&utm_medium=email&utm_content=share&action=share"
        ))
        .expect("valid url");

        for escaped in [
            format!(
                "{post}?utm_source=substack&amp;utm_medium=email&amp;utm_content=share&amp;action=share"
            ),
            // The same link with the semicolon percent-encoded on top of the escape, which is
            // the shape a quarter of the run that found this defect arrived in.
            format!(
                "{post}?utm_source=substack&amp%3Butm_medium=email&amp%3Butm_content=share&amp%3Baction=share"
            ),
            format!(
                "{post}?utm_source=substack&AMP;utm_medium=email&amp%3butm_content=share&amp;action=share"
            ),
        ] {
            let filed = CanonicalUrl::parse(&escaped).expect("valid url");
            assert_eq!(filed, bare, "{escaped}");
            assert_eq!(filed.as_str(), format!("{post}?action=share"));
        }

        // The first parameter carries the tail too, once the tracking parameter that used to
        // sit in front of it has been dropped.
        assert_eq!(
            canonical("https://example.com/s/notebook-philosophy?amp%3Butm_medium=menu"),
            "https://example.com/s/notebook-philosophy"
        );
    }

    /// `amp=1` is a parameter real platforms serve. What is stripped above is the tail of a
    /// character reference, which is the name plus the semicolon that ends it, so a parameter
    /// that merely starts with those three letters is untouched.
    #[test]
    fn a_parameter_actually_named_amp_is_left_alone() {
        assert_eq!(
            canonical("https://example.com/a?amp=1"),
            "https://example.com/a?amp=1"
        );
        assert_eq!(
            canonical("https://example.com/a?amp"),
            "https://example.com/a?amp"
        );
        assert_eq!(
            canonical("https://example.com/a?ampersand=1"),
            "https://example.com/a?ampersand=1"
        );
    }

    #[test]
    fn two_spellings_of_one_page_are_one_url() {
        let spelled_out =
            CanonicalUrl::parse("https://WWW.rust-lang.org.:443/learn?utm_medium=email#top")
                .expect("valid url");
        let plain = CanonicalUrl::parse("https://rust-lang.org/learn").expect("valid url");

        assert_eq!(spelled_out.as_str(), "https://rust-lang.org/learn");
        assert_eq!(spelled_out, plain);
    }

    #[test]
    fn host_directory_lowercases_a_domain() {
        let url = CanonicalUrl::parse("https://Example.COM/a").expect("valid url");
        assert_eq!(url.host_dir(), "example.com");
    }

    #[test]
    fn host_directory_refuses_to_be_a_path() {
        assert!(matches!(
            CanonicalUrl::parse("https://../../etc/passwd"),
            Err(InvalidCanonicalUrl::UnsafeHost { .. })
        ));
        // Trimming the root dot must not be able to trim a host away entirely.
        assert!(matches!(
            CanonicalUrl::parse("http://."),
            Err(InvalidCanonicalUrl::UnsafeHost { .. })
        ));
    }

    #[test]
    fn host_directory_keeps_an_ipv6_address_filename_safe() {
        let url = CanonicalUrl::parse("http://[2001:db8::1]/a").expect("valid url");
        assert_eq!(url.host_dir(), "2001-db8--1");
    }

    #[test]
    fn a_url_the_archive_cannot_fetch_is_refused() {
        assert!(matches!(
            CanonicalUrl::parse("mailto:someone@example.com"),
            Err(InvalidCanonicalUrl::UnsupportedScheme { .. })
        ));
        assert!(matches!(
            CanonicalUrl::parse("file:///etc/passwd"),
            Err(InvalidCanonicalUrl::UnsupportedScheme { .. })
        ));
        assert!(matches!(
            CanonicalUrl::parse("javascript:alert(1)"),
            Err(InvalidCanonicalUrl::UnsupportedScheme { .. })
        ));
        assert!(matches!(
            CanonicalUrl::parse("https://"),
            Err(InvalidCanonicalUrl::Unparseable { .. })
        ));
    }

    #[test]
    fn a_stored_url_is_canonicalized_on_the_way_back_in() {
        let record: CanonicalUrl =
            serde_json::from_str(r#""https://www.example.com/a#top""#).expect("a URL");
        assert_eq!(record.as_str(), "https://example.com/a");
        assert!(serde_json::from_str::<CanonicalUrl>(r#""file:///etc/passwd""#).is_err());
    }
}
