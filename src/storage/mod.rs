//! The archive on disk: the records it holds and the store that reads and writes them.
//!
//! The layout and the reasoning behind it are written down in `docs/storage-model.md`.

mod archive;
mod model;
mod walk;

pub use archive::{Archive, StorageError};
pub use model::{
    Asset, AssetMiss, Capture, CaptureId, ContentHash, Header, Item, ItemId, MalformedIdentifier,
    MissedAsset, NewAsset, NewCapture, StoredBody,
};
pub use walk::{ArchiveWalk, UnreadableItem};
