//! Reading a sitemap: the archive's other way to learn what a site has.
//!
//! A crawl can only reach a page something else links to, and most publishing platforms do
//! not link their own pages to each other: an index renders a handful server side and loads
//! the rest through an API a crawl has no reason to call. A sitemap is the general answer,
//! because it is an open standard nearly every site already publishes, and reading one needs
//! no code that knows what any particular platform is.
//!
//! Everything a sitemap says is untrusted, exactly like a page. It is found the way a browser
//! would find it, fetched through the same engine and the same guards a page gets, and parsed
//! by a reader that never expands an entity of its own: `quick_xml::Reader` tokenizes markup
//! without resolving a document's own `<!ENTITY>` declarations at all, so there is no billion
//! laughs to guard against here, only the ceilings below.

use std::fmt::{self, Display};

use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::Event;
use quick_xml::name::{Namespace, ResolveResult};
use quick_xml::reader::NsReader;
use url::Url;

use crate::crawl::{CrawlEngine, PageEvent, Seed};

/// The most a sitemap is allowed to name. The sitemap protocol's own limit for one file is
/// fifty thousand URLs, so a legitimate file never meets this ceiling, while an adversarial
/// one cannot force the archive to hold an array sized by whatever the far side felt like
/// sending. It bounds the list while it is still being read, not after: a body under the
/// response byte ceiling can still spell out far more than fifty thousand short entries.
const MAX_SITEMAP_URLS: usize = 50_000;

/// The sitemap protocol's own namespace. A `<loc>` belongs to a page only when it resolves
/// to this URI or to none at all; an extension such as Yoast's image sitemap declares its
/// own namespace for its `<image:loc>` entries, and the prefix it happens to use for that
/// namespace is a document-local label, not something this reader may key on.
const SITEMAP_NS_URI: &[u8] = b"http://www.sitemaps.org/schemas/sitemap/0.9";

/// What a sitemap named, once the host and count ceilings have run.
#[derive(Debug)]
pub struct SitemapListing {
    /// The address this run read, whether it was given directly or discovered.
    pub sitemap_url: String,
    /// Every `<loc>` the file named, before either filter below ran. A sitemap listing 247
    /// posts against the twelve a crawl reaches is exactly the gap this field exists to show.
    pub urls_listed: usize,
    /// What is left after both filters: on the seed's host, and inside the count ceiling.
    pub urls: Vec<String>,
    /// Listed URLs refused for naming a host other than the seed's. A sitemap is read for one
    /// host's sake, and taking an address it names for another host would let that site decide
    /// what this run fetches next.
    pub refused_off_host: usize,
    /// Listed URLs refused only because `MAX_SITEMAP_URLS` was already spent.
    pub refused_over_ceiling: usize,
}

/// Why a sitemap did not become a list of URLs. None of these end the run: the seed's own
/// capture already happened, and a sitemap that cannot be read leaves that capture standing
/// rather than turning into an archive with nothing in it.
#[derive(Debug)]
pub enum SitemapError {
    /// The sitemap, or the `robots.txt` read to find it, produced nothing this could use.
    Fetch { url: String, reason: String },
    /// The body starts with the gzip magic bytes. A compressed sitemap is a decompression
    /// bomb surface neither site this feature was measured against needs, so it is refused
    /// in words rather than fed to a parser that would only fail on it silently.
    Compressed { url: String },
    /// The root element is `<sitemapindex>`, a sitemap that lists further sitemaps rather
    /// than pages. Expanding it is a recursion this change chose not to take on: both sites
    /// it was measured against publish one plain sitemap, and a bound on how deep an index
    /// may point at further indexes is a number with nothing real to calibrate it against yet.
    SitemapIndex { url: String },
    /// The document did not parse as XML, or named no `<urlset>` this reader recognises.
    Unparseable { url: String, reason: String },
}

