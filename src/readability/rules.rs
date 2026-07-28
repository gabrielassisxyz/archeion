//! Telling the extractor what the scoring pass cannot work out on its own.
//!
//! The heuristic stays the default and the baseline. This is the escape hatch for the sites it
//! cannot reach, and it is expressed as data rather than as code because the operator is the one
//! who hits the exotic site: waiting for a release to fix one extraction is the worst ergonomics
//! this tool could have. `docs/readability.md` has the file, the two directives and the shapes
//! that motivated them.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use dom_query::Matcher;
use serde::Deserialize;

use crate::canonical_url::CanonicalUrl;

/// How large a rule file may be.
///
/// A rule is a host, a sentence and a handful of selectors, so this holds thousands of them. It
/// is here because the file sits at a path anything on this machine can write to, and the ceiling
/// costs one `stat` while its absence costs whatever the largest file on the disk is.
const MAX_RULES_BYTES: u64 = 256 * 1024;

/// A rule the extractor did not use, and why.
///
/// Every one of these degrades to the heuristic instead of ending the capture. A run that refused
/// to archive anything because a configuration file has a comma in the wrong place would trade a
/// worse extraction for no archive at all, and the response is the part that cannot be recovered
/// later. It is reported rather than counted for the same reason as `UnreadableArticle`: the
/// point of saying it is that someone goes and edits the file named here.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{origin}: {reason}")]
pub struct UnusedRules {
    /// The rule file, so the message names a path rather than a fact about it.
    pub origin: String,
    pub reason: String,
}

/// What one host's markup needs said about it.
///
/// Both directives are lists of CSS selectors, and both are deliberately narrow: they cover the
/// shapes that real sites have demanded so far. The corpus of rule formats everyone else grew,
/// `ftr-site-config` above all, reached twenty directives against real sites, and every one of
/// them arrived because a site asked for it. Adding one here follows the same rule.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteRule {
    /// Why this host needs to be told. Read by a person, never by the extractor: a rule whose
    /// reason is not written down is a rule nobody can decide to delete a year from now.
    #[serde(default)]
    pub why: Option<String>,
    /// Where the article is. The first selector that matches anything wins, and what it matched
    /// becomes the whole document, so a later selector is an alternative spelling and not an
    /// addition. A host that declares this and a page that has none of it is not an article: the
    /// operator made a positive statement about where the prose lives on this site, and falling
    /// back to the heuristic would switch the rule off on exactly the pages that motivated it.
    #[serde(default)]
    pub body: Vec<String>,
    /// What is furniture inside it. Applied after `body`, so a selector only has to describe the
    /// article's own surroundings once the page around it is already gone.
    #[serde(default)]
    pub strip: Vec<String>,
}

/// The rules for every host that has any.
///
/// Keyed by the host as canonicalization spells it, which is the same string the archive files
/// the item under: `www.` folded away and the case flattened, so one rule covers every spelling
/// of one site. The match is exact, and a subdomain is a different host until a site asks for
/// something else.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SiteRules {
    hosts: BTreeMap<String, SiteRule>,
}

/// The rule that applies to a page, under the name a record will carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MatchedRule<'a> {
    pub(super) host: &'a str,
    pub(super) rule: &'a SiteRule,
}

/// The file as it is written, wrapped so the top level has somewhere to grow that is not a host
/// name. A flat map would make the first directive that is not per-host collide with a site.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleFile {
    hosts: BTreeMap<String, SiteRule>,
}

