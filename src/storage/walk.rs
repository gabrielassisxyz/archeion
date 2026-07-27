//! Reading the archive back from the tree instead of from an address.
//!
//! Every other entry point into the store is given a canonical URL and derives a path from
//! it. This one goes the other way, which is what anything answering "what is in here" needs,
//! and it is therefore the only one that reaches a path without being asked for it. It treats
//! what it finds as a claim to be checked rather than as a record to be trusted, and it costs
//! a damaged corner of a tree the corner rather than the whole walk.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::archive::StorageError;
use super::model::{Item, ItemId};

/// How much an item record may weigh.
///
/// One holds an id, an address and two instants, so this is generous by three orders of
/// magnitude: a file that reaches it is not a record that grew, it is a file that is not one.
const MAX_ITEM_RECORD_BYTES: u64 = 64 * 1024;

/// What a walk found, and what it refused to call an item.
///
/// The two are returned together rather than the first one alone, because a caller counting
/// items has to be able to tell an archive that is empty from one that is damaged.
#[derive(Debug, Default)]
pub struct ArchiveWalk {
    /// Ordered by canonical URL, so that a listing or an export built on this is the same
    /// twice in a row. The tree orders items by the hash of their address, which is to say
    /// not at all.
    pub items: Vec<Item>,
    pub unreadable: Vec<UnreadableItem>,
}

/// Something under `items/` that could not be read back as one item.
///
/// Every variant carries the item directory rather than the file inside it, so a caller can
/// report or revisit any of them the same way.
#[derive(Debug, thiserror::Error)]
pub enum UnreadableItem {
    /// No record where one belongs. The write order makes this unreachable for an item that
    /// has captures, since the record carries the only copy of the address the hashed
    /// directory name does not. It is still reachable without anyone editing the tree: a
    /// crash inside the first write of a new item leaves the directory and no record in it.
    #[error("{path} holds no item record, so nothing here says which address it is")]
    NoRecord { path: PathBuf },
    #[error("{path}: malformed item record: {source}")]
    Malformed {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    /// The record and the tree disagree about which address this is. It matters more than a
    /// tidiness complaint: the walk found this record here, a lookup by the URL inside it
    /// would go somewhere else, and an archive that answers two ways is worse than one that
    /// refuses to answer.
    ///
    /// The address is re-canonicalized on the way in, so changing a canonicalization rule
    /// reports every item already filed under the old spelling as this, and drops all of them
    /// out of anything built on the walk. A rule change is a migration that rewrites the tree,
    /// not an edit to the rules.
    #[error("{path} holds the record for {url}, which belongs under {expected}")]
    Misfiled {
        path: PathBuf,
        url: String,
        expected: PathBuf,
    },
    /// A path the walk refused rather than read. Nothing this program writes takes any of
    /// these shapes: following a symbolic link would let an edited tree aim the walk at any
    /// path on the machine, a device or a pipe costs unbounded memory or time behind a size
    /// that promises nothing, and a record far past its ceiling is not a record.
    #[error("{path} was refused rather than read: {reason}")]
    Refused { path: PathBuf, reason: &'static str },
    /// This entry could not be read at all. It is collected rather than raised, because one
    /// unreadable directory among a hundred thousand is a damaged archive and not an
    /// unreadable one, and refusing to list the intact items would be the larger loss.
    #[error("{path} could not be read: {source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

pub(super) fn walk(root: &Path) -> Result<ArchiveWalk, StorageError> {
    let items_root = root.join("items");
    let mut found = ArchiveWalk::default();
    let hosts = match subdirectories(&items_root, &mut found) {
        Ok(hosts) => hosts,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(found),
        // An archive whose `items/` will not list has no entries to report one by one, so
        // there is nothing here to salvage and reporting emptiness would be a lie.
        Err(source) => {
            return Err(StorageError::Io {
                path: items_root,
                source,
            });
        }
    };

    for host in hosts {
        match subdirectories(&host, &mut found) {
            Ok(item_dirs) => {
                for dir in item_dirs {
                    read_item(root, &dir, &mut found);
                }
            }
            Err(source) => found
                .unreadable
                .push(UnreadableItem::Unreadable { path: host, source }),
        }
    }
    found.items.sort_by(|left, right| {
        left.canonical_url
            .as_str()
            .cmp(right.canonical_url.as_str())
    });
    Ok(found)
}

fn read_item(root: &Path, dir: &Path, found: &mut ArchiveWalk) {
    let item = match item_record(dir) {
        Ok(item) => item,
        Err(problem) => {
            found.unreadable.push(problem);
            return;
        }
    };

    let derived = ItemId::of(&item.canonical_url);
    let expected = root
        .join("items")
        .join(item.canonical_url.host_dir())
        .join(derived.as_str());
    if dir != expected || item.id != derived {
        found.unreadable.push(UnreadableItem::Misfiled {
            path: dir.to_owned(),
            url: item.canonical_url.to_string(),
            expected,
        });
        return;
    }
    found.items.push(item);
}

fn item_record(dir: &Path) -> Result<Item, UnreadableItem> {
    let path = dir.join("item.json");
    // Read on the link rather than through it, and before the file is opened. A record is the
    // one path down here that something outside this program can aim elsewhere, and by the
    // time `/dev/zero` is being read the choice not to read it has already been made.
    let shape = match fs::symlink_metadata(&path) {
        Ok(shape) => shape,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(UnreadableItem::NoRecord {
                path: dir.to_owned(),
            });
        }
        Err(source) => {
            return Err(UnreadableItem::Unreadable {
                path: dir.to_owned(),
                source,
            });
        }
    };
    if !shape.is_file() {
        return Err(UnreadableItem::Refused {
            path: dir.to_owned(),
            reason: "an item record is a regular file, and this is not one",
        });
    }
    if shape.len() > MAX_ITEM_RECORD_BYTES {
        return Err(UnreadableItem::Refused {
            path: dir.to_owned(),
            reason: "larger than an item record can be",
        });
    }

    let bytes = fs::read(&path).map_err(|source| UnreadableItem::Unreadable {
        path: dir.to_owned(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| UnreadableItem::Malformed {
        path: dir.to_owned(),
        source,
    })
}

/// The directories directly inside `path`, sorted, with everything else either passed over or
/// recorded in `found`. It fails only when the directory itself will not list.
///
/// A regular file is passed over without a word: a stray one is not a damaged record, and
/// reporting every `.DS_Store` would bury the entries that are. Anything else is recorded,
/// because passing over a symbolic link in silence would report an archive as whole while
/// items are missing from it.
fn subdirectories(path: &Path, found: &mut ArchiveWalk) -> Result<Vec<PathBuf>, io::Error> {
    let mut directories = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = match entry {
            Ok(entry) => entry,
            // One entry that will not yield is that entry's problem. The listing itself is
            // still going, so abandoning the rest of it would throw away intact items.
            Err(source) => {
                found.unreadable.push(UnreadableItem::Unreadable {
                    path: path.to_owned(),
                    source,
                });
                continue;
            }
        };
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => directories.push(entry.path()),
            Ok(kind) if kind.is_file() => {}
            Ok(_) => found.unreadable.push(UnreadableItem::Refused {
                path: entry.path(),
                reason: "an archive holds directories here, and this is neither one nor a file",
            }),
            Err(source) => found.unreadable.push(UnreadableItem::Unreadable {
                path: entry.path(),
                source,
            }),
        }
    }
    directories.sort();
    Ok(directories)
}