impl Display for SitemapError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fetch { url, reason } => write!(out, "{url} could not be read: {reason}"),
            Self::Compressed { url } => write!(
                out,
                "{url} is compressed, and this archive does not decompress a sitemap"
            ),
            Self::SitemapIndex { url } => write!(
                out,
                "{url} is a sitemap index, naming further sitemaps rather than pages, \
                 which is not read"
            ),
            Self::Unparseable { url, reason } => {
                write!(
                    out,
                    "{url} is not a sitemap this reader could parse: {reason}"
                )
            }
        }
    }
}

impl std::error::Error for SitemapError {}

/// Finds and reads the sitemap for a seed's host, or the one address given explicitly.
///
/// With no address, the `Sitemap:` directives of the host's `robots.txt` are read first,
/// case insensitively, since real files spell the directive in every case; the first one
/// found is what is read. `/sitemap.xml` is what is tried when `robots.txt` names none, since
/// that is where a browser would look next.
pub fn read_sitemap(
    engine: &dyn CrawlEngine,
    seed: &Seed,
    explicit_url: Option<&str>,
) -> Result<SitemapListing, SitemapError> {
    let sitemap_url = match explicit_url {
        Some(url) => url.to_owned(),
        None => discover_sitemap_url(engine, seed),
    };
    let body = fetch_body(engine, seed, &sitemap_url)?;
    if is_gzip(&body) {
        return Err(SitemapError::Compressed { url: sitemap_url });
    }
    let parsed = parse_locs(&body).map_err(|reason| SitemapError::Unparseable {
        url: sitemap_url.clone(),
        reason,
    })?;
    if parsed.root_is_index {
        return Err(SitemapError::SitemapIndex { url: sitemap_url });
    }

    let seed_host = Url::parse(&seed.url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned));
    let mut urls = Vec::new();
    let mut refused_off_host = 0;
    for loc in parsed.locs {
        match Url::parse(&loc) {
            Ok(parsed_url) if parsed_url.host_str() == seed_host.as_deref() => {
                urls.push(parsed_url.into());
            }
            _ => refused_off_host += 1,
        }
    }

    Ok(SitemapListing {
        sitemap_url,
        urls_listed: parsed.urls_listed,
        urls,
        refused_off_host,
        refused_over_ceiling: parsed.refused_over_ceiling,
    })
}

/// Where a sitemap is read from when the operator named no address.
///
/// This reads `robots.txt` on its own rather than depending on anything the crawl engine
/// learned from it internally, since the engine's own robots handling has no entry point
/// this crate can reach. Fetching it a second time is not a new kind of request the host
/// has not already seen: the crawl asks for it too.
fn discover_sitemap_url(engine: &dyn CrawlEngine, seed: &Seed) -> String {
    let origin = seed_origin(seed);
    let robots_url = format!("{origin}/robots.txt");
    let directive = match engine.fetch(&robots_url, seed) {
        PageEvent::Response(response) => first_sitemap_directive(&response.body),
        PageEvent::NoResponse(_) => None,
    };
    directive.unwrap_or_else(|| format!("{origin}/sitemap.xml"))
}

/// `scheme://host[:port]` for the seed, which is where `robots.txt` and the fallback
/// `/sitemap.xml` both live. Falling back to the seed's own URL keeps this total rather than
/// asking a caller to have validated the seed a second time; a seed that reached this point
/// already passed `check_seed`, so the parse here does not fail in practice.
fn seed_origin(seed: &Seed) -> String {
    Url::parse(&seed.url)
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or_else(|_| seed.url.clone())
}

/// The first `Sitemap:` directive in a `robots.txt` body, read case insensitively because
/// real files are inconsistent about how they spell it.
fn first_sitemap_directive(body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    text.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if !key.trim().eq_ignore_ascii_case("sitemap") {
            return None;
        }
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    })
}

