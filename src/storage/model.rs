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
    /// Whether the stored body is less than the response promised. A record written before
    /// the archive tracked this reads as false, which is what it always meant: the field
    /// says a shortfall was seen, never that none happened.
    #[serde(default)]
    pub body_truncated: bool,
    pub fetched_at: Timestamp,
    pub assets: Vec<Asset>,
    /// Subresources the page referenced and the archive does not have, each with the reason.
    ///
    /// Without this the absence is readable but mute: a reader can see that the page
    /// references twenty subresources and the capture holds twelve, and has no way to tell
    /// the eight the archive refused from the eight a server never sent. The run reports the
    /// same thing while it is running, and a run is over by the time anyone asks.
    ///
    /// Empty for a capture that got everything, which is the ordinary case, and for one
    /// written before the archive recorded this.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assets_missed: Vec<MissedAsset>,
    /// How this capture departed from the archive's default policy, empty for a run that
    /// departed from nothing and for every record written before the field existed.
    ///
    /// It stays out of the capture fingerprint, for the reason `assets_missed` does: it says
    /// what the run did rather than what the response was, and two fetches that agree on every
    /// recorded byte are the same capture whatever the run was carrying at the time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policy_departures: Vec<PolicyDeparture>,
}

/// How a capture departed from the archive's default policy.
///
/// Two captures of one URL, one made as an anonymous reader and one with a subscription, are
/// different observations of the page, and a reader comparing them has to be able to tell which
/// is which. It is one field with one entry per departure rather than a boolean per decision,
/// so the next thing a run can be told to do differently costs a variant here and nothing else.
///
/// What it describes is this capture and not the whole run: a run holding a session for one host
/// asks another host's pages without it, and marking those as authenticated would be a lie about
/// the observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDeparture {
    /// The request carried a subscription the operator holds, so what came back is what a paying
    /// reader is served rather than what the page shows everyone else. What the credential was is
    /// not recorded anywhere: the archive keeps that a session was used and never the session.
    Session,
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

/// A subresource that was referenced and not stored, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissedAsset {
    pub url: String,
    /// Flattened, so the record reads as one object with a `reason` in it rather than as a
    /// reason wrapped in a field also called reason.
    #[serde(flatten)]
    pub reason: AssetMiss,
}

/// Why a referenced subresource is not in the capture.
///
/// The distinction that matters is between what the archive decided and what the web did.
/// A ceiling reached is a decision, and knowing which one was reached is what tells a reader
/// whether raising a number would have kept the page whole. A response that never came is
/// the web, and no number here would have changed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum AssetMiss {
    /// No server answered, or the engine refused to dial the address at all.
    NoResponse { detail: String },
    /// The response was larger than one subresource may spend. The size is kept because it is
    /// what says how far over the ceiling the page was.
    TooLarge { byte_len: u64 },
    /// Less arrived than the response promised. Unlike a page, which is kept short and marked
    /// as short, a subresource that is incomplete is not stored at all: it exists so the page
    /// still works, a stylesheet missing its end does not, and this record has nowhere to say
    /// that the bytes are partial.
    ArrivedShort { byte_len: u64 },
    /// The capture had already dealt with as many references as one capture may. The
    /// references dealt with first are the ones an archive of prose loses the least without
    /// missing, so a reference marked with this reason is disproportionately a script: the
    /// ceiling was reached only after the kinds ranked ahead of it were already asked for.
    CountCeilingReached,
    /// The capture had already spent the bytes one capture may spend on subresources.
    ByteCeilingReached,
    /// The run's wall-clock budget was gone before this one was asked for.
    DeadlineReached,
    /// The capture stopped asking, because the requests before this one had produced no
    /// response at all several times over. That is a fact about what was answering rather than
    /// about this file, so the file may well still be there.
    NothingWasAnswering,
    /// The reference ended on an address that exists only inside a network, which a run that
    /// did not ask for those addresses had no business reaching.
    InsideANetwork,
}

/// A capture before it is stored. It carries the page's bytes where the stored record
/// carries a hash, which is the whole difference between the two and the reason they are
/// separate types rather than one type with optional fields.
///
/// Its subresources arrive already stored, as records. They are shared, so the run that
/// captured them stores each set of bytes once and hands the same record to every capture
/// that referenced it: keeping the bytes here instead would mean carrying a site's
/// stylesheets in memory for as long as the run lasts, to write files that are already
/// there.
#[derive(Debug, Clone)]
pub struct NewCapture {
    pub canonical_url: CanonicalUrl,
    pub requested_url: String,
    pub final_url: String,
    pub status: u16,
    pub media_type: Option<String>,
    pub response_headers: Vec<Header>,
    pub body: Vec<u8>,
    pub body_truncated: bool,
    /// The clock is an input rather than something the store reads, so the same capture
    /// written twice produces the same record and a test needs no clock of its own.
    pub fetched_at: Timestamp,
    pub assets: Vec<Asset>,
    pub assets_missed: Vec<MissedAsset>,
    pub policy_departures: Vec<PolicyDeparture>,
}

/// A subresource that was fetched and not yet stored.
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
