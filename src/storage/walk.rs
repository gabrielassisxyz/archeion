//! Reading the archive back from the tree instead of from an address.
//!
//! Every other entry point into the store is given a canonical URL and derives a path from
//! it. This one goes the other way, which is what anything answering "what is in here" needs,
//! and it is therefore the only one that meets a directory nobody wrote through this program.
//! It treats what it finds as a claim to be checked rather than as a record to be trusted.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::archive::{StorageError, read_optional_json};
use super::model::{Item, ItemId};

/// What a walk found, and what it refused to call an item.
///
/// The two are returned together rather than the first one alone, because a caller counting
/// items has to be able to tell an archive that is empty from one that is damaged.
#[derive(Debug, Default)]
pub struct ArchiveWalk {
    /// Ordered by canonical URL, so that a listing or an export built on this is the same
    /// twice in a row rather than following whatever order the filesystem returned.
    pub items: Vec<Item>,
    pub unreadable: Vec<UnreadableItem>,
}

/// A directory under `items/` that could not be read back as one item.
#[derive(Debug, thiserror::Error)]
pub enum UnreadableItem {
    /// Captures with no item record beside them. The write order exists to make this
    /// unreachable, since the record carries the only copy of the address that the hashed
    /// directory name does not, so reaching it means something outside this program has
    /// been at the tree.
    #[error("{path} has no item record, so nothing says which address its captures came from")]
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
    #[error("{path} holds the record for {url}, which belongs under {expected}")]
    Misfiled {
        path: PathBuf,
        url: String,
        expected: PathBuf,
    },
}

pub(super) fn walk(root: &Path) -> Result<ArchiveWalk, StorageError> {
    let mut found = ArchiveWalk::default();
    for host in subdirectories(&root.join("items"))? {
        for item_dir in subdirectories(&host)? {
            read_item(root, &item_dir, &mut found)?;
        }
    }
    found.items.sort_by(|left, right| {
        left.canonical_url
            .as_str()
            .cmp(right.canonical_url.as_str())
    });
    Ok(found)
}

fn read_item(root: &Path, dir: &Path, found: &mut ArchiveWalk) -> Result<(), StorageError> {
    let item: Item = match read_optional_json(&dir.join("item.json")) {
        Ok(Some(item)) => item,
        Ok(None) => {
            found.unreadable.push(UnreadableItem::NoRecord {
                path: dir.to_owned(),
            });
            return Ok(());
        }
        Err(StorageError::MalformedRecord { path, source }) => {
            found
                .unreadable
                .push(UnreadableItem::Malformed { path, source });
            return Ok(());
        }
        // A record that will not parse is the archive being wrong, which a walk over hostile
        // input has to survive. A directory that will not be read is the machine being wrong,
        // and carrying on there would report a partial archive as a whole one.
        Err(unreadable) => return Err(unreadable),
    };

    let expected = root
        .join("items")
        .join(item.canonical_url.host_dir())
        .join(ItemId::of(&item.canonical_url).as_str());
    if dir != expected || item.id != ItemId::of(&item.canonical_url) {
        found.unreadable.push(UnreadableItem::Misfiled {
            path: dir.to_owned(),
            url: item.canonical_url.to_string(),
            expected,
        });
        return Ok(());
    }
    found.items.push(item);
    Ok(())
}

/// The directories directly inside `path`, sorted, and none if the path is not there yet.
///
/// Anything that is not a directory is passed over without a word. A stray file is not a
/// damaged record, and reporting every one of them would bury the entries that are.
fn subdirectories(path: &Path) -> Result<Vec<PathBuf>, StorageError> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(StorageError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };

    let mut directories = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| StorageError::Io {
            path: path.to_owned(),
            source,
        })?;
        if entry
            .file_type()
            .map_err(|source| StorageError::Io {
                path: entry.path(),
                source,
            })?
            .is_dir()
        {
            directories.push(entry.path());
        }
    }
    directories.sort();
    Ok(directories)
}
