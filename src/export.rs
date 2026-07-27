use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use pulldown_cmark::{Event, LinkType, Parser, Tag, TagEnd};
use thiserror::Error;
use url::Url;

use crate::CanonicalUrl;
use crate::metadata::PageMetadata;
use crate::readability::Article;
use crate::storage::{Archive, Asset, Capture, CaptureId, ContentHash, Item, ItemId, StorageError};

const SLUG_MAX_BYTES: usize = 80;
const ID_PREFIX_BYTES: usize = 12;
const ASSET_DIRECTORY: &str = "assets";

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
    canonical_url: String,
    host: String,
    path: PathBuf,
    captured_at: Timestamp,
    date: String,
    slug: String,
    item_prefix: String,
    capture_id: String,
    front_matter: FrontMatter,
    body: String,
    assets: Vec<ExportedAsset>,
    unreadable: Vec<String>,
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

struct ExportedAsset {
    filename: String,
    hash: ContentHash,
}

struct RewrittenAssets {
    assets: Vec<ExportedAsset>,
    unreadable: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkdownDestinationKind {
    Link,
    Image,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MarkdownDestination {
    kind: MarkdownDestinationKind,
    destination: String,
    span: Range<usize>,
}

struct ActiveMarkdownDestination {
    kind: MarkdownDestinationKind,
    destination: String,
    span: Range<usize>,
    label_content_end: usize,
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
    let mut notes = Vec::new();
    for item in walk.items {
        let captures = archive.list_captures(&item.canonical_url)?;
        let selected: Vec<CaptureId> = if options.all_captures {
            captures
        } else {
            captures.into_iter().rev().take(1).collect()
        };
        for capture_id in selected {
            match export_note(archive, &item, &capture_id) {
                Ok(Some(mut note)) => {
                    note.path = unique_note_path(&note, &mut used);
                    report.unreadable.extend(note.unreadable.iter().cloned());
                    notes.push(note);
                }
                Ok(None) => {}
                Err(error) => report.unreadable.push(format!(
                    "{} capture {}: {error}",
                    item.canonical_url, capture_id
                )),
            }
        }
    }
    let link_targets = link_targets_by_url(&notes);
    for note in &mut notes {
        rewrite_note_links(&link_targets, &mut note.body, &note.path);
    }
    for note in &notes {
        write_note(archive, destination, note)?;
        report.notes_written += 1;
    }
    report.notes_written += write_host_indexes(destination, &notes)?;
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
    let mut note = note_from(
        item,
        capture_id,
        &capture.fetched_at,
        metadata.as_ref(),
        article,
    );
    let rewritten = rewrite_note_assets(archive, &capture, &mut note.body);
    note.assets = rewritten.assets;
    note.unreadable = rewritten.unreadable;
    Ok(Some(note))
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
        canonical_url: item.canonical_url.to_string(),
        host: item.canonical_url.host_dir().to_owned(),
        path: PathBuf::new(),
        captured_at: *fetched_at,
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
        assets: Vec::new(),
        unreadable: Vec::new(),
    }
}

fn link_targets_by_url(notes: &[ExportNote]) -> HashMap<String, PathBuf> {
    let mut targets: HashMap<String, usize> = HashMap::new();
    for (index, note) in notes.iter().enumerate() {
        match targets.get(&note.canonical_url) {
            Some(existing) if notes[*existing].captured_at >= note.captured_at => {}
            _ => {
                targets.insert(note.canonical_url.clone(), index);
            }
        }
    }
    targets
        .into_iter()
        .map(|(url, index)| (url, notes[index].path.clone()))
        .collect()
}

fn rewrite_note_links(targets: &HashMap<String, PathBuf>, markdown: &mut String, note_path: &Path) {
    let replacements = markdown_destinations(markdown)
        .into_iter()
        .filter_map(|destination| {
            if destination.kind != MarkdownDestinationKind::Link {
                return None;
            }
            let Ok(target_url) = CanonicalUrl::parse(&destination.destination) else {
                return None;
            };
            let target_path = targets.get(target_url.as_str())?;
            Some((
                destination.span,
                relative_markdown_path(note_path, target_path),
            ))
        })
        .collect();

    apply_markdown_replacements(markdown, replacements);
}

fn rewrite_note_assets(
    archive: &Archive,
    capture: &Capture,
    markdown: &mut String,
) -> RewrittenAssets {
    let assets_by_url = assets_by_url(capture);
    let mut replacements = Vec::new();
    let mut copied = HashSet::new();
    let mut exported = Vec::new();
    let mut unreadable = Vec::new();

    for destination in markdown_destinations(markdown) {
        if destination.kind != MarkdownDestinationKind::Image {
            continue;
        }
        let Some(asset) = assets_by_url.get(&asset_url_key(&destination.destination)) else {
            continue;
        };
        if !(200..300).contains(&asset.status) {
            continue;
        }
        let Some(extension) = image_extension(asset.media_type.as_deref()) else {
            continue;
        };
        let filename = format!("{}.{}", asset.body.sha256.as_str(), extension);
        if copied.insert(filename.clone()) {
            match archive.read_body(&asset.body.sha256) {
                Ok(_) => {}
                Err(error) => {
                    unreadable.push(format!(
                        "capture {} asset {}: {error}",
                        capture.id, asset.body.sha256
                    ));
                    copied.remove(&filename);
                    continue;
                }
            };
            exported.push(ExportedAsset {
                filename: filename.clone(),
                hash: asset.body.sha256.clone(),
            });
        }
        replacements.push((destination.span, format!("../{ASSET_DIRECTORY}/{filename}")));
    }

    apply_markdown_replacements(markdown, replacements);
    RewrittenAssets {
        assets: exported,
        unreadable,
    }
}

fn assets_by_url(capture: &Capture) -> HashMap<String, &Asset> {
    let mut assets = HashMap::new();
    for asset in &capture.assets {
        assets.insert(asset_url_key(&asset.requested_url), asset);
        assets.insert(asset_url_key(&asset.final_url), asset);
    }
    assets
}

fn asset_url_key(address: &str) -> String {
    match Url::parse(address) {
        Ok(mut url) => {
            url.set_fragment(None);
            url.to_string()
        }
        Err(_) => address.to_owned(),
    }
}

fn image_extension(media_type: Option<&str>) -> Option<&'static str> {
    let media_type = media_type?.split(';').next()?.trim().to_ascii_lowercase();
    match media_type.as_str() {
        "image/avif" => Some("avif"),
        "image/gif" => Some("gif"),
        "image/jpeg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/svg+xml" => Some("svg"),
        "image/webp" => Some("webp"),
        _ => None,
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

fn write_note(archive: &Archive, destination: &Path, note: &ExportNote) -> Result<(), ExportError> {
    write_assets(archive, destination, &note.assets)?;

    let host = destination.join(&note.host);
    fs::create_dir_all(&host).map_err(|source| ExportError::Io {
        path: host.clone(),
        source,
    })?;

    let path = destination.join(&note.path);

    fs::write(&path, note_text(&note.front_matter, &note.body)).map_err(|source| ExportError::Io {
        path: path.clone(),
        source,
    })
}

fn write_assets(
    archive: &Archive,
    destination: &Path,
    assets: &[ExportedAsset],
) -> Result<(), ExportError> {
    if assets.is_empty() {
        return Ok(());
    }
    let root = destination.join(ASSET_DIRECTORY);
    fs::create_dir_all(&root).map_err(|source| ExportError::Io {
        path: root.clone(),
        source,
    })?;
    for asset in assets {
        let path = root.join(&asset.filename);
        let bytes = archive.read_body(&asset.hash)?;
        fs::write(&path, &bytes).map_err(|source| ExportError::Io {
            path: path.clone(),
            source,
        })?;
    }
    Ok(())
}

fn unique_note_path(note: &ExportNote, used: &mut HashSet<PathBuf>) -> PathBuf {
    for name in [
        format!("{}-{}.md", note.date, note.slug),
        format!("{}-{}-{}.md", note.date, note.slug, note.item_prefix),
        format!("{}-{}-{}.md", note.date, note.slug, note.capture_id),
    ] {
        let path = Path::new(&note.host).join(name);
        if used.insert(path.clone()) {
            return path;
        }
    }
    unreachable!("capture ids are unique within an export")
}

fn write_host_indexes(destination: &Path, notes: &[ExportNote]) -> Result<usize, ExportError> {
    let mut by_host: HashMap<&str, Vec<&ExportNote>> = HashMap::new();
    for note in notes {
        by_host.entry(&note.host).or_default().push(note);
    }

    for (host, host_notes) in &mut by_host {
        host_notes.sort_by(|left, right| {
            left.date
                .cmp(&right.date)
                .then_with(|| left.canonical_url.cmp(&right.canonical_url))
        });
        let mut index = format!("# {host}\n\n");
        for note in host_notes {
            let filename = note
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("export note paths are UTF-8 filenames");
            index.push_str("- [");
            index.push_str(filename);
            index.push_str("](");
            index.push_str(filename);
            index.push_str(")\n");
        }
        let path = destination.join(host).join("index.md");
        fs::write(&path, index).map_err(|source| ExportError::Io {
            path: path.clone(),
            source,
        })?;
    }

    Ok(by_host.len())
}

fn relative_markdown_path(from_note: &Path, target_note: &Path) -> String {
    let from_parent: Vec<_> = from_note
        .parent()
        .map(|path| path.iter().collect())
        .unwrap_or_default();
    let target_components: Vec<_> = target_note.iter().collect();
    let common = from_parent
        .iter()
        .zip(&target_components)
        .take_while(|(left, right)| left == right)
        .count();

    let mut relative = PathBuf::new();
    for _ in common..from_parent.len() {
        relative.push("..");
    }
    for component in &target_components[common..] {
        relative.push(component);
    }
    markdown_path(&relative)
}

fn markdown_path(path: &Path) -> String {
    path.iter()
        .map(|component| {
            component
                .to_str()
                .expect("export paths are generated as UTF-8")
        })
        .collect::<Vec<_>>()
        .join("/")
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

fn markdown_destinations(markdown: &str) -> Vec<MarkdownDestination> {
    let mut destinations = Vec::new();
    let mut active = Vec::new();
    for (event, range) in Parser::new(markdown).into_offset_iter() {
        match event {
            Event::Start(Tag::Link {
                link_type: LinkType::Inline,
                dest_url,
                ..
            }) => {
                active.push(ActiveMarkdownDestination {
                    kind: MarkdownDestinationKind::Link,
                    destination: dest_url.to_string(),
                    label_content_end: range.start,
                    span: range,
                });
            }
            Event::Start(Tag::Image {
                link_type: LinkType::Inline,
                dest_url,
                ..
            }) => {
                active.push(ActiveMarkdownDestination {
                    kind: MarkdownDestinationKind::Image,
                    destination: dest_url.to_string(),
                    label_content_end: range.start,
                    span: range,
                });
            }
            Event::End(TagEnd::Link) => {
                if active
                    .last()
                    .is_some_and(|destination| destination.kind == MarkdownDestinationKind::Link)
                    && let Some(destination) = active.pop()
                    && let Some(span) = inline_destination_span(
                        markdown,
                        destination.span.clone(),
                        destination.label_content_end,
                    )
                {
                    if let Some(parent) = active.last_mut() {
                        parent.label_content_end =
                            parent.label_content_end.max(destination.span.end);
                    }
                    destinations.push(MarkdownDestination {
                        kind: MarkdownDestinationKind::Link,
                        destination: destination.destination,
                        span,
                    });
                }
            }
            Event::End(TagEnd::Image) => {
                if active
                    .last()
                    .is_some_and(|destination| destination.kind == MarkdownDestinationKind::Image)
                    && let Some(destination) = active.pop()
                    && let Some(span) = inline_destination_span(
                        markdown,
                        destination.span.clone(),
                        destination.label_content_end,
                    )
                {
                    if let Some(parent) = active.last_mut() {
                        parent.label_content_end =
                            parent.label_content_end.max(destination.span.end);
                    }
                    destinations.push(MarkdownDestination {
                        kind: MarkdownDestinationKind::Image,
                        destination: destination.destination,
                        span,
                    });
                }
            }
            _ => {
                if let Some(destination) = active.last_mut()
                    && destination.span.start < range.start
                    && range.end < destination.span.end
                {
                    destination.label_content_end = destination.label_content_end.max(range.end);
                }
            }
        }
    }
    destinations
}

fn apply_markdown_replacements(
    markdown: &mut String,
    mut replacements: Vec<(Range<usize>, String)>,
) {
    replacements.sort_by_key(|(span, _)| span.start);
    let mut previous_end = 0;
    replacements.retain(|(span, _)| {
        if span.start < previous_end {
            return false;
        }
        previous_end = span.end;
        true
    });
    for (span, replacement) in replacements.into_iter().rev() {
        markdown.replace_range(span, &replacement);
    }
}

fn inline_destination_span(
    markdown: &str,
    span: Range<usize>,
    label_content_end: usize,
) -> Option<Range<usize>> {
    let inline = markdown.get(span.clone())?;
    let bytes = inline.as_bytes();
    let mut at = label_content_end.checked_sub(span.start)?;
    while bytes.get(at).is_some_and(|byte| *byte != b']') {
        at += 1;
    }
    at += 1;
    if bytes.get(at) != Some(&b'(') {
        return None;
    }
    at += 1;
    at = skip_ascii_whitespace(bytes, at);
    if bytes.get(at) == Some(&b'<') {
        let start = at + 1;
        let end = bytes[start..].iter().position(|byte| *byte == b'>')? + start;
        return Some(span.start + start..span.start + end);
    }

    let start = at;
    let mut depth = 0usize;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'(' => {
                depth += 1;
                at += 1;
            }
            b')' if depth == 0 => {
                return (start < at).then_some(span.start + start..span.start + at);
            }
            b')' => {
                depth -= 1;
                at += 1;
            }
            byte if byte.is_ascii_whitespace() => {
                return (start < at).then_some(span.start + start..span.start + at);
            }
            _ => at += 1,
        }
    }
    None
}

fn skip_ascii_whitespace(bytes: &[u8], mut at: usize) -> usize {
    while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
        at += 1;
    }
    at
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

