//! The exit code table in `docs/cli.md` is the reference; `--help` is a second copy of the
//! same information and the one most people actually read. Nothing enforces that a change to
//! either one reaches the other, so this compares the two texts directly: the only way the
//! next drift between them gets caught is by never trusting either side to remember alone.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

fn archeion() -> Command {
    Command::new(env!("CARGO_BIN_EXE_archeion"))
}

/// A code's description is one or more causes. A trailing `, or <last cause>` is the
/// enumeration this project writes when a code can happen for more than one reason; anything
/// else is a single cause, and the whole description is that cause, comma or no comma. There
/// is no branch here that stops comparing partway through a description: a clause added to
/// either side, anywhere, changes what this returns.
fn causes(description: &str) -> Vec<String> {
    let description = description.trim();
    if description.contains(", or ") {
        let mut parts: Vec<String> = description
            .split(", ")
            .map(|part| part.trim().to_string())
            .collect();
        if let Some(last) = parts.last_mut()
            && let Some(rest) = last.strip_prefix("or ")
        {
            *last = rest.to_string();
        }
        parts
    } else {
        vec![description.to_string()]
    }
}

/// Pulls `code -> description` out of the `--help` output's own exit code section, joining a
/// wrapped entry's continuation lines back into the one description they are part of.
fn codes_from_help(help: &str) -> BTreeMap<u32, String> {
    let section = help
        .split_once("Exit codes:\n")
        .expect("--help prints an exit code section")
        .1;
    let mut codes = BTreeMap::new();
    let mut current: Option<(u32, String)> = None;
    for line in section.lines() {
        if line.trim().is_empty() {
            break;
        }
        let trimmed = line.trim_start();
        let leading_digit = trimmed
            .split_whitespace()
            .next()
            .and_then(|token| token.parse::<u32>().ok());
        match leading_digit {
            Some(code) => {
                if let Some((code, description)) = current.take() {
                    codes.insert(code, description);
                }
                let rest = trimmed
                    .split_once(char::is_whitespace)
                    .map_or("", |(_, rest)| rest)
                    .trim()
                    .to_string();
                current = Some((code, rest));
            }
            _ => {
                if let Some((_, description)) = current.as_mut() {
                    description.push(' ');
                    description.push_str(trimmed);
                }
            }
        }
    }
    if let Some((code, description)) = current {
        codes.insert(code, description);
    }
    codes
}

/// Pulls `code -> description` out of the exit code table in `docs/cli.md`, scoped to the
/// `## Exit codes` section so an unrelated table elsewhere in the document can't be mistaken
/// for it.
fn codes_from_docs(doc: &str) -> BTreeMap<u32, String> {
    let section = doc
        .split_once("## Exit codes")
        .expect("docs/cli.md has an Exit codes section")
        .1;
    let section = section.split("\n## ").next().unwrap_or(section);
    let mut codes = BTreeMap::new();
    for line in section.lines() {
        let line = line.trim();
        if !line.starts_with('|') {
            continue;
        }
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        // A row is `| code | description |`, which splits into `["", code, description, ""]`.
        if cells.len() != 4 {
            continue;
        }
        if let Ok(code) = cells[1].parse::<u32>() {
            codes.insert(code, cells[2].to_string());
        }
    }
    codes
}

#[test]
fn help_and_docs_name_the_same_exit_code_causes() {
    let output = archeion()
        .arg("--help")
        .output()
        .expect("archeion --help runs");
    assert!(output.status.success(), "archeion --help must exit 0");
    let help = String::from_utf8(output.stdout).expect("--help output is UTF-8");

    let doc_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/cli.md");
    let doc = std::fs::read_to_string(&doc_path).expect("docs/cli.md is readable");

    let help_codes = codes_from_help(&help);
    let doc_codes = codes_from_docs(&doc);

    assert_eq!(
        help_codes.keys().collect::<Vec<_>>(),
        doc_codes.keys().collect::<Vec<_>>(),
        "--help and docs/cli.md must document the same set of exit codes"
    );

    for (code, doc_description) in &doc_codes {
        let help_description = &help_codes[code];
        assert_eq!(
            causes(help_description),
            causes(doc_description),
            "exit code {code}: --help says {help_description:?}, docs/cli.md says {doc_description:?}"
        );
    }
}
