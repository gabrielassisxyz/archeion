//! What a site's `robots.txt` refuses, matched the way RFC 9309 says to match it.
//!
//! The file itself is read and parsed by the crawl engine, once per crawl, and this module
//! only re-decides which rule applies to a URL. That split is deliberate. The engine's own
//! matcher handles a pattern whose only wildcard is the last character and nothing else: a
//! `Disallow: /p/*/comment/*` loses its trailing `*` and becomes the literal prefix
//! `/p/*/comment/`, which no path a site serves begins with, so every matching page is
//! fetched and archived against the site's wishes. The engine's second implementation, behind
//! its `regex` feature, compiles the pattern as a regular expression, which is a different
//! language that happens to share the character `*`: it does not match that path either, it
//! matches unanchored so `Disallow: /subscribe` starts refusing `/x/subscribe`, and it drops
//! `Allow` lines entirely.
//!
//! So this owns the two rules the acceptance of a URL turns on, and nothing else about the
//! file: how one pattern matches one path, and which of two matching patterns wins.

use url::Url;

/// One `Allow:` or `Disallow:` line of a group.
pub(super) struct Rule {
    /// The pattern exactly as the file wrote it, wildcards and anchor included, because its
    /// length as written is what decides a conflict with another matching rule.
    pattern: String,
    /// Whether a URL this matches may be fetched.
    allowed: bool,
}

impl Rule {
    /// `pattern` is expected undecoded, exactly as `robots.txt` spelled it: normalizing it
    /// here, once, is what lets every caller, the vendored parser's grouping and every test
    /// in this file alike, hand over a raw rule value without knowing the representation the
    /// matcher below needs.
    pub(super) fn new(pattern: String, allowed: bool) -> Self {
        let pattern = normalize_percent_encoding(&pattern);
        Self {
            // A `Disallow:` with nothing after it is the one line that means the opposite of
            // the word on it: RFC 9309 reads an empty pattern as refusing nothing. It still
            // matches every path, at length zero, so any other rule in the group outranks it.
            allowed: allowed || pattern.is_empty(),
            pattern,
        }
    }
}

/// One group of a `robots.txt` file: the agents its `User-agent:` lines name, and the rules
/// stated under them.
pub(super) struct Group {
    pub(super) agents: Vec<String>,
    pub(super) rules: Vec<Rule>,
}

/// The rules of the one group that governs this crawler, ready to answer about a URL.
pub(super) struct RobotRules {
    rules: Vec<Rule>,
}

impl RobotRules {
    /// What a site that stated no rules gets: a `robots.txt` that answered 404, one whose
    /// body could not be read, or a host that never answered at all.
    pub(super) fn everything_allowed() -> Self {
        Self { rules: Vec::new() }
    }

    /// The rules that govern `user_agent`, chosen the way RFC 9309 chooses them: every group
    /// naming this crawler's product token if the file has any, the `*` groups otherwise, and
    /// nothing else. A file with neither states nothing about this crawler.
    ///
    /// One kind of group, not the union of the two. A file that names this crawler has already
    /// said what it wants from it, and reading the general rules on top of the specific ones
    /// would let `User-agent: *` overrule a line written for this crawler by name.
    ///
    /// Within a kind the groups are combined rather than raced, because a file is free to
    /// state the same agent twice and the second group is as much about this crawler as the
    /// first: taking whichever came first would archive everything the other one refused. A
    /// named group carrying no rules still counts as the file having named this crawler, which
    /// is why an empty result is not the same question as no named group at all.
    pub(super) fn for_agent(groups: Vec<Group>, user_agent: &str) -> Self {
        let token = product_token(user_agent);
        let mut named: Option<Vec<Rule>> = None;
        let mut general: Vec<Rule> = Vec::new();
        for group in groups {
            if group
                .agents
                .iter()
                .any(|agent| agent.eq_ignore_ascii_case(&token))
            {
                named.get_or_insert_default().extend(group.rules);
            } else if group.agents.iter().any(|agent| agent == "*") {
                general.extend(group.rules);
            }
        }
        Self {
            rules: named.unwrap_or(general),
        }
    }

