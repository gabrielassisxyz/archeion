# Markdown export

`archeion export <archive> <destination>` writes a disposable Markdown vault from an archive. The archive remains the record; the export is a joined projection of item, capture, metadata and article records so a Markdown application can browse the collection without knowing Archeion's storage layout.

## Format

Each exported article capture becomes one Markdown note:

```text
<destination>/<host>/<capture-date>-<slug>.md
```

`<host>` is the same validated host directory the archive already uses. `<capture-date>` is the UTC calendar date of the capture, written as `YYYY-MM-DD`. The note body after the front matter is the stored article Markdown byte for byte as text. Captures with no stored article are not exported, because there is no document body to write.

By default, one capture is considered for each item: the most recent capture in that item's history. `--all-captures` considers every capture, oldest first, so a vault can carry the page's article history when that is wanted.

The destination must resolve to an absent or empty directory. Exporting over an existing tree is deliberately not implemented, because a previous export may contain human edits and the rules for preserving, replacing or deleting those files need their own design.

The command prints the number of Markdown notes it wrote, including host indexes. If part of the archive is unreadable, export still writes the intact items, warns about the damaged paths and exits non-zero.

## Assets

Images referenced by an exported article body are copied to:

```text
<destination>/assets/<sha256>.<ext>
```

The note is rewritten to point at that file with a relative path from the note's host directory, for example `../assets/<sha256>.png`. The name is the content hash of the stored body, so the export keeps the archive's dedupe property and never derives an asset filename from remote data.

Only successful image responses are carried into the vault, and only when the article Markdown actually references the captured asset. A stylesheet, script, icon or PDF can exist in the capture record and still stay out of the export, because those files have no article-body meaning in a Markdown vault. An unreferenced captured image also stays out of the export.

The extension comes from the recorded media type through an allowlist: `image/avif`, `image/gif`, `image/jpeg`, `image/png`, `image/svg+xml` and `image/webp`. The source URL is never consulted for the extension.

If a referenced image record exists but its content-addressed body is missing or corrupt, the note is still exported and that one destination is left pointing at the original URL. The export warns about the damaged asset and exits non-zero, the same way it does for other unreadable archive state.

Destination rewriting is done by a CommonMark parser used only to locate byte spans. The exporter does not render the parser's events back to Markdown. It replaces only the destination range inside an inline image and leaves every other byte of the stored article document untouched. A text scan cannot make that guarantee: link-shaped text can appear inside code spans and fenced code blocks, titles can follow destinations, destinations can be wrapped in angle brackets, and escaped brackets or parentheses can appear inside the syntax itself. The parser decides which spans are real Markdown image destinations; the original document remains the document that gets written.

## Links

Exported notes link to each other when an inline Markdown link names another item in the same export. The destination is parsed as an absolute URL, passed through Archeion canonicalization, and looked up in the export's complete map of canonical URL to note path. When the target exists, only the destination span is replaced with a relative Markdown path to that note. The link text, title, escaping and surrounding prose stay byte identical.

The lookup is by canonical URL rather than by raw string, so `https://www.example.com/page?utm_source=x#part` resolves to an exported note for `https://example.com/page` when the canonicalization rules make those one item. A target outside the collection stays as it was. A destination that never resolved to an absolute `http` or `https` URL also stays as it was, so relative links that the archive never resolved are not guessed during export.

Resolution is deliberately two pass. The exporter first walks every selected article capture, assigns every note path and builds the canonical URL map. It writes no note body before that map is complete, because one page can cite another page that was captured later and appears later in the walk. With `--all-captures`, the map still has one path per canonical URL, so links point to the most recent exported capture for that item.

## Host Indexes

Each host directory contains `index.md`, a mechanical list of every exported note under that host. The list is ordered by capture date, with canonical URL as the deterministic tie breaker, and every entry is a normal relative Markdown link to the note file. With `--all-captures`, one item can therefore appear more than once, once for each exported capture. The index does not select, rank, group or summarize entries, because export records what the archive contains rather than deciding which page matters most.

## Front Matter

Every note starts with YAML front matter. The schema is fixed:

```yaml
---
title: "Example title"
canonical_url: "https://example.com/page"
captured_at: "2026-07-25T14:03:22Z"
published_at: "2026-07-24T00:00:00Z"
author: "J. Writer"
site_name: "Example Site"
language: "en"
word_count: 420
excerpt: "The first sentence of the article."
accessible_for_free: null
---
```

`title`, `author`, `site_name`, `language`, `published_at` and `excerpt` are `null` when the archive has no value for them. `canonical_url` comes from the item record. `captured_at` comes from the capture record. `word_count`, `excerpt` and `accessible_for_free` come from the article record. `published_at` uses the parsed timestamp from the metadata record when it has one, otherwise it uses the raw publication date string the page supplied.

Timestamp fields are RFC 3339 strings. The examples above have whole-second precision because those records do, but the export preserves fractional seconds when the stored record has them.

`accessible_for_free` carries the page's own declaration, read from its JSON-LD, about whether the content in front of a reader was accessible without paying for it. `false` is what a paywall declares, and it is the one value that says the note's prose may stop where the wall does: the archive still keeps the response, but what got exported is only as much of it as the site let a reader without a subscription see. `true` is the page declaring itself whole. `null` is `word_count` and `excerpt`'s own case again: nothing in the archive said either way, and that is not the same claim as `true`, so a reader must not treat the two as interchangeable.

The YAML emitter is local to this command. String values are always double quoted, booleans and `null` are written bare, and quotes, backslashes, newlines, carriage returns, tabs and control characters inside a string are escaped. The schema is small and fixed, so a general YAML dependency would add maintenance and audit surface without buying a capability the command needs.

## Slugs

The slug is derived by rule, not by judgment:

- **First source:** the metadata title.
- **Second source:** the canonical URL path.
- **Final source:** the first 12 hex digits of the item id.

The chosen source is lowercased to ASCII, then every run outside lowercase ASCII letters and digits becomes one hyphen. Leading and trailing hyphens are removed. The slug is capped at 80 bytes. If the result is empty, the next source is tried.

Collisions inside one host and date are deterministic. The first note keeps the base filename. A later note with the same filename appends the 12 digit item id prefix. If `--all-captures` produces another collision for the same item on the same date, the capture id is appended.

The export is the only place a filename derives from remote data. It is safe because the title and path are reduced through an allowlist before they ever become a path segment. There is no escaping step that has to recognize traversal, because dots and slashes never survive into the filename.

## Boundary

The export invents nothing and ranks nothing. A value is eligible only if it traces back to a stored record by a stated rule. If a feature would require deciding which capture, title, tag, excerpt, image or link is more important than another beyond those rules, it is not export. It belongs in a reader, sync pass or future derived layer.

## Deliberately Left Out

- **No `--format` flag.** There is one export format, so a format flag would pretend a second one exists.
- **No sync onto a previous export.** Preserving human edits and removing stale notes is a separate problem.
- **No non-image assets.** Stylesheets, scripts and other subresources belong to the captured page, not to the Markdown article projection.
- **No ranking or enrichment.** Tags, summaries, related notes and reading order require judgment that is outside this projection.
