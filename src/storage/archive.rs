//! The store over an archive directory.
//!
//! The directory is the record, not a cache of one: every write lands as a file whose
//! meaning is readable without this program, and nothing here keeps state a reader would
//! have to trust. An index, when the collection grows enough to need one, is derived from
//! this tree and can be deleted and rebuilt at any time.

use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use jiff::Timestamp;
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::model::{
    Asset, Capture, CaptureId, ContentHash, Item, ItemId, NewAsset, NewCapture, StoredBody,
};
use super::walk::ArchiveWalk;
use crate::canonical_url::CanonicalUrl;
use crate::metadata::PageMetadata;
use crate::readability::{Article, ArticleRecord, RefusedExtraction};

const MARKER_FILE: &str = "archeion.json";
const FORMAT_NAME: &str = "archeion-archive";
const FORMAT_VERSION: u32 = 1;
const MAX_ARTICLE_RECORD_BYTES: u64 = 64 * 1024;
const MAX_ARTICLE_MARKDOWN_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Serialize, serde::Deserialize)]
struct FormatMarker {
    format: String,
    version: u32,
}

/// An article record as it sits on disk: what the extractor produced, plus the address of the
/// document it describes.
///
/// The hash belongs here and not in `ArticleRecord` because it is a fact about the pair of
/// files, which is this layer's concern; the extractor produces one article and has no
/// filesystem to tear. Flattened, so the record reads as one object rather than as fields
/// nested under a wrapper nobody outside this file knows about.
#[derive(Debug, Serialize, serde::Deserialize)]
struct StoredArticle {
    markdown_sha256: ContentHash,
    #[serde(flatten)]
    record: ArticleRecord,
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path}: malformed record: {source}")]
    MalformedRecord {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{path} holds something else, not an Archeion archive")]
    NotAnArchive { path: PathBuf },
    #[error("{path} does not exist")]
    MissingArchive { path: PathBuf },
    #[error("{path} does not hold an Archeion archive")]
    NoArchiveMarker { path: PathBuf },
    #[error("{path} is an archive in format version {found}, this build reads version {readable}")]
    UnreadableFormat {
        path: PathBuf,
        found: u32,
        readable: u32,
    },
    #[error("nothing archived for {url}")]
    NoSuchItem { url: String },
    #[error("{url} has no capture {capture}")]
    NoSuchCapture { url: String, capture: String },
    #[error("body {hash} is referenced by a record but missing from the archive")]
    MissingBody { hash: String },
    #[error("body {hash} does not hash to the name it is stored under")]
    CorruptBody { hash: String },
    #[error("{path}: this record cannot be written as JSON: {source}")]
    UnserializableRecord {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{path} was refused rather than read: {reason}")]
    RefusedRecord { path: PathBuf, reason: &'static str },
}

pub struct Archive {
    root: PathBuf,
}

impl Archive {
    /// Opens the archive at `root`, creating it when the directory is empty or absent. A
    /// directory that holds anything else is refused rather than adopted: pointing this
    /// at the wrong path should fail loudly, not scatter records into it.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, StorageError> {
        let root = root.into();
        let marker_path = root.join(MARKER_FILE);

        match read_optional_json::<FormatMarker>(&marker_path)? {
            Some(marker) if marker.format != FORMAT_NAME => {
                return Err(StorageError::NotAnArchive { path: root });
            }
            Some(marker) if marker.version > FORMAT_VERSION => {
                return Err(StorageError::UnreadableFormat {
                    path: root,
                    found: marker.version,
                    readable: FORMAT_VERSION,
                });
            }
            Some(_) => {}
            None => {
                if directory_has_visible_entries(&root)? {
                    return Err(StorageError::NotAnArchive { path: root });
                }
                write_json(
                    &marker_path,
                    &FormatMarker {
                        format: FORMAT_NAME.to_owned(),
                        version: FORMAT_VERSION,
                    },
                )?;
            }
        }

        Ok(Self { root })
    }

    /// Opens an existing archive without creating a marker when the path is absent or empty.
    ///
    /// Read-only callers use this so a mistyped path does not become a valid empty archive
    /// merely by being listed.
    pub fn open_existing(root: impl Into<PathBuf>) -> Result<Self, StorageError> {
        let root = root.into();
        if !root.exists() {
            return Err(StorageError::MissingArchive { path: root });
        }
        let marker_path = root.join(MARKER_FILE);

        match read_optional_json::<FormatMarker>(&marker_path)? {
            Some(marker) if marker.format != FORMAT_NAME => {
                return Err(StorageError::NotAnArchive { path: root });
            }
            Some(marker) if marker.version > FORMAT_VERSION => {
                return Err(StorageError::UnreadableFormat {
                    path: root,
                    found: marker.version,
                    readable: FORMAT_VERSION,
                });
            }
            Some(_) => {}
            None => return Err(StorageError::NoArchiveMarker { path: root }),
        }

        Ok(Self { root })
    }

    /// Stores the bytes of one subresource and answers with the record that references them.
    ///
    /// It is separate from writing the capture because a subresource is shared: one
    /// stylesheet belongs to every page of a site that links it, and the run that captured
    /// it stores it once and then hands this record to each of those captures. The store
    /// would have deduplicated the bytes either way, since the address is the content, but
    /// only the caller can avoid fetching them twice, and that is the part that costs
    /// somebody else's bandwidth.
    pub fn store_asset(&self, new: &NewAsset) -> Result<Asset, StorageError> {
        Ok(Asset {
            requested_url: new.requested_url.clone(),
            final_url: new.final_url.clone(),
            status: new.status,
            media_type: new.media_type.clone(),
            body: self.write_body(&new.body)?,
        })
    }

    /// Stores one fetch of one item: its bytes, the item record and the capture record.
    ///
    /// The write order is what a run cut short leaves behind, so it is chosen rather than
    /// incidental. The body comes first, because an unreferenced blob costs disk space and
    /// nothing else while a record pointing at absent bytes is broken. The item record
    /// comes next, because it holds the canonical URL that the hashed directory name does
    /// not: a capture stranded without it cannot be read back to the address it came from.
    /// The subresources arrive already stored, by the same rule applied one step earlier.
    pub fn write_capture(&self, new: NewCapture) -> Result<Capture, StorageError> {
        let body = self.write_body(&new.body)?;
        let fingerprint = fingerprint_of(&new, &body.sha256);
        let capture = Capture {
            id: CaptureId::new(new.fetched_at, &fingerprint),
            item_id: ItemId::of(&new.canonical_url),
            requested_url: new.requested_url,
            final_url: new.final_url,
            status: new.status,
            media_type: new.media_type,
            response_headers: new.response_headers,
            body,
            body_truncated: new.body_truncated,
            fetched_at: new.fetched_at,
            assets: new.assets,
            assets_missed: new.assets_missed,
        };

        self.record_item(&new.canonical_url, new.fetched_at)?;
        write_json(
            &self.capture_path(&new.canonical_url, &capture.id),
            &capture,
        )?;
        Ok(capture)
    }

    /// Stores what was extracted from a capture, beside it rather than inside it.
    ///
    /// The capture record is what the archive observed and the only thing that cannot be
    /// recovered; this is a reading of it, and a better extractor will want to replace it.
    /// Keeping the two in separate files is what lets that later pass rewrite every
    /// derived file in the archive without touching a single recorded one, and what makes
    /// deleting the whole derived layer a safe thing to do.
    ///
    /// It is written after the capture for the same reason bodies are written before
    /// records: a run cut short then leaves a capture with no reading of it, which the next
    /// pass can produce, rather than a reading of a capture that was never stored.
    pub fn write_metadata(
        &self,
        url: &CanonicalUrl,
        capture: &CaptureId,
        metadata: &PageMetadata,
    ) -> Result<(), StorageError> {
        write_json(&self.metadata_path(url, capture), metadata)
    }

    /// What was extracted from a capture, or `None`.
    ///
    /// Absent is an ordinary answer and not a broken archive: a capture of an image has
    /// nothing to extract, one written before this existed has nothing stored, and a
    /// derived file that was deliberately deleted is meant to be regenerated.
    pub fn read_metadata(
        &self,
        url: &CanonicalUrl,
        capture: &CaptureId,
    ) -> Result<Option<PageMetadata>, StorageError> {
        read_optional_json(&self.metadata_path(url, capture))
    }

    /// Writes the prose read out of a capture: the Markdown document, and the record beside
    /// it. Both are derived, on the same terms as the metadata above.
    ///
    /// The Markdown is a file of its own rather than a string inside the record. It is the
    /// artifact a person opens and a reader renders, and a JSON string would escape every
    /// newline and quote in it, turning a document meant to outlive this tool into something
    /// only a program can read.
    ///
    /// The record goes last and names the document by its hash, which is what makes the pair
    /// safe to write over. Ordering alone is only enough the first time: over an existing
    /// pair, a write cut between the two files leaves the new prose beside the old record,
    /// both present and both parsing, and every field describing prose that is no longer
    /// there. The hash is what turns that from silent into detectable.
    pub fn write_article(
        &self,
        url: &CanonicalUrl,
        capture: &CaptureId,
        article: &Article,
    ) -> Result<(), StorageError> {
        write_atomically(
            &self.article_markdown_path(url, capture),
            article.markdown.as_bytes(),
        )?;
        write_json(
            &self.article_record_path(url, capture),
            &StoredArticle {
                markdown_sha256: ContentHash::of(article.markdown.as_bytes()),
                record: article.record.clone(),
            },
        )
    }

    /// Writes what an extraction measured about a page it refused to call an article.
    ///
    /// Only the pages the extractor's own rule turned down are written here, never the ones
    /// that simply held no prose. Most of the web is the second kind, and a file for each of
    /// those would bury the few that are worth reviewing under the many that say nothing.
    ///
    /// It is one file rather than a pair, because the point of the refusal is that the
    /// document beside it was not written. The prose is still derivable from the stored
    /// response whenever somebody re-reads these, which is what they are kept for.
    pub fn write_refused_extraction(
        &self,
        url: &CanonicalUrl,
        capture: &CaptureId,
        refused: &RefusedExtraction,
    ) -> Result<(), StorageError> {
        write_json(&self.refused_extraction_path(url, capture), refused)
    }

    /// What an extraction refused to call an article, or `None`.
    pub fn read_refused_extraction(
        &self,
        url: &CanonicalUrl,
        capture: &CaptureId,
    ) -> Result<Option<RefusedExtraction>, StorageError> {
        read_optional_bounded_json(
            &self.refused_extraction_path(url, capture),
            MAX_ARTICLE_RECORD_BYTES,
            "larger than an article record can be",
        )
    }

    /// The prose read out of a capture, or `None`.
    ///
    /// Absent is an ordinary answer: most of the web is not an article, a capture of an image
    /// has no prose, and a derived file that was deliberately deleted is meant to be
    /// regenerated. A half-written pair is absent too, in either direction: a record with no
    /// document, or a record describing a document other than the one on disk. Both are the
    /// half of the archive that can be rebuilt from the stored response without fetching, so
    /// reporting nothing is what lets the next pass simply redo it.
    pub fn read_article(
        &self,
        url: &CanonicalUrl,
        capture: &CaptureId,
    ) -> Result<Option<Article>, StorageError> {
        let record_path = self.article_record_path(url, capture);
        let Some(stored) = read_optional_bounded_json::<StoredArticle>(
            &record_path,
            MAX_ARTICLE_RECORD_BYTES,
            "larger than an article record can be",
        )?
        else {
            return Ok(None);
        };
        let path = self.article_markdown_path(url, capture);
        let Some(bytes) = read_optional_regular_file(
            &path,
            MAX_ARTICLE_MARKDOWN_BYTES,
            "larger than an article document can be",
        )?
        else {
            return Ok(None);
        };
        let markdown = String::from_utf8(bytes).map_err(|source| StorageError::Io {
            path: path.clone(),
            source: io::Error::new(io::ErrorKind::InvalidData, source),
        })?;
        if ContentHash::of(markdown.as_bytes()) != stored.markdown_sha256 {
            return Ok(None);
        }
        Ok(Some(Article {
            markdown,
            record: stored.record,
        }))
    }

    /// Every item in the archive, read from the tree rather than from addresses a caller
    /// already holds, together with whatever the tree held that is not one.
    pub fn walk(&self) -> Result<ArchiveWalk, StorageError> {
        super::walk::walk(&self.root)
    }

    pub fn read_item(&self, url: &CanonicalUrl) -> Result<Item, StorageError> {
        read_optional_json(&self.item_path(url))?.ok_or_else(|| StorageError::NoSuchItem {
            url: url.to_string(),
        })
    }

    pub fn read_capture(
        &self,
        url: &CanonicalUrl,
        capture: &CaptureId,
    ) -> Result<Capture, StorageError> {
        read_optional_json(&self.capture_path(url, capture))?.ok_or_else(|| {
            StorageError::NoSuchCapture {
                url: url.to_string(),
                capture: capture.to_string(),
            }
        })
    }

    /// Every capture of an item, oldest first, which is the order the ids already sort in.
    pub fn list_captures(&self, url: &CanonicalUrl) -> Result<Vec<CaptureId>, StorageError> {
        let dir = self.item_dir(url).join("captures");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(StorageError::Io { path: dir, source }),
        };

        let mut captures = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| StorageError::Io {
                path: dir.clone(),
                source,
            })?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json")
                && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
                && let Ok(id) = CaptureId::try_from(stem.to_owned())
            {
                captures.push(id);
            }
        }
        captures.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        Ok(captures)
    }

    /// Reads a stored body and checks it against the name it was filed under. The address
    /// is the hash, so the check costs one pass over bytes already in memory and turns
    /// "the archive is intact" from an assumption into something the read proves.
    pub fn read_body(&self, hash: &ContentHash) -> Result<Vec<u8>, StorageError> {
        let path = self.body_path(hash);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Err(StorageError::MissingBody {
                    hash: hash.to_string(),
                });
            }
            Err(source) => return Err(StorageError::Io { path, source }),
        };
        if ContentHash::of(&bytes) == *hash {
            Ok(bytes)
        } else {
            Err(StorageError::CorruptBody {
                hash: hash.to_string(),
            })
        }
    }

    /// Bytes already in the store are left alone: the address is the content, so an
    /// existing file is by definition the file that would have been written.
    fn write_body(&self, bytes: &[u8]) -> Result<StoredBody, StorageError> {
        let sha256 = ContentHash::of(bytes);
        let path = self.body_path(&sha256);
        if !path.exists() {
            write_atomically(&path, bytes)?;
        }
        Ok(StoredBody {
            sha256,
            byte_len: bytes.len() as u64,
        })
    }

    /// The item record is rewritten on every capture so its window covers all of them.
    /// Both ends widen rather than only the last one, because captures are not always
    /// written in the order they were fetched: a backfilled older capture moves the start.
    fn record_item(&self, url: &CanonicalUrl, fetched_at: Timestamp) -> Result<(), StorageError> {
        let path = self.item_path(url);
        let known: Option<Item> = read_optional_json(&path)?;
        let item = Item {
            id: ItemId::of(url),
            canonical_url: url.clone(),
            first_captured_at: known
                .as_ref()
                .map_or(fetched_at, |item| item.first_captured_at.min(fetched_at)),
            last_captured_at: known
                .as_ref()
                .map_or(fetched_at, |item| item.last_captured_at.max(fetched_at)),
        };
        write_json(&path, &item)
    }

    fn item_dir(&self, url: &CanonicalUrl) -> PathBuf {
        self.root
            .join("items")
            .join(url.host_dir())
            .join(ItemId::of(url).as_str())
    }

    fn item_path(&self, url: &CanonicalUrl) -> PathBuf {
        self.item_dir(url).join("item.json")
    }

    fn capture_path(&self, url: &CanonicalUrl, capture: &CaptureId) -> PathBuf {
        self.item_dir(url)
            .join("captures")
            .join(format!("{capture}.json"))
    }

    /// Beside the capture and named after it, so the pair is obvious in a directory listing
    /// and neither has to be found through the other. The extra suffix is also what keeps
    /// `list_captures` from reading a derived file as a capture: the stem it would parse is
    /// no longer the shape of a capture id.
    fn metadata_path(&self, url: &CanonicalUrl, capture: &CaptureId) -> PathBuf {
        self.item_dir(url)
            .join("captures")
            .join(format!("{capture}.metadata.json"))
    }

    /// The prose, as a document rather than as a field. The `.article` suffix keeps the stem
    /// from being the shape of a capture id, for the same reason `.metadata` does.
    fn article_markdown_path(&self, url: &CanonicalUrl, capture: &CaptureId) -> PathBuf {
        self.item_dir(url)
            .join("captures")
            .join(format!("{capture}.article.md"))
    }

    fn article_record_path(&self, url: &CanonicalUrl, capture: &CaptureId) -> PathBuf {
        self.item_dir(url)
            .join("captures")
            .join(format!("{capture}.article.json"))
    }

    /// Named so that it sorts beside the article record it stands in for, and reads in a
    /// directory listing as what it is: the decision about this capture's article was made,
    /// and it was no.
    fn refused_extraction_path(&self, url: &CanonicalUrl, capture: &CaptureId) -> PathBuf {
        self.item_dir(url)
            .join("captures")
            .join(format!("{capture}.article-refused.json"))
    }

    fn body_path(&self, hash: &ContentHash) -> PathBuf {
        let (first, second) = hash.shard();
        self.root
            .join("blobs")
            .join("sha256")
            .join(first)
            .join(second)
            .join(hash.as_str())
    }
}

