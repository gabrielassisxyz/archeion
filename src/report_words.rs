//! The one word each fate of a page or a capture is called by, wherever it is printed.
//!
//! A run says what it is doing twice: once on stderr while it goes, once on stdout when it is
//! over. Two spellings of one fate makes those two accounts read as accounts of different
//! runs, so every word either of them spends comes from here and from nowhere else. Adding a
//! fate means adding a constant, which is what makes the collision visible: two fates reaching
//! for one constant is the failure this module exists to make impossible to write by accident.
//!
//! The end-of-run report prints each of these under a label, so it can drop a noun the label
//! already carries. A progress line has no label, so it carries the noun itself. Where that
//! splits one fate into two spellings, both live here beside each other and a test below pins
//! the second to the first.

/// A host answered a request with a refusal of its own, which is a page the run paid for and
/// did not get. The report's row label, and the word a progress line spends on the same fate.
pub const HOST_REFUSED: &str = "host refused";

/// A response was stored and its prose reading found an article.
pub const EXTRACTED: &str = "extracted";

/// A response was stored and the reading of its prose refused it: a paywall, a page too thin
/// to be prose at all. The half of the report's `articles` row that counts it, which reads
/// "N extracted, M refused" under a label already naming articles.
pub const REFUSED_IN_ARTICLES_ROW: &str = "refused";

/// The same fate as [`REFUSED_IN_ARTICLES_ROW`], spelled for a line with no label above it.
/// It is a different fate from [`HOST_REFUSED`]: one is a host declining to serve a page, the
/// other is this project declining to call a page it did receive an article.
pub const ARTICLE_REFUSED: &str = "article refused";

/// A response was stored and its prose reading found no article and no reason to refuse one.
pub const NOT_ARTICLE: &str = "not article";

/// A response was stored and nothing distinguishing was read out of it.
pub const STORED: &str = "stored";

/// Nothing answered: the request was made and no response came back.
pub const NO_RESPONSE: &str = "no response";

/// The address resolved to somewhere only this machine or its network can reach.
pub const INSIDE_A_NETWORK: &str = "inside a network";

/// The URL could not be canonicalized into an address the archive can file anything under.
pub const NO_ADDRESS: &str = "no address";

/// A repass wrote an article over a capture already on disk. The half of the repass report's
/// `articles` row that counts it, which reads "N written" under that same label.
pub const WRITTEN: &str = "written";

/// A repass replaced the metadata beside a capture and left the article as it found it. The
/// report counts this under `metadata_written`, so progress may not call it unchanged.
pub const METADATA_WRITTEN: &str = "metadata written";

/// A repass found every derived record over a capture already current.
pub const UNCHANGED: &str = "unchanged";

/// A repass could not read something it needed, which is the word its warnings already spend
/// on an item, a body or a reading it could not refresh.
pub const UNREADABLE: &str = "unreadable";

#[cfg(test)]
mod tests {
    use super::*;

    /// The one pairing here that is two spellings of one fate rather than one word used
    /// twice. If the row's half is ever reworded, the standalone spelling has to follow it,
    /// and this is what says so out loud.
    #[test]
    fn the_standalone_refusal_is_the_articles_row_s_own_word_with_its_noun_restored() {
        assert_eq!(
            ARTICLE_REFUSED,
            format!("article {REFUSED_IN_ARTICLES_ROW}")
        );
    }

    /// The failure the split exists to prevent: a reader of a progress line cannot tell a
    /// host's refusal from a reading's refusal if both print the same string.
    #[test]
    fn a_host_s_refusal_and_a_reading_s_refusal_are_not_the_same_word() {
        assert_ne!(HOST_REFUSED, ARTICLE_REFUSED);
    }
}
