# Roadmap

Direction, not a schedule. It answers three questions for someone arriving at the repo: what works, what is missing, and what will not be built.

## What exists

- The crawl engine decision. Two candidates were benchmarked against the same seed set under identical limits (200 pages per seed, depth 2, concurrency 16). The Rust engine produced roughly twice the page coverage of the Go comparator at the cost of wall-clock time, and it surfaced non-200 responses into the manifest, which is what an archive needs for diagnostics. A per-page limit alone proved insufficient as an execution policy: single domains dominated the run.
- The engineering harness: a secret scan and a prose gate on every commit, `bin/ci` as the single gate runner, CI and a tagged release pipeline with checksums and an installer.
- The storage model. Item, capture and asset as records, response bodies addressed by content hash, and a store that writes a capture and reads it back. The archive is a directory whose files are the record: an index, when one is needed, is derived from it and disposable. [`docs/storage-model.md`](docs/storage-model.md) has the layout and the reasoning.
- The crawl boundary, and a seed captured into an archive through it. The engine reaches the archive as page events and nothing else, so it can be replaced by writing one adapter, and a run reports what it archived, what it could not address and what it lost. [`docs/crawl-boundary.md`](docs/crawl-boundary.md) has what crosses the line and why it has that shape.
- The execution policy. A seed carries a wall-clock deadline, a ceiling on one request and a retry budget, so no single host can own a whole run and a run that ends early says whether it ran out of pages, of time, or of patience with a disk. [`docs/crawl-boundary.md`](docs/crawl-boundary.md) has where each of the three is enforced and why the deadline is enforced twice.
- The guards on the fetch path. A seed cannot point at loopback, a private range, link-local or a cloud metadata service unless the run asks for it, a redirect stays on the host the run was pointed at and is screened and bounded, one response is capped at sixty-four megabytes so an endless one cannot take the run down with it, and a body that arrived short is recorded as short instead of passing for the whole page. [`docs/crawl-boundary.md`](docs/crawl-boundary.md) has each guard, what it does not cover, and why the one gap that is left needs a resolving connector rather than another check.
- Canonicalization and dedupe. Every spelling of a page reduces to one address, so `www.example.com` and `example.com` are one item and a campaign parameter does not make a second one, while identical bytes stay one stored file however many captures reference them. [`docs/canonicalization.md`](docs/canonicalization.md) has the rules, the ones deliberately rejected, and the reason a lossy rule is safe to apply.

## What is missing

Roughly in dependency order, since each item unlocks the next.

1. **Metadata extraction.** Title, author, publication date, OpenGraph and schema.org, outbound links, referenced assets.
2. **Asset capture.** The images, stylesheets and media a page needs to still make sense once the source is gone.
3. **Query and export.** An index over the collection, and an export format that outlives this tool.
4. **Readability extraction.** Article text separated from page furniture, kept alongside the raw response and never in place of it.

## Out of scope

- **A hosted service.** Local-first is the design, not a stage before a server.
- **Browser rendering as the default.** Plain HTTP is the baseline capture path. A headless browser may be added for sites that genuinely require it, as an opt-in per source.
- **AI summarization.** The durable archive and the metadata model come first. Anything derived can be recomputed later from the raw responses; the raw responses cannot be recovered later.
- **A bookmark-manager interface.** Archeion owns the archival layer, not a reading application on top of it.
- **Redistribution.** The default posture is personal archiving.
