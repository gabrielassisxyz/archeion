# Roadmap

Direction, not a schedule. It answers three questions for someone arriving at the repo: what works, what is missing, and what will not be built.

## What exists

- The crawl engine decision. Two candidates were benchmarked against the same seed set under identical limits (200 pages per seed, depth 2, concurrency 16). The Rust engine produced roughly twice the page coverage of the Go comparator at the cost of wall-clock time, and it surfaced non-200 responses into the manifest, which is what an archive needs for diagnostics. A per-page limit alone proved insufficient as an execution policy: single domains dominated the run.
- The engineering harness: a secret scan and a prose gate on every commit, `bin/ci` as the single gate runner, CI and a tagged release pipeline with checksums and an installer.

## What is missing

Roughly in dependency order, since each item unlocks the next.

1. **The storage model.** Item, capture and asset as first-class records, with the raw response addressed by content hash. Everything else is built on this.
2. **Canonicalization and dedupe.** `www.example.com` and `example.com` are one item, not two. Dedupe on both canonical URL and content hash, so a re-capture of unchanged bytes is recorded as a new capture of the same content and not as a duplicate.
3. **Execution policy.** Per-seed deadlines, per-domain timeout, retry and backoff. Without these a single slow domain silently consumes an entire run.
4. **Metadata extraction.** Title, author, publication date, OpenGraph and schema.org, outbound links, referenced assets.
5. **Asset capture.** The images, stylesheets and media a page needs to still make sense once the source is gone.
6. **Query and export.** An index over the collection, and an export format that outlives this tool.
7. **Readability extraction.** Article text separated from page furniture, kept alongside the raw response and never in place of it.

## Out of scope

- **A hosted service.** Local-first is the design, not a stage before a server.
- **Browser rendering as the default.** Plain HTTP is the baseline capture path. A headless browser may be added for sites that genuinely require it, as an opt-in per source.
- **AI summarization.** The durable archive and the metadata model come first. Anything derived can be recomputed later from the raw responses; the raw responses cannot be recovered later.
- **A bookmark-manager interface.** Archeion owns the archival layer, not a reading application on top of it.
- **Redistribution.** The default posture is personal archiving.