impl SiteRules {
    /// Reads the rules that sit beside an archive.
    ///
    /// An absent file is the ordinary case and says nothing. Everything else answers with
    /// whatever rules were usable and reports the rest, so a broken file costs the extractions it
    /// would have improved rather than the capture.
    pub fn read(path: &Path) -> (Self, Vec<UnusedRules>) {
        let origin = path.display().to_string();
        let unused = |reason: String| {
            (
                Self::default(),
                vec![UnusedRules {
                    origin: origin.clone(),
                    reason,
                }],
            )
        };

        // On the link rather than through it, and before the file is opened, on the same terms as
        // an item record in `storage::walk`: this is a path outside the program's own writes, and
        // by the time `/dev/zero` is being read the choice not to read it has been made.
        let shape = match fs::symlink_metadata(path) {
            Ok(shape) => shape,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return (Self::default(), Vec::new());
            }
            Err(error) => return unused(format!("could not be read: {error}")),
        };
        if !shape.is_file() {
            return unused("a rule file is a regular file, and this is not one".to_owned());
        }
        if shape.len() > MAX_RULES_BYTES {
            return unused(format!(
                "is {} bytes, over the {MAX_RULES_BYTES} byte ceiling for a rule file",
                shape.len()
            ));
        }
        match fs::read_to_string(path) {
            Ok(declared) => Self::parse(&declared, &origin),
            Err(error) => unused(format!("could not be read: {error}")),
        }
    }

    /// Reads the rules out of the text of a rule file.
    ///
    /// `origin` names where the text came from, and is what a reported problem points at.
    pub fn parse(declared: &str, origin: &str) -> (Self, Vec<UnusedRules>) {
        let file: RuleFile = match serde_json::from_str(declared) {
            Ok(file) => file,
            Err(error) => {
                return (
                    Self::default(),
                    vec![UnusedRules {
                        origin: origin.to_owned(),
                        reason: format!("is not a readable rule file: {error}"),
                    }],
                );
            }
        };

        let mut hosts = BTreeMap::new();
        let mut unused = Vec::new();
        for (host, rule) in file.hosts {
            // A selector that no parser will read drops its host rather than its file or itself.
            // Dropping the file would let one typo silently switch off every other rule; dropping
            // the selector alone would leave a rule doing something the operator did not write.
            match unreadable_selector(&rule) {
                Some(selector) => unused.push(UnusedRules {
                    origin: origin.to_owned(),
                    reason: format!("{host} is ignored, {selector:?} is not a selector"),
                }),
                None => {
                    hosts.insert(host, rule);
                }
            }
        }
        (Self { hosts }, unused)
    }

    /// The rule for the page at this address, if its host has one.
    pub(super) fn for_url(&self, url: &str) -> Option<MatchedRule<'_>> {
        let canonical = CanonicalUrl::parse(url).ok()?;
        let (host, rule) = self.hosts.get_key_value(canonical.host_dir())?;
        Some(MatchedRule { host, rule })
    }

    /// Whether anything at all was declared, so a caller can skip work it has no rules for.
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }
}

