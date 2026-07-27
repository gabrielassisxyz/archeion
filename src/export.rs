use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use thiserror::Error;
use url::Url;

use crate::CanonicalUrl;
use crate::metadata::PageMetadata;
use crate::readability::Article;
use crate::storage::{Archive, CaptureId, Item, ItemId, StorageError};

const SLUG_MAX_BYTES: usize = 80;
const ID_PREFIX_BYTES: usize = 12;

#[derive(Debug, Clone, Copy, Default)]
pub struct ExportOptions {
    pub all_captures: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ExportReport {
    pub notes_written: usize,
    pub unreadable: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ExportError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path} exists and is not an empty directory")]
    DestinationNotEmpty { path: PathBuf },
    #[error("{path} exists and is not a directory")]
    DestinationNotDirectory { path: PathBuf },
}

struct ExportNote {
    host: String,
    date: String,
    slug: String,
    item_prefix: String,
    capture_id: String,
    front_matter: FrontMatter,
    body: String,
}

struct FrontMatter {
    title: Option<String>,
    canonical_url: String,
    captured_at: String,
    published_at: Option<String>,
    author: Option<String>,
    site_name: Option<String>,
    language: Option<String>,
    word_count: usize,
    excerpt: Option<String>,
}

struct DestinationState {
    created_root: bool,
}

pub fn export_archive(
    archive: &Archive,
    destination: impl AsRef<Path>,
    options: ExportOptions,
) -> Result<ExportReport, ExportError> {
    let destination = destination.as_ref();
    let state = prepare_destination(destination)?;
    match write_export(archive, destination, options) {
        Ok(report) => Ok(report),
        Err(error) => {
            if state.created_root {
                let _ = fs::remove_dir_all(destination);
            }
            Err(error)
        }
    }
}

fn write_export(
    archive: &Archive,
    destination: &Path,
    options: ExportOptions,
) -> Result<ExportReport, ExportError> {
    let walk = archive.walk()?;
    let mut report = ExportReport {
        notes_written: 0,
        unreadable: walk.unreadable.iter().map(ToString::to_string).collect(),
    };
    let mut used = HashSet::new();
    for item in walk.items {
        let captures = archive.list_captures(&item.canonical_url)?;
        let selected: Vec<CaptureId> = if options.all_captures {
            captures
        } else {
            captures.into_iter().rev().take(1).collect()
        };
        for capture_id in selected {
            match export_note(archive, &item, &capture_id) {
                Ok(Some(note)) => {
                    write_note(destination, note, &mut used)?;
                    report.notes_written += 1;
                }
                Ok(None) => {}
                Err(error) => report.unreadable.push(format!(
                    "{} capture {}: {error}",
                    item.canonical_url, capture_id
                )),
            }
        }
    }
    Ok(report)
}

fn export_note(
    archive: &Archive,
    item: &Item,
    capture_id: &CaptureId,
) -> Result<Option<ExportNote>, ExportError> {
    let Some(article) = archive.read_article(&item.canonical_url, capture_id)? else {
        return Ok(None);
    };
    let capture = archive.read_capture(&item.canonical_url, capture_id)?;
    let metadata = archive.read_metadata(&item.canonical_url, capture_id)?;
    Ok(Some(note_from(
        item,
        capture_id,
        &capture.fetched_at,
        metadata.as_ref(),
        article,
    )))
}

