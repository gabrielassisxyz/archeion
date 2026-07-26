//! The three records the archive is built from: an item is a canonical URL, a capture is
//! one fetch of it at a point in time, an asset is a subresource that capture needed.
//!
//! Everything that can differ between two fetches of the same URL belongs to the capture.
//! The item holds only what stays true across all of them.

use std::fmt::{self, Write as _};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::canonical_url::CanonicalUrl;

/// An identifier that came from outside this process and turned out not to be one.
///
/// Every id below is spliced into a filesystem path, and every one of them can arrive by
/// deserializing a record that something else may have edited. An archive is hostile input
/// forever, not only while it is being written, so each id is parsed on the way in rather
/// than trusted because of where it was found.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MalformedIdentifier {
    #[error("{0} is not a SHA-256 in lowercase hex")]
    ContentHash(String),
    #[error("{0} is not an item id")]
    ItemId(String),
    #[error("{0} is not a capture id")]
    CaptureId(String),
}

/// The identity of an item, derived from its canonical URL so that two captures of the
/// same page land in the same place without any registry to consult.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ItemId(String);

impl ItemId {
    pub fn of(url: &CanonicalUrl) -> Self {
        Self(sha256_hex(url.as_str().as_bytes()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ItemId {
    type Error = MalformedIdentifier;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if is_sha256_hex(&value) {
            Ok(Self(value))
        } else {
            Err(MalformedIdentifier::ItemId(value))
        }
    }
}

impl From<ItemId> for String {
    fn from(id: ItemId) -> Self {
        id.0
    }
}

impl fmt::Display for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The SHA-256 of a stored body, in lowercase hex. It is both the integrity check and the
/// address: identical bytes are one file no matter how many captures reference them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ContentHash(String);

impl ContentHash {
    pub fn of(bytes: &[u8]) -> Self {
        Self(sha256_hex(bytes))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Two levels of two hex digits, so no single directory ever holds more than a few
    /// hundred entries no matter how large the collection grows.
    pub(crate) fn shard(&self) -> (&str, &str) {
        (&self.0[0..2], &self.0[2..4])
    }

    pub(crate) fn short(&self) -> &str {
        &self.0[0..8]
    }
}

impl TryFrom<String> for ContentHash {
    type Error = MalformedIdentifier;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if is_sha256_hex(&value) {
            Ok(Self(value))
        } else {
            Err(MalformedIdentifier::ContentHash(value))
        }
    }
}

impl From<ContentHash> for String {
    fn from(hash: ContentHash) -> Self {
        hash.0
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A capture is named after the instant it was fetched, so the directory listing of an
/// item is already its history in order, plus a fingerprint of the response that separates
/// two captures landing in the same second.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CaptureId(String);

impl CaptureId {
    /// The fingerprint covers the response and not just its body: a retry within the same
    /// second can return identical bytes under a different status or a different final
    /// URL, and naming that after the body alone files the second capture on top of the
    /// first. Two fetches alike in every recorded respect are the same capture, so they
    /// share a name on purpose and rewriting one is idempotent.
    pub(crate) fn new(fetched_at: Timestamp, fingerprint: &ContentHash) -> Self {
        Self(format!(
            "{}-{}",
            fetched_at.strftime("%Y%m%dT%H%M%SZ"),
            fingerprint.short()
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CaptureId {
    type Error = MalformedIdentifier;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if is_capture_id(&value) {
            Ok(Self(value))
        } else {
            Err(MalformedIdentifier::CaptureId(value))
        }
    }
}

impl From<CaptureId> for String {
    fn from(id: CaptureId) -> Self {
        id.0
    }
}

impl fmt::Display for CaptureId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The exact shape `CaptureId::new` writes: `20260725T140322Z-3c70219d`. Checking the
/// shape and not merely the character set is what keeps a name read off the filesystem
/// from becoming a path, and it costs the same.
fn is_capture_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 25
        && bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[8] == b'T'
        && bytes[9..15].iter().all(u8::is_ascii_digit)
        && bytes[15] == b'Z'
        && bytes[16] == b'-'
        && bytes[17..]
            .iter()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// What stays true about a URL across every capture of it. The canonical URL is the
/// load-bearing field: the directory it lives in is a hash, so without this record the
/// tree cannot be read back to the addresses it was built from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub id: ItemId,
    pub canonical_url: CanonicalUrl,
    pub first_captured_at: Timestamp,
    pub last_captured_at: Timestamp,
}

/// One response header, kept in the order and multiplicity it arrived in, because a map
/// would silently drop the repeated ones a diagnosis later depends on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

/// A reference to bytes held in the content-addressed store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredBody {
    pub sha256: ContentHash,
    pub byte_len: u64,
}

/// One fetch of an item. Every field here can differ from one capture to the next, which
/// is exactly why none of them belongs on the item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capture {
    pub id: CaptureId,
    pub item_id: ItemId,
    /// Where the fetch started, which differs from the final URL whenever it redirected.
    pub requested_url: String,
    pub final_url: String,
    pub status: u16,
    pub media_type: Option<String>,
    pub response_headers: Vec<Header>,
    pub body: StoredBody,
    pub fetched_at: Timestamp,
    pub assets: Vec<Asset>,
}

/// A subresource a capture needed. It is stored inside the capture rather than as a
/// record of its own because it means nothing without the page that referenced it, while
/// its bytes are already shared through the content-addressed store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub requested_url: String,
    pub final_url: String,
    pub status: u16,
    pub media_type: Option<String>,
    pub body: StoredBody,
}

/// A capture before it is stored. It carries bytes where the stored record carries a
/// hash, which is the whole difference between the two and the reason they are separate
/// types rather than one type with optional fields.
#[derive(Debug, Clone)]
pub struct NewCapture {
    pub canonical_url: CanonicalUrl,
    pub requested_url: String,
    pub final_url: String,
    pub status: u16,
    pub media_type: Option<String>,
    pub response_headers: Vec<Header>,
    pub body: Vec<u8>,
    /// The clock is an input rather than something the store reads, so the same capture
    /// written twice produces the same record and a test needs no clock of its own.
    pub fetched_at: Timestamp,
    pub assets: Vec<NewAsset>,
}

#[derive(Debug, Clone)]
pub struct NewAsset {
    pub requested_url: String,
    pub final_url: String,
    pub status: u16,
    pub media_type: Option<String>,
    pub body: Vec<u8>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        // Writing into a String cannot fail, and the alternative is a hex dependency for
        // sixteen characters of code.
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_id_is_the_hash_of_the_canonical_url() {
        let url = CanonicalUrl::parse("https://example.com/a").expect("valid url");
        let same = CanonicalUrl::parse("https://example.com/a").expect("valid url");
        let other = CanonicalUrl::parse("https://example.com/b").expect("valid url");

        assert_eq!(ItemId::of(&url), ItemId::of(&same));
        assert_ne!(ItemId::of(&url), ItemId::of(&other));
        assert_eq!(ItemId::of(&url).as_str().len(), 64);
    }

    #[test]
    fn capture_id_orders_by_time_and_separates_the_same_second() {
        let earlier: Timestamp = "2026-07-25T14:03:22Z".parse().expect("valid timestamp");
        let later: Timestamp = "2026-07-25T14:03:23Z".parse().expect("valid timestamp");
        let one = CaptureId::new(earlier, &ContentHash::of(b"one"));
        let two = CaptureId::new(earlier, &ContentHash::of(b"two"));

        assert!(one.as_str() < CaptureId::new(later, &ContentHash::of(b"one")).as_str());
        assert_ne!(one, two);
        assert_eq!(one.as_str(), "20260725T140322Z-7692c3ad");
    }

    #[test]
    fn an_identifier_read_off_disk_cannot_be_a_path() {
        for hostile in ["../../etc/passwd", "//etc/passwd", "ab", "é", ""] {
            assert!(ContentHash::try_from(hostile.to_owned()).is_err());
            assert!(ItemId::try_from(hostile.to_owned()).is_err());
            assert!(CaptureId::try_from(hostile.to_owned()).is_err());
        }
        assert!(CaptureId::try_from("20260725T140322Z-7692c3ad".to_owned()).is_ok());
        assert!(ContentHash::try_from(ContentHash::of(b"a").to_string()).is_ok());
    }

    #[test]
    fn a_capture_id_that_is_the_right_length_but_the_wrong_shape_is_refused() {
        for wrong in [
            "-------------------------",
            "AAAAAAAAAAAAAAAAAAAAAAAAA",
            "20260725X140322Z-7692c3ad",
            "20260725T140322Z-7692c3aG",
        ] {
            assert!(CaptureId::try_from(wrong.to_owned()).is_err());
        }
    }

    #[test]
    fn a_malformed_identifier_in_a_record_is_refused_on_the_way_in() {
        let tampered = format!(
            r#"{{"sha256":"../../../etc/passwd","byte_len":{}}}"#,
            u64::MAX
        );
        assert!(serde_json::from_str::<StoredBody>(&tampered).is_err());
    }

    #[test]
    fn identical_bytes_hash_to_one_address() {
        assert_eq!(ContentHash::of(b"<html>"), ContentHash::of(b"<html>"));
        assert_ne!(ContentHash::of(b"<html>"), ContentHash::of(b"<HTML>"));
        assert_eq!(ContentHash::of(b"").shard(), ("e3", "b0"));
    }
}
