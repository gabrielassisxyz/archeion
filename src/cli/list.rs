//! `archeion list`: what a collection already holds, one line per item.

use std::error::Error;
use std::fmt::Write as _;
use std::path::PathBuf;

use archeion::storage::{Archive, Item};
use serde::Serialize;

use super::{damaged_archive, warn, write_stdout};

#[derive(Debug, Serialize)]
struct ListRow {
    canonical_url: String,
    captures: usize,
    last_captured_at: Option<String>,
    has_article: bool,
}

#[derive(Debug, Default)]
struct ListedArchive {
    rows: Vec<ListRow>,
    unreadable: Vec<String>,
}

pub fn list(path: PathBuf, json: bool) -> Result<(), Box<dyn Error>> {
    let archive = Archive::open_existing(path)?;
    let walk = archive.walk()?;
    let mut listed = list_rows(&archive, &walk.items);
    listed
        .unreadable
        .extend(walk.unreadable.iter().map(ToString::to_string));

    let output = if json {
        json_lines(&listed.rows)?
    } else {
        table(&listed.rows)
    };
    write_stdout(&output)?;
    warn(listed.unreadable.iter().cloned());

    if listed.unreadable.is_empty() {
        Ok(())
    } else {
        Err(damaged_archive(listed.unreadable.len()).into())
    }
}

fn list_rows(archive: &Archive, items: &[Item]) -> ListedArchive {
    let mut listed = ListedArchive {
        rows: Vec::with_capacity(items.len()),
        unreadable: Vec::new(),
    };
    for item in items {
        let captures = match archive.list_captures(&item.canonical_url) {
            Ok(captures) => captures,
            Err(error) => {
                listed
                    .unreadable
                    .push(format!("{}: {error}", item.canonical_url));
                continue;
            }
        };
        let has_article = match captures.last() {
            Some(capture) => match archive.read_article(&item.canonical_url, capture) {
                Ok(article) => article.is_some(),
                Err(error) => {
                    listed
                        .unreadable
                        .push(format!("{}: {error}", item.canonical_url));
                    continue;
                }
            },
            None => false,
        };
        listed.rows.push(ListRow {
            canonical_url: item.canonical_url.to_string(),
            captures: captures.len(),
            last_captured_at: captures.last().map(|_| item.last_captured_at.to_string()),
            has_article,
        });
    }
    listed
}

/// One object per line rather than one array, so a reader can consume an archive of any size
/// without holding all of it, and `grep` stays a legitimate way to ask a question of it.
fn json_lines(rows: &[ListRow]) -> Result<String, serde_json::Error> {
    let mut output = String::new();
    for row in rows {
        output.push_str(&serde_json::to_string(row)?);
        output.push('\n');
    }
    Ok(output)
}

fn table(rows: &[ListRow]) -> String {
    let url_width = rows
        .iter()
        .map(|row| row.canonical_url.len())
        .max()
        .unwrap_or(0)
        .max("URL".len());
    let captures_width = rows
        .iter()
        .map(|row| row.captures.to_string().len())
        .max()
        .unwrap_or(0)
        .max("CAPTURES".len());
    let last_width = rows
        .iter()
        .filter_map(|row| row.last_captured_at.as_ref())
        .map(String::len)
        .max()
        .unwrap_or(0)
        .max("LAST_CAPTURED_AT".len());

    let mut output = String::new();
    writeln!(
        output,
        "{:<url_width$}  {:>captures_width$}  {:<last_width$}  ARTICLE",
        "URL", "CAPTURES", "LAST_CAPTURED_AT"
    )
    .expect("writing to a string cannot fail");
    for row in rows {
        writeln!(
            output,
            "{:<url_width$}  {:>captures_width$}  {:<last_width$}  {}",
            row.canonical_url,
            row.captures,
            row.last_captured_at.as_deref().unwrap_or(""),
            if row.has_article { "yes" } else { "no" }
        )
        .expect("writing to a string cannot fail");
    }
    output
}