fn note_from(
    item: &Item,
    capture_id: &CaptureId,
    fetched_at: &Timestamp,
    metadata: Option<&PageMetadata>,
    article: Article,
) -> ExportNote {
    let title = metadata.and_then(|metadata| metadata.title.as_ref());
    let item_prefix = item.id.as_str()[..ID_PREFIX_BYTES].to_owned();
    ExportNote {
        host: item.canonical_url.host_dir().to_owned(),
        date: fetched_at.strftime("%Y-%m-%d").to_string(),
        slug: slug_for(
            &item.canonical_url,
            title.map(|title| title.value.as_str()),
            &item.id,
        ),
        item_prefix,
        capture_id: capture_id.to_string(),
        front_matter: FrontMatter {
            title: title.map(|title| title.value.clone()),
            canonical_url: item.canonical_url.to_string(),
            captured_at: fetched_at.to_string(),
            published_at: metadata
                .and_then(|metadata| metadata.published_at.as_ref())
                .map(|date| {
                    date.timestamp
                        .as_ref()
                        .map_or_else(|| date.raw.clone(), ToString::to_string)
                }),
            author: metadata
                .and_then(|metadata| metadata.author.as_ref())
                .map(|author| author.value.clone()),
            site_name: metadata
                .and_then(|metadata| metadata.site_name.as_ref())
                .map(|site| site.value.clone()),
            language: metadata
                .and_then(|metadata| metadata.language.as_ref())
                .map(|language| language.value.clone()),
            word_count: article.record.word_count,
            excerpt: article.record.excerpt.clone(),
        },
        body: article.markdown,
    }
}

fn prepare_destination(destination: &Path) -> Result<DestinationState, ExportError> {
    match fs::metadata(destination) {
        Ok(metadata) if !metadata.is_dir() => Err(ExportError::DestinationNotDirectory {
            path: destination.to_owned(),
        }),
        Ok(_) if destination_is_empty(destination)? => Ok(DestinationState {
            created_root: false,
        }),
        Ok(_) => Err(ExportError::DestinationNotEmpty {
            path: destination.to_owned(),
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(destination).map_err(|source| ExportError::Io {
                path: destination.to_owned(),
                source,
            })?;
            Ok(DestinationState { created_root: true })
        }
        Err(source) => Err(ExportError::Io {
            path: destination.to_owned(),
            source,
        }),
    }
}

fn destination_is_empty(destination: &Path) -> Result<bool, ExportError> {
    let mut entries = fs::read_dir(destination).map_err(|source| ExportError::Io {
        path: destination.to_owned(),
        source,
    })?;
    Ok(entries
        .next()
        .transpose()
        .map_err(|source| ExportError::Io {
            path: destination.to_owned(),
            source,
        })?
        .is_none())
}

fn write_note(
    destination: &Path,
    note: ExportNote,
    used: &mut HashSet<PathBuf>,
) -> Result<(), ExportError> {
    let host = destination.join(&note.host);
    fs::create_dir_all(&host).map_err(|source| ExportError::Io {
        path: host.clone(),
        source,
    })?;

    let path = unique_note_path(&host, &note, used);

    fs::write(&path, note_text(&note.front_matter, &note.body)).map_err(|source| ExportError::Io {
        path: path.clone(),
        source,
    })
}

fn unique_note_path(host: &Path, note: &ExportNote, used: &mut HashSet<PathBuf>) -> PathBuf {
    for name in [
        format!("{}-{}.md", note.date, note.slug),
        format!("{}-{}-{}.md", note.date, note.slug, note.item_prefix),
        format!("{}-{}-{}.md", note.date, note.slug, note.capture_id),
    ] {
        let path = host.join(name);
        if used.insert(path.clone()) {
            return path;
        }
    }
    unreachable!("capture ids are unique within an export")
}

fn note_text(front_matter: &FrontMatter, body: &str) -> String {
    let mut note = String::new();
    note.push_str("---\n");
    push_optional_string(&mut note, "title", front_matter.title.as_deref());
    push_string(&mut note, "canonical_url", &front_matter.canonical_url);
    push_string(&mut note, "captured_at", &front_matter.captured_at);
    push_optional_string(
        &mut note,
        "published_at",
        front_matter.published_at.as_deref(),
    );
    push_optional_string(&mut note, "author", front_matter.author.as_deref());
    push_optional_string(&mut note, "site_name", front_matter.site_name.as_deref());
    push_optional_string(&mut note, "language", front_matter.language.as_deref());
    note.push_str(&format!("word_count: {}\n", front_matter.word_count));
    push_optional_string(&mut note, "excerpt", front_matter.excerpt.as_deref());
    note.push_str("---\n\n");
    note.push_str(body);
    note
}

fn push_optional_string(output: &mut String, key: &str, value: Option<&str>) {
    match value {
        Some(value) => push_string(output, key, value),
        None => output.push_str(&format!("{key}: null\n")),
    }
}

fn push_string(output: &mut String, key: &str, value: &str) {
    output.push_str(key);
    output.push_str(": ");
    output.push('"');
    push_escaped_yaml(output, value);
    output.push_str("\"\n");
}

fn push_escaped_yaml(output: &mut String, value: &str) {
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character.is_control() => {
                output.push_str(&format!("\\x{:02x}", character as u32));
            }
            character => output.push(character),
        }
    }
}