/// Whether a directory holds anything that would make adopting it as an archive a
/// mistake. Dotted entries do not count: a crash during the very first write leaves this
/// store's own temporary file behind, and letting that permanently block the directory it
/// was created in would be the store poisoning its own root.
fn directory_has_visible_entries(path: &Path) -> Result<bool, StorageError> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(StorageError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| StorageError::Io {
            path: path.to_owned(),
            source,
        })?;
        if !entry.file_name().to_string_lossy().starts_with('.') {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The name a capture is filed under, derived from everything about the response that
/// distinguishes it from another one. Two fetches that agree on all of it are the same
/// capture and share a file; anything that differs, including a status or a header behind
/// identical bytes, gets a name of its own.
///
/// The fields are hashed length-prefixed rather than serialized. A capture id is a
/// filename that has to stay the same forever, and JSON output is a moving target: a
/// renamed field or a changed formatter would silently rename every capture written after
/// it, filing a re-write of an existing capture beside the original instead of over it.
///
/// What a capture missed is not in here, and does not need to be, but only because each asset
/// below contributes its address as well as its bytes. Two captures of one page that agree on
/// every asset they hold, address by address, referenced the same subresources and therefore
/// missed the same ones. Adding the misses would hash a reason string that varies between two
/// attempts at the same failure.
fn fingerprint_of(new: &NewCapture, body: &ContentHash) -> ContentHash {
    fn push_field(buffer: &mut Vec<u8>, value: &str) {
        buffer.extend_from_slice(&(value.len() as u64).to_le_bytes());
        buffer.extend_from_slice(value.as_bytes());
    }

    let media_type = new.media_type.as_deref();
    let mut buffer = Vec::new();
    push_field(&mut buffer, &new.requested_url);
    push_field(&mut buffer, &new.final_url);
    push_field(&mut buffer, &new.status.to_string());
    // An absent media type is not an empty one, and a name must not conflate them.
    buffer.push(u8::from(media_type.is_some()));
    push_field(&mut buffer, media_type.unwrap_or(""));
    for header in &new.response_headers {
        push_field(&mut buffer, &header.name);
        push_field(&mut buffer, &header.value);
    }
    push_field(&mut buffer, body.as_str());
    // Two fetches can agree on every byte they kept and disagree on whether that was all
    // of them, and the whole promise of this name is that a difference gets a file rather
    // than overwriting the capture it differs from.
    buffer.push(u8::from(new.body_truncated));
    // Each asset contributes what it is and not only what it holds. Hashing the body alone
    // would name two captures alike whenever the bytes match and the records do not, and a
    // page referencing two addresses that serve identical bytes, which every tracking pixel
    // on a site is, is enough for that: a capture that got the first and a capture that got
    // the second would share a name, and the second write would land on top of the first.
    for asset in &new.assets {
        let media_type = asset.media_type.as_deref();
        push_field(&mut buffer, &asset.requested_url);
        push_field(&mut buffer, &asset.final_url);
        push_field(&mut buffer, &asset.status.to_string());
        buffer.push(u8::from(media_type.is_some()));
        push_field(&mut buffer, media_type.unwrap_or(""));
        push_field(&mut buffer, asset.body.sha256.as_str());
    }
    ContentHash::of(&buffer)
}

fn read_optional_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, StorageError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(StorageError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| StorageError::MalformedRecord {
            path: path.to_owned(),
            source,
        })
}