/// The first selector in a rule that no CSS parser will read.
///
/// Compiled here, once, at the moment the file is read, rather than where it is used. A selector
/// arrives from a file this program did not write, and the parser behind it answers a bad one
/// with an error only if it is asked: the same string reaches `Document::select`, which panics.
fn unreadable_selector(rule: &SiteRule) -> Option<&str> {
    rule.body
        .iter()
        .chain(&rule.strip)
        .find(|selector| Matcher::new(selector).is_err())
        .map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(declared: &str) -> (SiteRules, Vec<UnusedRules>) {
        SiteRules::parse(declared, "a test")
    }

    #[test]
    fn a_rule_is_found_under_every_spelling_of_its_host() {
        let (rules, unused) = parse(
            r#"{"hosts": {"lwn.net": {"body": ["div.ArticleText"], "strip": ["blockquote.ad"]}}}"#,
        );
        assert!(unused.is_empty(), "{unused:?}");

        for spelling in [
            "https://lwn.net/Articles/1/",
            "https://www.lwn.net/Articles/1/",
            "https://LWN.NET/Articles/1/",
            "http://lwn.net/",
        ] {
            let matched = rules
                .for_url(spelling)
                .unwrap_or_else(|| panic!("no rule for {spelling}"));
            assert_eq!(matched.host, "lwn.net");
            assert_eq!(matched.rule.body, ["div.ArticleText"]);
        }
    }

    /// A rule keys on a host and not on a site, so a subdomain is a different site until one asks
    /// for something else. Guessing the other way would apply a newspaper's article rule to its
    /// unrelated jobs board.
    #[test]
    fn a_rule_does_not_reach_a_host_it_does_not_name() {
        let (rules, _) = parse(r#"{"hosts": {"lwn.net": {"strip": [".ad"]}}}"#);

        assert!(rules.for_url("https://old.lwn.net/Articles/1/").is_none());
        assert!(rules.for_url("https://example.com/").is_none());
        // Not an address this archive files anything under, so it has no host to key on.
        assert!(rules.for_url("file:///etc/passwd").is_none());
    }

    /// The whole point of the tolerant read. A file nobody can parse leaves the extractor exactly
    /// where it was, and says so, because the alternative is a run that archives nothing.
    #[test]
    fn a_file_that_is_not_a_rule_file_degrades_to_the_heuristic() {
        for declared in [
            "",
            "{",
            "[]",
            r#"{"lwn.net": {"strip": [".ad"]}}"#,
            r#"{"hosts": {"lwn.net": {"stripe": [".ad"]}}}"#,
            r#"{"hosts": {"lwn.net": {"strip": ".ad"}}}"#,
        ] {
            let (rules, unused) = parse(declared);
            assert!(rules.is_empty(), "{declared} was read as {rules:?}");
            assert_eq!(unused.len(), 1, "{declared}");
            assert_eq!(unused[0].origin, "a test");
        }
    }

    /// A typo in one host costs that host, and nothing else. Dropping the whole file would let
    /// one bad selector switch off every rule beside it, which is the failure this degradation is
    /// supposed to prevent rather than cause.
    #[test]
    fn a_selector_no_parser_will_read_costs_its_host_and_no_other() {
        let (rules, unused) = parse(
            r#"{"hosts": {
                 "broken.example": {"strip": ["div..oops"]},
                 "fine.example": {"strip": [".ad"]}
               }}"#,
        );

        assert!(rules.for_url("https://broken.example/a").is_none());
        assert!(rules.for_url("https://fine.example/a").is_some());
        assert_eq!(unused.len(), 1, "{unused:?}");
        assert!(unused[0].reason.contains("broken.example"), "{unused:?}");
        assert!(unused[0].reason.contains("div..oops"), "{unused:?}");
    }

    #[test]
    fn a_rule_file_that_is_not_there_says_nothing() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let (rules, unused) = SiteRules::read(&directory.path().join("extraction-rules.json"));

        assert!(rules.is_empty());
        assert!(unused.is_empty(), "{unused:?}");
    }

    #[test]
    fn a_rule_file_is_read_off_the_disk() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("extraction-rules.json");
        fs::write(
            &path,
            r#"{"hosts": {"lwn.net": {"why": "the pitch is prose", "strip": ["blockquote.ad"]}}}"#,
        )
        .expect("the rule file is written");

        let (rules, unused) = SiteRules::read(&path);

        assert!(unused.is_empty(), "{unused:?}");
        let matched = rules
            .for_url("https://lwn.net/Articles/1/")
            .expect("a rule");
        assert_eq!(matched.rule.why.as_deref(), Some("the pitch is prose"));
        assert_eq!(matched.rule.strip, ["blockquote.ad"]);
    }

    /// The ceiling is on the file rather than on what parsing it would cost, so it is checked
    /// before the bytes are read rather than after.
    #[test]
    fn a_rule_file_past_the_ceiling_is_refused_before_it_is_read() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("extraction-rules.json");
        fs::write(&path, "x".repeat(MAX_RULES_BYTES as usize + 1)).expect("the file is written");

        let (rules, unused) = SiteRules::read(&path);

        assert!(rules.is_empty());
        assert_eq!(unused.len(), 1, "{unused:?}");
        assert!(unused[0].reason.contains("ceiling"), "{unused:?}");
    }

    #[test]
    fn a_directory_where_a_rule_file_belongs_is_refused_rather_than_read() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("extraction-rules.json");
        fs::create_dir(&path).expect("the directory is created");

        let (rules, unused) = SiteRules::read(&path);

        assert!(rules.is_empty());
        assert_eq!(unused.len(), 1, "{unused:?}");
        assert!(unused[0].reason.contains("regular file"), "{unused:?}");
    }
}
