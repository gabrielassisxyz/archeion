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
    let rooted = name.trim_end_matches('.');
    let stripped = match rooted.strip_prefix("www.") {
        // Without a dot left, `www` was the site and not a prefix of it: `www.com` is a
        // registrable name, and reducing it to `com` would key on a different host.
        Some(rest) if rest.contains('.') => rest,
        _ => rooted,
    };
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
/// The parameters are kept as the raw text they arrived as rather than decoded and
/// re-encoded. A round trip through key-value pairs rewrites the escaping and turns a
/// valueless `?print` into `?print=`, which would canonicalize a URL into one the server
/// was never asked for.
fn drop_tracking_parameters(url: &mut Url) {
    let rebuilt = {
        let Some(query) = url.query() else {
            return;
        };
        let mut kept: Vec<&str> = query
            .split('&')
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

    #[test]
    fn canonicalizing_a_canonical_url_changes_nothing() {
        for url in [
            "https://www.example.com/a?utm_source=news&b=2&a=1#top",
            "https://example.com./",
            "http://[2001:db8::1]:8080/a",
        ] {
            let once = canonical(url);
            assert_eq!(canonical(&once), once);
        }
    }

    #[test]
    fn two_spellings_of_one_page_are_one_url() {
        assert_eq!(
            CanonicalUrl::parse("https://WWW.rust-lang.org.:443/learn?utm_medium=email#top"),
            CanonicalUrl::parse("https://rust-lang.org/learn"),
        );
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
        assert!(!url.host_dir().contains(':'));
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
