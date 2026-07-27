# Roadmap

Direction, not a schedule. It answers three questions for someone arriving at the repo: what works, what is missing, and what will not be built.

## What exists

- The crawl engine decision. Two candidates were benchmarked against the same seed set under identical limits (200 pages per seed, depth 2, concurrency 16). The Rust engine produced roughly twice the page coverage of the Go comparator at the cost of wall-clock time, and it surfaced non-200 responses into the manifest, which is what an archive needs for diagnostics. A per-page limit alone proved insufficient as an execution policy: single domains dominated the run.
- The engineering harness: a secret scan and a prose gate on every commit, `bin/ci` as the single gate runner, CI and a tagged release pipeline with checksums and an installer.
- The storage model. Item, capture and asset as records, response bodies addressed by content hash, and a store that writes a capture and reads it back. The archive is a directory whose files are the record: an index, when one is needed, is derived from it and disposable. [`docs/storage-model.md`](docs/storage-model.md) has the layout and the reasoning.
- The crawl boundary, and a seed captured into an archive through it. The engine reaches the archive as page events and nothing else, so it can be replaced by writing one adapter, and a run reports what it archived, what it could not address and what it lost. [`docs/crawl-boundary.md`](docs/crawl-boundary.md) has what crosses the line and why it has that shape.
- The execution policy. A seed carries a wall-clock deadline, a ceiling on one request and a retry budget, so no single host can own a whole run and a run that ends early says whether it ran out of pages, of time, or of patience with a disk. [`docs/crawl-boundary.md`](docs/crawl-boundary.md) has where each of the three is enforced and why the deadline is enforced twice.
- The guards on the fetch path. A seed cannot point at loopback, a private range, link-local or a cloud metadata service unless the run asks for it, a redirect is screened, bounded, and recorded rather than followed once it leaves the host the run was pointed at, one response is capped at sixty-four megabytes so an endless one cannot take the run down with it, and a body that arrived short is recorded as short instead of passing for the whole page. [`docs/crawl-boundary.md`](docs/crawl-boundary.md) has each guard, what it does not cover, and why the one gap that is left needs a resolving connector rather than another check.
- Metadata extraction. Every page the crawl archives is read once as it is filed: title, author, publication date, description, the OpenGraph and schema.org tags behind them, the address the page claims for itself, its outbound links and the subresources it references. The reading is stored beside the capture rather than inside it, so a better extractor can rewrite the whole derived layer later without touching a single recorded response. [`docs/metadata-extraction.md`](docs/metadata-extraction.md) has the precedence rules, the ceilings a hostile page runs into, and what was left out.
- Asset capture. The images, stylesheets, scripts and media a page needs to still make sense once the source is gone are fetched beside it, stored once however many pages reference them, and bounded: a count of references one capture deals with, a size for one file and for all of them together, and the run's own deadline. What the archive did not get is in the record with the reason, so an absence says whether a number would have kept the page whole. [`docs/asset-capture.md`](docs/asset-capture.md) has the numbers, the guards on an address a page chose, and what was left out.
- Readability extraction. A page that is an article is also stored as Markdown, with the navigation, the cookie banner, the share buttons and the sidebar taken out, so that a thousand sites end up in one uniform vocabulary instead of a thousand layouts. Most captures produce none, which is correct: most of the web is navigation. The scoring pass turned out to be cubic in how deeply markup nests, so a page is refused before it is scored rather than after, and the numbers behind that ceiling are in [`docs/readability.md`](docs/readability.md) along with what a generic extractor cannot do.
- Canonicalization and dedupe. Every spelling of a page reduces to one address, so `www.example.com` and `example.com` are one item and a campaign parameter does not make a second one, while identical bytes stay one stored file however many captures reference them. [`docs/canonicalization.md`](docs/canonicalization.md) has the rules, the ones deliberately rejected, and the reason a lossy rule is safe to apply.

## What is missing

Roughly in dependency order, since each item unlocks the next.

1. **Query and export.** An index over the collection, and an export format that outlives this tool.
2. **Per-host extraction rules.** A generic scorer reads markup that follows convention and fails on sites with a layout of their own. The escape hatch is a per-host override saying which subtree is the article and which selectors are furniture, and it waits for real sites to define the shape of the rule rather than being designed against imagined ones.
3. **A pass over an existing archive.** Re-reading stored responses with a better extractor, and fetching the subresources a capture already on disk never got. Both work from records the archive already holds, so neither needs a crawl.

## Out of scope

- **A hosted service.** Local-first is the design, not a stage before a server.
- **Browser rendering as the default.** Plain HTTP is the baseline capture path. A headless browser may be added for sites that genuinely require it, as an opt-in per source.
- **AI summarization.** The durable archive and the metadata model come first. Anything derived can be recomputed later from the raw responses; the raw responses cannot be recovered later.
- **A bookmark-manager interface.** Archeion owns the archival layer, not a reading application on top of it.
- **Redistribution.** The default posture is personal archiving.
