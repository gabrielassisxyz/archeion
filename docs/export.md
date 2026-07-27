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

The command prints the number of notes it wrote. If part of the archive is unreadable, export still writes the intact items, warns about the damaged paths and exits non-zero.

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
---
```

`title`, `author`, `site_name`, `language`, `published_at` and `excerpt` are `null` when the archive has no value for them. `canonical_url` comes from the item record. `captured_at` comes from the capture record. `word_count` and `excerpt` come from the article record. `published_at` uses the parsed timestamp from the metadata record when it has one, otherwise it uses the raw publication date string the page supplied.

Timestamp fields are RFC 3339 strings. The examples above have whole-second precision because those records do, but the export preserves fractional seconds when the stored record has them.

The YAML emitter is local to this command. String values are always double quoted, and quotes, backslashes, newlines, carriage returns, tabs and control characters are escaped. The schema is small and fixed, so a general YAML dependency would add maintenance and audit surface without buying a capability the command needs.

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
