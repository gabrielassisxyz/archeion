//! Re-reading an archive without crawling it again.
//!
//! A repass spends only records already in the archive, except for subresources the archive
//! itself recorded as missed. The page responses remain authoritative; metadata, articles and
//! late assets are the derived layer this pass is allowed to replace.

use std::collections::HashMap;
use std::time::Instant;

use crate::CanonicalUrl;
use crate::assets::{AssetCapture, retryable_miss};
use crate::crawl::{CrawlEngine, Seed};
use crate::metadata::{self, AssetKind, PageMetadata, PageSource, ReferencedAsset};
use crate::readability::{self, Extraction, ExtractionRules, SiteRules};
use crate::storage::{Archive, Capture, CaptureId, StorageError};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RepassOptions {
    pub allow_private_addresses: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RepassRun {
    pub captures_seen: usize,
    pub metadata_written: usize,
    pub articles_written: usize,
    pub extractions_refused: usize,
    pub non_articles_marked: usize,
    pub derived_unchanged: usize,
    pub assets_recovered: usize,
    pub asset_fetches: usize,
    pub assets_still_missing: usize,
    pub assets_not_retried: usize,
    pub unreadable_items: Vec<String>,
    pub unreadable_captures: Vec<RepassLoss>,
    pub unreadable_bodies: Vec<RepassLoss>,
    pub unreadable_pages: Vec<RepassLoss>,
    pub unreadable_articles: Vec<RepassLoss>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepassLoss {
    pub url: String,
    pub capture: Option<String>,
    pub reason: String,
}

#[derive(Debug, thiserror::Error)]
pub enum RepassError {
    #[error("{source}")]
    Storage {
        #[source]
        source: StorageError,
        run: Box<RepassRun>,
    },
}

pub fn repass_archive(
    engine: &dyn CrawlEngine,
    archive: &Archive,
    rules: &SiteRules,
    options: RepassOptions,
) -> Result<RepassRun, RepassError> {
    let mut run = RepassRun::default();
    let walk = archive.walk().map_err(|source| RepassError::Storage {
        source,
        run: Box::new(RepassRun::default()),
    })?;
    run.unreadable_items = walk.unreadable.iter().map(ToString::to_string).collect();

    // A seed with no URL and no session. A credential binds to the origin of the address that was
    // typed, and there is none here: this walks an archive that may hold captures of any number of
    // hosts, so nothing in a repass could say which origin a session belongs to.
    let mut seed = Seed::new(String::new());
    seed.allow_private_addresses = options.allow_private_addresses;
    let mut assets = AssetCapture::new(engine, archive, &seed, Instant::now());

    for item in walk.items {
        let captures = match archive.list_captures(&item.canonical_url) {
            Ok(captures) => captures,
            Err(source) => {
                run.unreadable_captures.push(RepassLoss {
                    url: item.canonical_url.to_string(),
                    capture: None,
                    reason: source.to_string(),
                });
                continue;
            }
        };
        for capture_id in captures {
            let capture = match archive.read_capture(&item.canonical_url, &capture_id) {
                Ok(capture) => capture,
                Err(source) => {
                    run.unreadable_captures.push(loss(
                        &item.canonical_url,
                        Some(&capture_id),
                        source,
                    ));
                    continue;
                }
            };
            if capture.id != capture_id {
                run.unreadable_captures.push(RepassLoss {
                    url: item.canonical_url.to_string(),
                    capture: Some(capture_id.to_string()),
                    reason: format!("capture record names itself as {}", capture.id),
                });
                continue;
            }
            run.captures_seen += 1;
            if let Err(source) = repass_one(
                archive,
                rules,
                &item.canonical_url,
                &capture,
                &mut assets,
                &mut run,
            ) {
                run.asset_fetches = assets.fetches();
                return Err(RepassError::Storage {
                    source,
                    run: Box::new(run),
                });
            }
        }
    }
    run.asset_fetches = assets.fetches();
    Ok(run)
}

/// Everything one capture is owed: the subresources it missed, and the derived records over it.
///
/// The metadata record is read here rather than inside either half, because both of them need
/// it and it is the same record: it says which role each reference was named in, which is what
/// orders the retries, and it says which extractor wrote it, which is what makes the derived
/// layer stale.
fn repass_one(
    archive: &Archive,
    rules: &SiteRules,
    url: &CanonicalUrl,
    capture: &Capture,
    assets: &mut AssetCapture<'_>,
    run: &mut RepassRun,
) -> Result<(), StorageError> {
    let metadata = archive.read_metadata(url, &capture.id)?;
    recover_assets(archive, url, capture, metadata.as_ref(), assets, run)?;
    repass_capture(archive, rules, url, capture, metadata, run)
}

fn recover_assets(
    archive: &Archive,
    url: &CanonicalUrl,
    capture: &Capture,
    metadata: Option<&PageMetadata>,
    assets: &mut AssetCapture<'_>,
    run: &mut RepassRun,
) -> Result<(), StorageError> {
    let kinds = kinds_by_url(metadata);
    let mut retryable = Vec::new();
    for missed in &capture.assets_missed {
        if !retryable_miss(&missed.reason) {
            run.assets_not_retried += 1;
            continue;
        }
        retryable.push(ReferencedAsset {
            url: missed.url.clone(),
            // An image when the record does not name the address, which is what every retry
            // was handed over as until the roles were read back at all. A record re-derived
            // under a later extractor can name fewer references than the capture missed, a
            // `srcset`'s narrower renditions being the case in hand, and a lookup that came up
            // empty is not evidence that a page's picture belongs behind its script bundles.
            kind: kinds
                .get(missed.url.as_str())
                .copied()
                .unwrap_or(AssetKind::Image),
        });
    }
    let captured = assets.of_page(&retryable)?;
    run.assets_recovered += captured.stored.len();
    run.assets_still_missing += captured.missed.len();
    archive.add_recovered_assets(url, &capture.id, &captured.stored, &captured.missed)
}

/// The role each reference was named in, keyed by address.
///
/// Built once per capture rather than searched per miss: a page may reference two thousand
/// addresses and miss hundreds of them, and a scan per miss is that product.
fn kinds_by_url(metadata: Option<&PageMetadata>) -> HashMap<&str, AssetKind> {
    metadata
        .map(|metadata| {
            metadata
                .assets
                .iter()
                .map(|asset| (asset.url.as_str(), asset.kind))
                .collect()
        })
        .unwrap_or_default()
}

fn repass_capture(
    archive: &Archive,
    rules: &SiteRules,
    url: &CanonicalUrl,
    capture: &Capture,
    metadata: Option<PageMetadata>,
    run: &mut RepassRun,
) -> Result<(), StorageError> {
    let article_state = ArticleState::read(archive, url, &capture.id)?;
    let metadata_stale = metadata
        .as_ref()
        .is_some_and(|metadata| metadata.extractor_version < metadata::EXTRACTOR_VERSION)
        || (metadata.is_none() && is_html(capture));
    // An article embeds what the metadata said, the page title reaching its first heading, so
    // a metadata record this pass is about to replace makes the article built on it stale as
    // well. Judged only by its own version, that article says it is current, which is exactly
    // what stops any later pass from repairing it: the two spellings then sit in one note, the
    // front matter carrying the new one and the heading the old one, permanently.
    let article_stale = metadata_stale || article_state.is_stale(capture, rules, metadata.as_ref());
    if !metadata_stale && !article_stale {
        run.derived_unchanged += 1;
        return Ok(());
    }

    let body = match archive.read_body(&capture.body.sha256) {
        Ok(body) => body,
        Err(source) => {
            run.unreadable_bodies
                .push(loss(url, Some(&capture.id), source));
            return Ok(());
        }
    };
    let source = PageSource {
        body: &body,
        content_type: content_type_of(&capture.response_headers),
        final_url: capture.final_url.as_str(),
    };
    let current_metadata = if metadata_stale {
        match metadata::extract(source) {
            Ok(Some(extracted)) => {
                archive.write_metadata(url, &capture.id, &extracted)?;
                run.metadata_written += 1;
                Some(extracted)
            }
            Ok(None) => None,
            Err(unreadable) => {
                run.unreadable_pages.push(RepassLoss {
                    url: unreadable.url,
                    capture: Some(capture.id.to_string()),
                    reason: unreadable.reason,
                });
                None
            }
        }
    } else {
        metadata
    };

    if article_stale {
        let title = current_metadata
            .as_ref()
            .and_then(|metadata| metadata.title.as_ref())
            .map(|title| title.value.as_str());
        let accessible_for_free = current_metadata
            .as_ref()
            .and_then(|metadata| readability::declared_accessible_for_free(&metadata.json_ld));
        match readability::extract(source, title, accessible_for_free, rules) {
            Ok(extracted) => {
                write_extraction(archive, url, capture, article_state, extracted, run)?
            }
            Err(unreadable) => run.unreadable_articles.push(RepassLoss {
                url: unreadable.url,
                capture: Some(capture.id.to_string()),
                reason: unreadable.reason,
            }),
        }
    }
    Ok(())
}

fn write_extraction(
    archive: &Archive,
    url: &CanonicalUrl,
    capture: &Capture,
    known: ArticleState,
    extracted: Extraction,
    run: &mut RepassRun,
) -> Result<(), StorageError> {
    match extracted {
        Extraction::Article(article) => {
            if known == ArticleState::Article(article.clone()) {
                run.derived_unchanged += 1;
            } else {
                archive.write_article(url, &capture.id, &article)?;
                run.articles_written += 1;
            }
        }
        Extraction::Refused(refused) => {
            if known == ArticleState::Refused(refused.clone()) {
                run.derived_unchanged += 1;
            } else {
                archive.write_refused_extraction(url, &capture.id, &refused)?;
                run.extractions_refused += 1;
            }
        }
        Extraction::NotArticle(non_article) => {
            if known == ArticleState::NotArticle(non_article.clone()) {
                run.derived_unchanged += 1;
            } else {
                archive.write_non_article(url, &capture.id, &non_article)?;
                run.non_articles_marked += 1;
            }
        }
        Extraction::Nothing => run.derived_unchanged += 1,
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ArticleState {
    Article(readability::Article),
    Refused(readability::RefusedExtraction),
    NotArticle(readability::NonArticle),
    Missing,
}

impl ArticleState {
    fn read(
        archive: &Archive,
        url: &crate::CanonicalUrl,
        capture: &CaptureId,
    ) -> Result<Self, StorageError> {
        if let Some(article) = archive.read_article(url, capture)? {
            return Ok(Self::Article(article));
        }
        if let Some(refused) = archive.read_refused_extraction(url, capture)? {
            return Ok(Self::Refused(refused));
        }
        if let Some(non_article) = archive.read_non_article(url, capture)? {
            return Ok(Self::NotArticle(non_article));
        }
        Ok(Self::Missing)
    }

    fn is_stale(
        &self,
        capture: &Capture,
        rules: &SiteRules,
        metadata: Option<&PageMetadata>,
    ) -> bool {
        let current_rule_exists = rules.has_rule_for(&capture.final_url);
        if current_rule_exists || self.was_made_by_a_site_rule() {
            return true;
        }
        match self {
            Self::Article(article) => {
                article.record.extractor_version < readability::EXTRACTOR_VERSION
                    || self.has_an_unread_declaration(metadata)
            }
            Self::Refused(refused) => refused.extractor_version < readability::EXTRACTOR_VERSION,
            Self::NotArticle(non_article) => {
                non_article.extractor_version < readability::EXTRACTOR_VERSION
            }
            Self::Missing => reads_as_prose(capture),
        }
    }

    /// Whether the page declared how much of itself it was serving and the record does not say.
    ///
    /// This is where the absence is answered, rather than by moving the extractor version. The
    /// version says a record was written under weaker rules and means something else now, which
    /// is not true here: a record with no declaration says nothing, and nothing is what it said.
    /// What is true is that the answer has been sitting in the stored response all along, so the
    /// captures worth re-reading are the ones whose own JSON-LD carries it, and moving the
    /// version would instead rewrite every article in the archive to reach them. It is the same
    /// answer the served-Markdown absence already got, for the same reason.
    fn has_an_unread_declaration(&self, metadata: Option<&PageMetadata>) -> bool {
        let Self::Article(article) = self else {
            return false;
        };
        article.record.accessible_for_free.is_none()
            && metadata.is_some_and(|metadata| {
                readability::declared_accessible_for_free(&metadata.json_ld).is_some()
            })
    }

    fn was_made_by_a_site_rule(&self) -> bool {
        match self {
            Self::Article(article) => matches!(article.record.rules, ExtractionRules::Site(_)),
            Self::Refused(refused) => matches!(refused.rules, ExtractionRules::Site(_)),
            Self::NotArticle(non_article) => {
                matches!(non_article.rules, ExtractionRules::Site(_))
            }
            Self::Missing => false,
        }
    }
}

/// Whether a capture is markup, which is what the metadata extractor reads and nothing else.
fn is_html(capture: &Capture) -> bool {
    capture.media_type.as_deref().is_some_and(|media_type| {
        media_type.eq_ignore_ascii_case("text/html")
            || media_type.eq_ignore_ascii_case("application/xhtml+xml")
    })
}

/// Whether a capture holds prose, which is a wider question than the one above and the reason
/// the two are not one function.
///
/// It is what decides whether no article beside a capture means the extractor has not answered
/// yet. A response served as Markdown is prose the extractor now reads, so every one already in
/// an archive is stale to this pass, which is what makes the change retroactive over captures
/// taken before it. Widening `is_html` instead would send the metadata extractor after a
/// document that has no tags to read, on every pass, forever.
fn reads_as_prose(capture: &Capture) -> bool {
    is_html(capture)
        || capture.media_type.as_deref().is_some_and(|media_type| {
            media_type.eq_ignore_ascii_case("text/markdown")
                || media_type.eq_ignore_ascii_case("text/x-markdown")
        })
}

fn content_type_of(headers: &[crate::storage::Header]) -> Option<&str> {
    headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("content-type"))
        .map(|header| header.value.as_str())
}

fn loss(
    url: &crate::CanonicalUrl,
    capture: Option<&CaptureId>,
    source: impl ToString,
) -> RepassLoss {
    RepassLoss {
        url: url.to_string(),
        capture: capture.map(ToString::to_string),
        reason: source.to_string(),
    }
}