fn fetch_body(engine: &dyn CrawlEngine, seed: &Seed, url: &str) -> Result<Vec<u8>, SitemapError> {
    match engine.fetch(url, seed) {
        PageEvent::Response(response) if (200..300).contains(&response.status) => Ok(response.body),
        PageEvent::Response(response) => Err(SitemapError::Fetch {
            url: url.to_owned(),
            reason: format!("answered with status {}", response.status),
        }),
        PageEvent::NoResponse(failure) => Err(SitemapError::Fetch {
            url: failure.url,
            reason: failure.reason,
        }),
    }
}

/// The gzip magic bytes. Read from the body rather than from the URL's spelling or a
/// declared content type, since both of those are the far side's claim and this is not.
fn is_gzip(body: &[u8]) -> bool {
    body.starts_with(&[0x1f, 0x8b])
}

/// What one parse of a sitemap's XML produced, before the host filter runs.
struct ParsedLocs {
    root_is_index: bool,
    /// Every `<loc>` encountered, capped while the file is still being read rather than
    /// after: a `Vec` this holds is bounded by `MAX_SITEMAP_URLS` at every point, never by
    /// however many entries a hostile file spells out before the ceiling is applied.
    locs: Vec<String>,
    urls_listed: usize,
    refused_over_ceiling: usize,
}

/// Whether a resolved namespace is the sitemap protocol's own, or none at all.
///
/// `Unbound` is the sloppy, unnamespaced feed: no `xmlns` anywhere in the document, which is
/// the ordinary shape on the open web and must keep working exactly as it does today. `Bound`
/// to any other URI is an extension element, whatever local name or prefix it was written
/// with; `Unknown` (a prefix used but never declared) is neither, so it is refused too.
fn is_page_namespace(resolved: ResolveResult) -> bool {
    match resolved {
        ResolveResult::Unbound => true,
        ResolveResult::Bound(Namespace(uri)) => uri == SITEMAP_NS_URI,
        ResolveResult::Unknown(_) => false,
    }
}