    /// Whether this crawler may fetch `url`.
    ///
    /// A URL that does not parse is allowed, which is the direction everything on this path
    /// fails in: refusing what cannot be read would silently return less of a site than the
    /// run found, and the addresses reaching here have already been through the engine.
    pub(super) fn allows(&self, url: &str) -> bool {
        let Some(path) = path_and_query(url) else {
            return true;
        };
        let mut longest_refusal: Option<usize> = None;
        let mut longest_permission: Option<usize> = None;
        for rule in &self.rules {
            if !pattern_matches(&rule.pattern, &path) {
                continue;
            }
            let longest = if rule.allowed {
                &mut longest_permission
            } else {
                &mut longest_refusal
            };
            if longest.is_none_or(|known| rule.pattern.len() > known) {
                *longest = Some(rule.pattern.len());
            }
        }
        match (longest_refusal, longest_permission) {
            // RFC 9309: the longest pattern wins, and a tie goes to the permission, so a
            // site that spells the same path both ways is read as allowing it.
            (Some(refusal), Some(permission)) => permission >= refusal,
            (Some(_), None) => false,
            _ => true,
        }
    }
}

/// The name a `User-agent:` line would have to carry to be about this crawler: everything
/// before the version, lowercased, per RFC 9309's product token.
fn product_token(user_agent: &str) -> String {
    user_agent
        .split('/')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase()
}

/// What a pattern is matched against: the path and, when there is one, the query. A rule
/// like `Disallow: /*?` says nothing at all about a URL read as its path alone, and rules
/// aimed at a query string are ordinary on real sites.
fn path_and_query(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let raw = match parsed.query() {
        Some(query) => format!("{}?{query}", parsed.path()),
        None => parsed.path().to_owned(),
    };
    Some(normalize_percent_encoding(&raw))
}

/// The representation RFC 9309 section 2.2.2 compares in, applied to one side of a
/// comparison at a time: every octet outside ASCII, and every ASCII octet in RFC 3986's
/// reserved set, stays (or becomes) percent-encoded; a percent-encoded ASCII octet outside
/// that reserved set is decoded to the literal character it names. A literal ASCII character
/// that was never percent-encoded to begin with is left exactly as written, which is what
/// keeps `*` and `$` meaningful as this module's own operators once normalization has run:
/// only a `%2A` or a `%24` goes through this untouched, never a bare `*` or `$`.
fn normalize_percent_encoding(value: &str) -> String {
    fn is_unreserved(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
    }

    let bytes = value.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let decoded = bytes.get(i + 1..i + 3).and_then(|hex| {
                std::str::from_utf8(hex)
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            });
            if let Some(byte) = decoded {
                if is_unreserved(byte) {
                    out.push(byte as char);
                } else {
                    out.push_str(&format!("%{byte:02X}"));
                }
                i += 3;
                continue;
            }
        }
        // A raw byte above ASCII is one half (or more) of a multi-byte UTF-8 character that
        // was written into the file literally rather than escaped; percent-encoding it here,
        // one octet at a time, is what makes it compare equal to the same character spelled
        // with `%XX` triplets, which is the only spelling a URI's own path ever carries it in.
        if bytes[i].is_ascii() {
            out.push(bytes[i] as char);
        } else {
            out.push_str(&format!("%{:02X}", bytes[i]));
        }
        i += 1;
    }
    out
}