fn read_optional_bounded_json<T: DeserializeOwned>(
    path: &Path,
    max_len: u64,
    too_large: &'static str,
) -> Result<Option<T>, StorageError> {
    let Some(bytes) = read_optional_regular_file(path, max_len, too_large)? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| StorageError::MalformedRecord {
            path: path.to_owned(),
            source,
        })
}

fn read_optional_regular_file(
    path: &Path,
    max_len: u64,
    too_large: &'static str,
) -> Result<Option<Vec<u8>>, StorageError> {
    let shape = match fs::symlink_metadata(path) {
        Ok(shape) => shape,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(StorageError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    if !shape.is_file() {
        return Err(StorageError::RefusedRecord {
            path: path.to_owned(),
            reason: "this archive record is a regular file, and this is not one",
        });
    }
    if shape.len() > max_len {
        return Err(StorageError::RefusedRecord {
            path: path.to_owned(),
            reason: too_large,
        });
    }

    fs::read(path).map(Some).map_err(|source| StorageError::Io {
        path: path.to_owned(),
        source,
    })
}

fn write_json<T: Serialize>(path: &Path, record: &T) -> Result<(), StorageError> {
    // Pretty printed because the archive is meant to be read by whoever finds it, and a
    // one-line JSON object is a wall no `diff` or `grep` can help with.
    let mut bytes =
        serde_json::to_vec_pretty(record).map_err(|source| StorageError::UnserializableRecord {
            path: path.to_owned(),
            source,
        })?;
    bytes.push(b'\n');
    write_atomically(path, &bytes)
}

/// Counter behind the temporary file names, so two writes in the same process cannot pick
/// the same one.
static WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Writes through a temporary file in the destination directory and renames it into
/// place. A reader therefore sees either the previous record or the complete new one,
/// never the half of it that had been flushed when the machine lost power.
///
/// Both the file and the directory holding it are flushed. Syncing only the file makes the
/// content durable but not the name it was given, and a record whose name did not survive
/// is a record the archive lost.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), StorageError> {
    let dir = path.parent().ok_or_else(|| StorageError::Io {
        path: path.to_owned(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "path has no parent directory"),
    })?;
    fs::create_dir_all(dir).map_err(|source| StorageError::Io {
        path: dir.to_owned(),
        source,
    })?;

    let temp = dir.join(format!(
        ".{}.{}.tmp",
        std::process::id(),
        WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = File::create(&temp).map_err(|source| StorageError::Io {
        path: temp.clone(),
        source,
    })?;
    let written = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::rename(&temp, path))
        .and_then(|()| File::open(dir)?.sync_all());
    if let Err(source) = written {
        let _ = fs::remove_file(&temp);
        return Err(StorageError::Io {
            path: path.to_owned(),
            source,
        });
    }
    Ok(())
}
