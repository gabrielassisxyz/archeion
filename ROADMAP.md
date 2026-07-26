# Roadmap

Direction, not a schedule. It answers three questions for someone arriving at the repo: what works, what is missing, and what will not be built.

## What exists

- The crawl engine decision. Two candidates were benchmarked against the same seed set under identical limits (200 pages per seed, depth 2, concurrency 16). The Rust engine produced roughly twice the page coverage of the Go comparator at the cost of wall-clock time, and it surfaced non-200 responses into the manifest, which is what an archive needs for diagnostics. A per-page limit alone proved insufficient as an execution policy: single domains dominated the run.
- The engineering harness: a secret scan and a prose gate on every commit, `bin/ci` as the single gate runner, CI and a tagged release pipeline with checksums and an installer.
- The storage model. Item, capture and asset as records, response bodies addressed by content hash, and a store that writes a capture and reads it back. The archive is a directory whose files are the record: an index, when one is needed, is derived from it and disposable. [`docs/storage-model.md`](docs/storage-model.md) has the layout and the reasoning.
- The crawl boundary, and a seed captured into an archive through it. The engine reaches the archive as page events and nothing else, so it can be replaced by writing one adapter, and a run reports what it archived, what it could not address and what it lost. [`docs/crawl-boundary.md`](docs/crawl-boundary.md) has what crosses the line and why it has that shape.
- Canonicalization and dedupe. Every spelling of a page reduces to one address, so `www.example.com` and `example.com` are one item and a campaign parameter does not make a second one, while identical bytes stay one stored file however many captures reference them. [`docs/canonicalization.md`](docs/canonicalization.md) has the rules, the ones deliberately rejected, and the reason a lossy rule is safe to apply.

## What is missing

Roughly in dependency order, since each item unlocks the next.

1. **Execution policy.** Per-seed deadlines, per-domain timeout, retry and backoff. Without these a single slow domain silently consumes an entire run.
2. **Metadata extraction.** Title, author, publication date, OpenGraph and schema.org, outbound links, referenced assets.
3. **Asset capture.** The images, stylesheets and media a page needs to still make sense once the source is gone.
4. **Query and export.** An index over the collection, and an export format that outlives this tool.
5. **Readability extraction.** Article text separated from page furniture, kept alongside the raw response and never in place of it.

## Out of scope

- **A hosted service.** Local-first is the design, not a stage before a server.
- **Browser rendering as the default.** Plain HTTP is the baseline capture path. A headless browser may be added for sites that genuinely require it, as an opt-in per source.
- **AI summarization.** The durable archive and the metadata model come first. Anything derived can be recomputed later from the raw responses; the raw responses cannot be recovered later.
- **A bookmark-manager interface.** Archeion owns the archival layer, not a reading application on top of it.
- **Redistribution.** The default posture is personal archiving.