/// Reads every `<loc>` a sitemap names, and whether its root is a sitemap index.
///
/// This tracks only whether the reader is presently inside a `<loc>` element, which is
/// enough to find one wherever it is nested: a sitemap is a flat list by the protocol's own
/// rules, so a document nesting far deeper than that is already hostile input, and nothing
/// here builds a tree that would cost memory proportional to how deep it goes. `<loc>` is
/// text content, never an attribute, so an attribute holding an enormous value has nothing
/// in this reader to spend it on.
///
/// A document truncated by the response byte ceiling reaches EOF with elements still open,
/// which this reader accepts rather than refuses: it returns whatever `<loc>` entries closed
/// before the cut, the same trade a page cut short already gets, rather than a hard error.
/// What still refuses is markup that is ill formed on its own terms, a closing tag naming
/// something other than what it closes being the shape a truncated response never produces.
fn parse_locs(body: &[u8]) -> Result<ParsedLocs, String> {
    let text = String::from_utf8_lossy(body);
    let mut reader = NsReader::from_str(&text);
    let mut seen_root = false;
    let mut root_is_index = false;
    let mut in_loc = false;
    let mut current = String::new();
    let mut locs = Vec::new();
    let mut urls_listed = 0usize;
    let mut refused_over_ceiling = 0usize;

    loop {
        let (resolved_ns, event) = reader
            .read_resolved_event()
            .map_err(|error| error.to_string())?;
        match event {
            Event::Start(start) => {
                let name = start.local_name();
                if !seen_root {
                    seen_root = true;
                    root_is_index = name.as_ref() == b"sitemapindex";
                    if root_is_index {
                        return Ok(ParsedLocs {
                            root_is_index,
                            locs,
                            urls_listed,
                            refused_over_ceiling,
                        });
                    }
                }
                if name.as_ref() == b"loc" && is_page_namespace(resolved_ns) {
                    in_loc = true;
                    current.clear();
                }
            }
            Event::Empty(start) if !seen_root => {
                seen_root = true;
                root_is_index = start.local_name().as_ref() == b"sitemapindex";
                if root_is_index {
                    return Ok(ParsedLocs {
                        root_is_index,
                        locs,
                        urls_listed,
                        refused_over_ceiling,
                    });
                }
            }
            Event::Text(text) if in_loc => {
                current.push_str(&text.decode().map_err(|error| error.to_string())?);
            }
            Event::GeneralRef(reference) if in_loc => {
                if let Some(character) = reference
                    .resolve_char_ref()
                    .map_err(|error| error.to_string())?
                {
                    current.push(character);
                } else {
                    let name = reference.decode().map_err(|error| error.to_string())?;
                    let resolved = resolve_predefined_entity(&name)
                        .ok_or_else(|| format!("&{name}; is not a recognised entity"))?;
                    current.push_str(resolved);
                }
            }
            Event::End(end) if in_loc && end.local_name().as_ref() == b"loc" => {
                in_loc = false;
                urls_listed += 1;
                if locs.len() < MAX_SITEMAP_URLS {
                    locs.push(std::mem::take(&mut current).trim().to_owned());
                } else {
                    refused_over_ceiling += 1;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(ParsedLocs {
        root_is_index,
        locs,
        urls_listed,
        refused_over_ceiling,
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::*;
    use crate::crawl::{CrawlError, FetchFailure};

    /// A crawl engine that answers a fixed page for whichever URL is fetched, and nothing for
    /// the rest. Sitemap discovery only ever calls `fetch`, never `crawl`, so that is the
    /// only method this fake has any reason to answer.
    struct FakeHost {
        pages: RefCell<HashMap<String, PageEvent>>,
    }

    impl FakeHost {
        fn new() -> Self {
            Self {
                pages: RefCell::new(HashMap::new()),
            }
        }

        fn serving(self, url: &str, body: &str) -> Self {
            self.pages.borrow_mut().insert(
                url.to_owned(),
                PageEvent::Response(response_of(url, body.as_bytes())),
            );
            self
        }

        fn serving_bytes(self, url: &str, body: &[u8]) -> Self {
            self.pages
                .borrow_mut()
                .insert(url.to_owned(), PageEvent::Response(response_of(url, body)));
            self
        }
    }

    fn response_of(url: &str, body: &[u8]) -> crate::crawl::PageResponse {
        crate::crawl::PageResponse {
            requested_url: url.to_owned(),
            final_url: url.to_owned(),
            status: 200,
            headers: Vec::new(),
            body: body.to_vec(),
            body_truncated: false,
            fetched_at: "2026-07-25T14:03:22Z".parse().expect("valid timestamp"),
        }
    }

    impl CrawlEngine for FakeHost {
        fn check_seed(&self, _seed: &Seed) -> Result<(), CrawlError> {
            Ok(())
        }

        fn crawl(
            &self,
            _seed: &Seed,
            _on_page: &mut dyn FnMut(PageEvent) -> std::ops::ControlFlow<()>,
        ) -> Result<crate::crawl::CrawlOutcome, CrawlError> {
            unimplemented!("sitemap discovery never crawls")
        }

        fn fetch(&self, url: &str, _seed: &Seed) -> PageEvent {
            self.pages.borrow().get(url).cloned().unwrap_or_else(|| {
                PageEvent::NoResponse(FetchFailure {
                    url: url.to_owned(),
                    reason: "this fake was given nothing to answer with".to_owned(),
                })
            })
        }
    }

    fn urlset(locs: &[&str]) -> String {
        let entries: String = locs
            .iter()
            .map(|loc| format!("<url><loc>{loc}</loc></url>"))
            .collect();
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">{entries}</urlset>"#
        )
    }

    /// The Yoast shape: a page `<loc>` in the default (protocol) namespace, and one or more
    /// `<image:loc>` beside it in a namespace declared for the extension.
    fn urlset_with_images(entries: &[(&str, &[&str])]) -> String {
        let body: String = entries
            .iter()
            .map(|(page, images)| {
                let image_tags: String = images
                    .iter()
                    .map(|image| {
                        format!("<image:image><image:loc>{image}</image:loc></image:image>")
                    })
                    .collect();
                format!("<url><loc>{page}</loc>{image_tags}</url>")
            })
            .collect();
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"
                    xmlns:image="http://www.google.com/schemas/sitemap-image/1.1">{body}</urlset>"#
        )
    }

    /// The measured defect itself: waitbutwhy's sitemap put several `<image:loc>` beside every
    /// page's `<loc>`, and every one of them was queued as if it were a page. This also covers
    /// the sloppy default case, since the page `<loc>` here relies on the same default-namespace
    /// declaration every plain fixture in this file already uses.
    #[test]
    fn an_image_extension_loc_is_not_queued_counted_or_charged_to_the_ceiling() {
        let engine = FakeHost::new().serving(
            "https://example.com/sitemap.xml",
            &urlset_with_images(&[
                (
                    "https://example.com/a",
                    &["https://example.com/a-1.jpg", "https://example.com/a-2.jpg"],
                ),
                ("https://example.com/b", &["https://example.com/b-1.jpg"]),
            ]),
        );

        let listing = read_sitemap(&engine, &Seed::new("https://example.com/"), None)
            .expect("the sitemap is read");

        assert_eq!(listing.urls_listed, 2);
        assert_eq!(
            listing.urls,
            ["https://example.com/a", "https://example.com/b"]
        );
    }

    /// A prefix used but never declared anywhere in the document resolves to no namespace this
    /// reader can name, which is neither the protocol's namespace nor the "no namespace at all"
    /// case that keeps a sloppy feed working: it is refused rather than guessed at.
    #[test]
    fn a_loc_under_a_prefix_nobody_declared_is_refused_rather_than_queued() {
        let engine = FakeHost::new().serving(
            "https://example.com/sitemap.xml",
            r#"<?xml version="1.0"?>
               <urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
                 <url>
                   <loc>https://example.com/a</loc>
                   <foo:loc>https://example.com/not-a-page</foo:loc>
                 </url>
               </urlset>"#,
        );

        let listing = read_sitemap(&engine, &Seed::new("https://example.com/"), None)
            .expect("the sitemap is read");

        assert_eq!(listing.urls_listed, 1);
        assert_eq!(listing.urls, ["https://example.com/a"]);
    }

    /// The ceiling is meant to bound how many pages a hostile file can force into memory. An
    /// extension that names far more addresses than that must not trip it, because none of
    /// those addresses are pages. The images are placed on the first entry and four more real
    /// pages follow: a ceiling counting elements rather than page addresses would still have
    /// "spent" itself well before reaching them, and wrongly refuse pages this file lists.
    #[test]
    fn extension_locs_alone_exceeding_the_ceiling_do_not_trip_it() {
        let images: Vec<String> = (0..MAX_SITEMAP_URLS + 5)
            .map(|index| format!("https://example.com/image-{index}.jpg"))
            .collect();
        let borrowed: Vec<&str> = images.iter().map(String::as_str).collect();
        let engine = FakeHost::new().serving(
            "https://example.com/sitemap.xml",
            &urlset_with_images(&[
                ("https://example.com/a", &borrowed),
                ("https://example.com/b", &[]),
                ("https://example.com/c", &[]),
                ("https://example.com/d", &[]),
                ("https://example.com/e", &[]),
            ]),
        );

        let listing = read_sitemap(&engine, &Seed::new("https://example.com/"), None)
            .expect("the sitemap is read");

        assert_eq!(listing.urls_listed, 5);
        assert_eq!(
            listing.urls,
            [
                "https://example.com/a",
                "https://example.com/b",
                "https://example.com/c",
                "https://example.com/d",
                "https://example.com/e",
            ]
        );
        assert_eq!(listing.refused_over_ceiling, 0);
    }

    /// The image extension is the case measured, but nothing here may key on the string
    /// `"image:"`: a different extension, under a different prefix and a different namespace,
    /// must be excluded by the same rule.
    #[test]
    fn an_extension_loc_under_a_different_prefix_is_excluded_by_the_same_rule() {
        let engine = FakeHost::new().serving(
            "https://example.com/sitemap.xml",
            r#"<?xml version="1.0"?>
               <urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"
                       xmlns:video="http://www.google.com/schemas/sitemap-video/1.1">
                 <url>
                   <loc>https://example.com/a</loc>
                   <video:video><video:loc>https://example.com/a.mp4</video:loc></video:video>
                 </url>
               </urlset>"#,
        );

        let listing = read_sitemap(&engine, &Seed::new("https://example.com/"), None)
            .expect("the sitemap is read");

        assert_eq!(listing.urls_listed, 1);
        assert_eq!(listing.urls, ["https://example.com/a"]);
    }

    #[test]
    fn a_sitemap_named_directly_is_the_one_read() {
        let engine = FakeHost::new().serving(
            "https://example.com/feed.xml",
            &urlset(&["https://example.com/a", "https://example.com/b"]),
        );

        let listing = read_sitemap(
            &engine,
            &Seed::new("https://example.com/"),
            Some("https://example.com/feed.xml"),
        )
        .expect("the sitemap is read");

        assert_eq!(listing.sitemap_url, "https://example.com/feed.xml");
        assert_eq!(listing.urls_listed, 2);
        assert_eq!(
            listing.urls,
            ["https://example.com/a", "https://example.com/b"]
        );
    }

    #[test]
    fn a_sitemap_directive_in_robots_txt_is_the_one_read_in_any_case() {
        for spelling in ["Sitemap:", "SITEMAP:", "sitemap:"] {
            let engine = FakeHost::new()
                .serving(
                    "https://example.com/robots.txt",
                    &format!("User-agent: *\n{spelling} https://example.com/from-robots.xml\n"),
                )
                .serving(
                    "https://example.com/from-robots.xml",
                    &urlset(&["https://example.com/a"]),
                );

            let listing = read_sitemap(&engine, &Seed::new("https://example.com/"), None)
                .unwrap_or_else(|error| panic!("{spelling} was not read: {error}"));

            assert_eq!(listing.sitemap_url, "https://example.com/from-robots.xml");
        }
    }

    #[test]
    fn with_no_directive_sitemap_xml_is_tried() {
        let engine = FakeHost::new()
            .serving(
                "https://example.com/robots.txt",
                "User-agent: *\nAllow: /\n",
            )
            .serving(
                "https://example.com/sitemap.xml",
                &urlset(&["https://example.com/a"]),
            );

        let listing = read_sitemap(&engine, &Seed::new("https://example.com/"), None)
            .expect("the fallback sitemap is read");

        assert_eq!(listing.sitemap_url, "https://example.com/sitemap.xml");
    }

    #[test]
    fn a_url_on_another_host_is_refused_and_the_rest_are_kept() {
        let engine = FakeHost::new().serving(
            "https://example.com/sitemap.xml",
            &urlset(&[
                "https://example.com/a",
                "https://elsewhere.example/b",
                "https://example.com/c",
            ]),
        );

        let listing = read_sitemap(&engine, &Seed::new("https://example.com/"), None)
            .expect("the sitemap is read");

        assert_eq!(listing.urls_listed, 3);
        assert_eq!(
            listing.urls,
            ["https://example.com/a", "https://example.com/c"]
        );
        assert_eq!(listing.refused_off_host, 1);
    }

    #[test]
    fn more_urls_than_the_ceiling_are_counted_and_left_out_rather_than_taken() {
        let locs: Vec<String> = (0..MAX_SITEMAP_URLS + 5)
            .map(|index| format!("https://example.com/{index}"))
            .collect();
        let borrowed: Vec<&str> = locs.iter().map(String::as_str).collect();
        let engine = FakeHost::new().serving("https://example.com/sitemap.xml", &urlset(&borrowed));

        let listing = read_sitemap(&engine, &Seed::new("https://example.com/"), None)
            .expect("the sitemap is read");

        assert_eq!(listing.urls_listed, MAX_SITEMAP_URLS + 5);
        assert_eq!(listing.urls.len(), MAX_SITEMAP_URLS);
        assert_eq!(listing.refused_over_ceiling, 5);
    }

    #[test]
    fn a_ceiling_of_exactly_the_limit_refuses_nothing() {
        let locs: Vec<String> = (0..MAX_SITEMAP_URLS)
            .map(|index| format!("https://example.com/{index}"))
            .collect();
        let borrowed: Vec<&str> = locs.iter().map(String::as_str).collect();
        let engine = FakeHost::new().serving("https://example.com/sitemap.xml", &urlset(&borrowed));

        let listing = read_sitemap(&engine, &Seed::new("https://example.com/"), None)
            .expect("the sitemap is read");

        assert_eq!(listing.urls.len(), MAX_SITEMAP_URLS);
        assert_eq!(listing.refused_over_ceiling, 0);
    }

    #[test]
    fn a_gzip_sitemap_is_refused_with_a_message_rather_than_parsed() {
        let engine = FakeHost::new().serving_bytes(
            "https://example.com/sitemap.xml.gz",
            &[0x1f, 0x8b, 0x08, 0x00],
        );

        let error = read_sitemap(
            &engine,
            &Seed::new("https://example.com/"),
            Some("https://example.com/sitemap.xml.gz"),
        )
        .expect_err("a compressed sitemap is refused");

        assert!(matches!(error, SitemapError::Compressed { .. }));
        assert!(error.to_string().contains("compressed"));
    }

    #[test]
    fn a_sitemap_index_is_refused_with_a_message_rather_than_read_as_empty() {
        let engine = FakeHost::new().serving(
            "https://example.com/sitemap.xml",
            r#"<?xml version="1.0"?><sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
               <sitemap><loc>https://example.com/sitemap-a.xml</loc></sitemap>
               </sitemapindex>"#,
        );

        let error = read_sitemap(&engine, &Seed::new("https://example.com/"), None)
            .expect_err("a sitemap index is refused");

        assert!(matches!(error, SitemapError::SitemapIndex { .. }));
        assert!(error.to_string().contains("sitemap index"));
    }

    #[test]
    fn markup_no_parser_would_read_is_reported_rather_than_read_as_an_empty_sitemap() {
        let engine = FakeHost::new().serving(
            "https://example.com/sitemap.xml",
            // The closing tag names something other than what it closes, which is ill
            // formed rather than merely unusual, and every reader refuses it.
            "<urlset><url><loc>https://example.com/a</loc></nested></urlset>",
        );

        let error = read_sitemap(&engine, &Seed::new("https://example.com/"), None)
            .expect_err("unclosed markup is refused");

        assert!(matches!(error, SitemapError::Unparseable { .. }));
    }

    #[test]
    fn a_sitemap_no_server_answered_is_reported_rather_than_read_as_empty() {
        let engine = FakeHost::new();

        let error = read_sitemap(&engine, &Seed::new("https://example.com/"), None)
            .expect_err("nothing answered for either address");

        assert!(matches!(error, SitemapError::Fetch { .. }));
    }

    #[test]
    fn a_loc_escaping_its_own_query_string_is_read_back_unescaped() {
        let engine = FakeHost::new().serving(
            "https://example.com/sitemap.xml",
            "<urlset><url><loc>https://example.com/a?x=1&amp;y=2</loc></url></urlset>",
        );

        let listing = read_sitemap(&engine, &Seed::new("https://example.com/"), None)
            .expect("the sitemap is read");

        assert_eq!(listing.urls, ["https://example.com/a?x=1&y=2"]);
    }
}