    #[test]
    fn markdown_destinations_are_rewritten_only_inside_real_inline_images() {
        let mut markdown = "`literal [text](https://example.com/x) here`\n\n\
                            ```\n\
                            see [text](https://example.com/x) in code\n\
                            ```\n\n\
                            ![this](https://example.com/a \"the title\")\n\n\
                            ![wiki](https://en.wikipedia.org/wiki/Foo_\\(bar\\))\n\n\
                            ![spaced](<https://example.com/a b>)\n\n\
                            ![a \\[b\\] c](https://example.com/a)\n\n\
                            ![a `](x)` b](https://example.com/code.png)\n\n\
                            [not an image](https://example.com/a)\n"
            .to_owned();

        let replacements = markdown_destinations(&markdown)
            .into_iter()
            .filter_map(|destination| {
                if destination.kind != MarkdownDestinationKind::Image {
                    return None;
                }
                let replacement = match destination.destination.as_str() {
                    "https://example.com/a" => "../assets/a.png",
                    "https://en.wikipedia.org/wiki/Foo_(bar)" => "../assets/wiki.jpg",
                    "https://example.com/a b" => "../assets/spaced.webp",
                    "https://example.com/code.png" => "../assets/code.png",
                    other => panic!("unexpected destination {other}"),
                };
                Some((destination.span, replacement.to_owned()))
            })
            .collect();

        apply_markdown_replacements(&mut markdown, replacements);

        assert_eq!(
            markdown,
            "`literal [text](https://example.com/x) here`\n\n\
             ```\n\
             see [text](https://example.com/x) in code\n\
             ```\n\n\
             ![this](../assets/a.png \"the title\")\n\n\
             ![wiki](../assets/wiki.jpg)\n\n\
             ![spaced](<../assets/spaced.webp>)\n\n\
             ![a \\[b\\] c](../assets/a.png)\n\n\
             ![a `](x)` b](../assets/code.png)\n\n\
             [not an image](https://example.com/a)\n"
        );
    }
}