fn slug_for(url: &CanonicalUrl, title: Option<&str>, item_id: &ItemId) -> String {
    slugify(title.unwrap_or(""))
        .or_else(|| slug_from_path(url))
        .unwrap_or_else(|| item_id.as_str()[..ID_PREFIX_BYTES].to_owned())
}

fn slug_from_path(url: &CanonicalUrl) -> Option<String> {
    let parsed = Url::parse(url.as_str()).ok()?;
    let decoded = percent_decode_utf8(parsed.path())?;
    slugify(&decoded)
}

fn slugify(value: &str) -> Option<String> {
    let mut slug = String::new();
    let mut previous_was_hyphen = true;
    for byte in value.bytes() {
        let next = match byte {
            b'A'..=b'Z' => Some((byte + 32) as char),
            b'a'..=b'z' | b'0'..=b'9' => Some(byte as char),
            _ => None,
        };
        match next {
            Some(character) => {
                if slug.len() == SLUG_MAX_BYTES {
                    break;
                }
                slug.push(character);
                previous_was_hyphen = false;
            }
            None if !previous_was_hyphen && slug.len() < SLUG_MAX_BYTES => {
                slug.push('-');
                previous_was_hyphen = true;
            }
            None => {}
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    (!slug.is_empty()).then_some(slug)
}

fn percent_decode_utf8(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex_value(*bytes.get(index + 1)?)?;
            let low = hex_value(*bytes.get(index + 2)?)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_fall_back_from_title_to_path_to_item_id() {
        let titled = CanonicalUrl::parse("https://example.com/fallback").expect("valid url");
        let rooted = CanonicalUrl::parse("https://example.com/").expect("valid url");

        assert_eq!(
            slug_for(
                &titled,
                Some("A Title, With Punctuation"),
                &ItemId::of(&titled)
            ),
            "a-title-with-punctuation"
        );
        assert_eq!(
            slug_for(&titled, Some("!!!"), &ItemId::of(&titled)),
            "fallback"
        );
        assert_eq!(
            slug_for(&rooted, Some("!!!"), &ItemId::of(&rooted)),
            &ItemId::of(&rooted).as_str()[..ID_PREFIX_BYTES]
        );
    }

    #[test]
    fn yaml_strings_are_quoted_and_escaped() {
        let mut output = String::new();

        push_string(&mut output, "title", "\"quoted\"\nsecond line\\");

        assert_eq!(output, "title: \"\\\"quoted\\\"\\nsecond line\\\\\"\n");
    }

    #[test]
    fn percent_encoded_paths_are_decoded_before_the_path_slug_is_judged() {
        let spaced = CanonicalUrl::parse("https://example.com/hello%20world").expect("valid url");
        let non_latin =
            CanonicalUrl::parse("https://example.com/%D1%81%D1%82%D0%B0%D1%82%D1%8C%D1%8F")
                .expect("valid url");

        assert_eq!(slug_from_path(&spaced).as_deref(), Some("hello-world"));
        assert_eq!(slug_from_path(&non_latin), None);
    }
}