/// Whether one pattern matches one path.
///
/// A pattern is a prefix match with two operators: `*` stands for any run of characters, and
/// a `$` at the very end holds the match to the end of the path. A `$` anywhere else is an
/// ordinary character, which is why it is stripped as a suffix rather than treated as the
/// anchor a regular expression would read it as.
///
/// Each literal between two wildcards is taken at the earliest place it occurs. That needs no
/// backtracking and loses no match: a wildcard absorbs anything, so consuming a literal as
/// early as possible leaves the longest remainder for everything after it, and a match that
/// exists under any other placement exists under this one.
fn pattern_matches(pattern: &str, path: &str) -> bool {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(head) => (head, true),
        None => (pattern, false),
    };
    let mut literals = pattern.split('*');
    let opening = literals.next().unwrap_or_default();
    let Some(mut rest) = path.strip_prefix(opening) else {
        return false;
    };
    let mut literals = literals.peekable();
    while let Some(literal) = literals.next() {
        if literals.peek().is_none() {
            // The closing literal of an anchored pattern has to sit at the end of the path;
            // an unanchored one only has to appear somewhere after everything before it.
            return if anchored {
                rest.ends_with(literal)
            } else {
                rest.contains(literal)
            };
        }
        match rest.find(literal) {
            Some(at) => rest = &rest[at + literal.len()..],
            None => return false,
        }
    }
    // No wildcard at all: the whole pattern was the opening literal, already consumed above.
    !anchored || rest.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(lines: &[(&str, bool)]) -> RobotRules {
        RobotRules::for_agent(
            vec![Group {
                agents: vec!["*".to_owned()],
                rules: lines
                    .iter()
                    .map(|(pattern, allowed)| Rule::new((*pattern).to_owned(), *allowed))
                    .collect(),
            }],
            "archeion/0.1.0 (+https://example.test)",
        )
    }

    /// The defect this module exists for, in the spelling the bug was measured on.
    #[test]
    fn a_disallow_with_an_interior_wildcard_refuses_the_paths_it_names() {
        let rules = rules(&[("/p/*/comment/*", false)]);
        assert!(!rules.allows("https://example.test/p/an-essay/comment/298986227"));
        assert!(!rules.allows("https://example.test/p/another/comment/1"));
        assert!(rules.allows("https://example.test/p/an-essay"));
        assert!(rules.allows("https://example.test/comment/1"));
    }

    /// The other half of the same measurement: the rules that already worked keep working,
    /// so the wildcard is not bought with a regression on the ordinary case.
    #[test]
    fn a_plain_prefix_still_refuses_what_starts_with_it_and_nothing_else() {
        let rules = rules(&[("/subscribe", false), ("/action/", false)]);
        assert!(!rules.allows("https://example.test/subscribe"));
        assert!(!rules.allows("https://example.test/subscribe?utm=1"));
        assert!(!rules.allows("https://example.test/action/follow"));
        assert!(rules.allows("https://example.test/"));
        // A prefix is not a substring, which is where the engine's regex implementation
        // goes wrong in the opposite direction.
        assert!(rules.allows("https://example.test/x/subscribe"));
    }

    #[test]
    fn a_rule_ending_in_a_dollar_matches_the_end_of_the_path_and_not_a_prefix() {
        let rules = rules(&[("/*.pdf$", false)]);
        assert!(!rules.allows("https://example.test/papers/one.pdf"));
        assert!(rules.allows("https://example.test/papers/one.pdf.html"));
        assert!(rules.allows("https://example.test/papers/one.pdfx"));
    }

    #[test]
    fn a_dollar_that_is_not_the_last_character_is_an_ordinary_one() {
        let rules = rules(&[("/price$list", false)]);
        assert!(!rules.allows("https://example.test/price$list/one"));
        assert!(rules.allows("https://example.test/pricelist"));
    }

    /// The protocol gives `*` and `$` meaning inside a pattern and gives nothing else any, so a
    /// character a regular expression would read as syntax is an ordinary character here.
    ///
    /// What this pins is this matcher, not the dependency. Nothing here reaches the engine, and
    /// adding its `regex` feature leaves every test in this file green. A wholesale swap to
    /// regular expressions is caught two cases up, by the `$` that this code does branch on;
    /// what is left for this one is a partial translation that gives `*` and `$` their meaning
    /// and forgets to escape the rest. The literal after a wildcard is where that would show,
    /// since that one is matched by `contains` rather than by stripping a prefix.
    ///
    /// The last assertion is the one that is not about metacharacters at all: without it a
    /// matcher that refused everything would satisfy the three above.
    #[test]
    fn a_regular_expression_metacharacter_is_an_ordinary_character() {
        assert!(!pattern_matches("/a.c", "/abc"));
        assert!(!pattern_matches("/p/*/a.c", "/p/x/abc"));
        assert!(!pattern_matches("/p+", "/ppq"));
        assert!(pattern_matches("/p/*/a.c", "/p/x/a.c"));
    }

    #[test]
    fn the_longer_of_two_matching_patterns_decides() {
        let rules = rules(&[("/p/", false), ("/p/keep/", true)]);
        assert!(!rules.allows("https://example.test/p/anything"));
        assert!(rules.allows("https://example.test/p/keep/one"));
    }

    #[test]
    fn a_permission_wins_a_tie_with_a_refusal_of_the_same_length() {
        let rules = rules(&[("/p/one", false), ("/p/one", true)]);
        assert!(rules.allows("https://example.test/p/one"));
    }

    #[test]
    fn a_refusal_of_everything_leaves_nothing_fetchable() {
        let rules = rules(&[("/", false)]);
        assert!(!rules.allows("https://example.test/"));
        assert!(!rules.allows("https://example.test/anything"));
    }

    /// `Disallow:` with an empty value is the line that reads as its own opposite, and it
    /// has to lose to every rule that names an actual path.
    #[test]
    fn an_empty_disallow_refuses_nothing_and_outranks_nothing() {
        assert!(rules(&[("", false)]).allows("https://example.test/anything"));
        let alongside = rules(&[("", false), ("/private", false)]);
        assert!(!alongside.allows("https://example.test/private"));
        assert!(alongside.allows("https://example.test/public"));
    }

    #[test]
    fn a_rule_written_about_the_query_is_matched_against_the_query() {
        let rules = rules(&[("/*?", false)]);
        assert!(!rules.allows("https://example.test/search?q=1"));
        assert!(rules.allows("https://example.test/search"));
    }

    #[test]
    fn a_group_naming_this_crawler_is_the_one_that_governs_it() {
        let rules = RobotRules::for_agent(
            vec![
                Group {
                    agents: vec!["*".to_owned()],
                    rules: vec![Rule::new("/".to_owned(), false)],
                },
                Group {
                    agents: vec!["archeion".to_owned()],
                    rules: vec![Rule::new("/private".to_owned(), false)],
                },
            ],
            "archeion/0.1.0 (+https://example.test)",
        );
        assert!(rules.allows("https://example.test/public"));
        assert!(!rules.allows("https://example.test/private"));
    }

    /// A file is free to state the same agent twice, in whatever spelling, and both groups are
    /// about this crawler. Reading only the first refuses `/first` and archives `/second`, in
    /// plain sight of a line saying not to.
    #[test]
    fn every_group_naming_this_crawler_is_read_and_not_only_the_first() {
        let rules = RobotRules::for_agent(
            vec![
                Group {
                    agents: vec!["archeion".to_owned()],
                    rules: vec![Rule::new("/first".to_owned(), false)],
                },
                Group {
                    agents: vec!["*".to_owned()],
                    rules: vec![Rule::new("/general".to_owned(), false)],
                },
                Group {
                    agents: vec!["Archeion".to_owned()],
                    rules: vec![Rule::new("/second".to_owned(), false)],
                },
            ],
            "archeion/0.1.0 (+https://example.test)",
        );
        assert!(!rules.allows("https://example.test/first"));
        assert!(!rules.allows("https://example.test/second"));
        // Combined with each other, not with the general group, which still does not reach a
        // file that has named this crawler.
        assert!(rules.allows("https://example.test/general"));
    }

    /// Precedence is decided over the combined rules rather than inside whichever group a
    /// pattern was written in, so an `Allow` in one group outranks a shorter `Disallow` in
    /// another exactly as it would if the site had written both under one heading.
    #[test]
    fn a_permission_in_one_group_outranks_a_shorter_refusal_in_another() {
        let rules = RobotRules::for_agent(
            vec![
                Group {
                    agents: vec!["archeion".to_owned()],
                    rules: vec![Rule::new("/p/".to_owned(), false)],
                },
                Group {
                    agents: vec!["archeion".to_owned()],
                    rules: vec![Rule::new("/p/keep/".to_owned(), true)],
                },
            ],
            "archeion/0.1.0 (+https://example.test)",
        );
        assert!(!rules.allows("https://example.test/p/anything"));
        assert!(rules.allows("https://example.test/p/keep/one"));
    }

    /// Naming this crawler and stating nothing under it is a file saying it refuses this
    /// crawler nothing, which is not the same as a file that never mentioned it: the general
    /// rules meant for everyone else stay out of it.
    #[test]
    fn a_group_naming_this_crawler_and_stating_no_rule_still_keeps_the_general_group_out() {
        let rules = RobotRules::for_agent(
            vec![
                Group {
                    agents: vec!["archeion".to_owned()],
                    rules: Vec::new(),
                },
                Group {
                    agents: vec!["*".to_owned()],
                    rules: vec![Rule::new("/".to_owned(), false)],
                },
            ],
            "archeion/0.1.0 (+https://example.test)",
        );
        assert!(rules.allows("https://example.test/anything"));
    }

    /// The same combination on the general side. No crawl reaches this, since the engine's
    /// parser keeps one `*` group and drops the rest before this is ever called, but a caller
    /// building groups itself would otherwise meet the defect above under another name.
    #[test]
    fn every_general_group_is_read_when_no_group_names_this_crawler() {
        let rules = RobotRules::for_agent(
            vec![
                Group {
                    agents: vec!["*".to_owned()],
                    rules: vec![Rule::new("/first".to_owned(), false)],
                },
                Group {
                    agents: vec!["*".to_owned()],
                    rules: vec![Rule::new("/second".to_owned(), false)],
                },
            ],
            "archeion/0.1.0 (+https://example.test)",
        );
        assert!(!rules.allows("https://example.test/first"));
        assert!(!rules.allows("https://example.test/second"));
        assert!(rules.allows("https://example.test/third"));
    }

    /// The general group is what a file that has never heard of this crawler leaves it, and
    /// it is read whichever side of the named groups it was written on.
    #[test]
    fn a_file_that_names_other_crawlers_leaves_this_one_the_general_group() {
        let rules = RobotRules::for_agent(
            vec![
                Group {
                    agents: vec!["gptbot".to_owned()],
                    rules: vec![Rule::new("/".to_owned(), false)],
                },
                Group {
                    agents: vec!["*".to_owned()],
                    rules: vec![Rule::new("/private".to_owned(), false)],
                },
            ],
            "archeion/0.1.0 (+https://example.test)",
        );
        assert!(rules.allows("https://example.test/public"));
        assert!(!rules.allows("https://example.test/private"));
    }

    #[test]
    fn a_file_with_no_group_this_crawler_answers_to_states_nothing_about_it() {
        let rules = RobotRules::for_agent(
            vec![Group {
                agents: vec!["gptbot".to_owned()],
                rules: vec![Rule::new("/".to_owned(), false)],
            }],
            "archeion/0.1.0 (+https://example.test)",
        );
        assert!(rules.allows("https://example.test/anything"));
    }

    #[test]
    fn a_site_that_stated_no_rules_refuses_nothing() {
        assert!(RobotRules::everything_allowed().allows("https://example.test/anything"));
    }

    /// A wildcard between two literals that repeat: the earliest placement of each literal
    /// is what the matcher takes, and this is the shape that would expose it going wrong.
    #[test]
    fn a_pattern_whose_literals_repeat_in_the_path_still_matches() {
        assert!(pattern_matches("/a*bc*c$", "/abcc"));
        assert!(pattern_matches("/a*bb*b$", "/abbb"));
        assert!(!pattern_matches("/a*b*bc$", "/abc"));
    }

    #[test]
    fn a_url_that_does_not_parse_is_not_refused() {
        assert!(rules(&[("/", false)]).allows("not a url"));
    }

    /// RFC 9309 section 2.2.2's under-refusal case: a non-ASCII octet stays percent-encoded
    /// on both sides of the comparison, whichever spelling the rule and the request each
    /// happened to use, rather than the rule's spelling winning only when it matches the
    /// request's byte for byte.
    #[test]
    fn a_literal_and_a_percent_encoded_spelling_of_a_non_ascii_path_are_both_governed() {
        let literal_rule = rules(&[("/café", false)]);
        assert!(!literal_rule.allows("https://example.test/café"));
        assert!(!literal_rule.allows("https://example.test/caf%C3%A9"));

        let encoded_rule = rules(&[("/caf%C3%A9", false)]);
        assert!(!encoded_rule.allows("https://example.test/café"));
        assert!(!encoded_rule.allows("https://example.test/caf%C3%A9"));
    }

    #[test]
    fn an_encoded_unreserved_character_compares_equal_to_its_decoded_spelling() {
        let rules = rules(&[("/%41bc", false)]);
        assert!(!rules.allows("https://example.test/Abc"));
        assert!(!rules.allows("https://example.test/%41bc"));
    }

    /// The over-refusal case: `%2A` and `%24` in a rule are the octets `*` and `$` name, not
    /// the operators, so they must not start matching what the literal operators match.
    #[test]
    fn an_encoded_reserved_wildcard_or_anchor_character_stays_literal() {
        let wildcard_rule = rules(&[("/a*c", false)]);
        assert!(!wildcard_rule.allows("https://example.test/abc"));

        let escaped_wildcard_rule = rules(&[("/a%2Ac", false)]);
        assert!(!escaped_wildcard_rule.allows("https://example.test/a%2Ac"));
        assert!(escaped_wildcard_rule.allows("https://example.test/abc"));

        let anchor_rule = rules(&[("/x$", false)]);
        assert!(!anchor_rule.allows("https://example.test/x"));
        assert!(anchor_rule.allows("https://example.test/xy"));

        let escaped_anchor_rule = rules(&[("/x%24", false)]);
        assert!(!escaped_anchor_rule.allows("https://example.test/x%24"));
        assert!(escaped_anchor_rule.allows("https://example.test/x"));
    }
}
