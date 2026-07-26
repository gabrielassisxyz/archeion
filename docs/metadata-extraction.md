# Metadata extraction

What is read out of a captured page, where the answer is stored, and which of the rules below are judgement calls rather than facts.

The premise is the one the storage model already rests on: **the response is the only thing that cannot be recovered, and everything derived from it can.** A page that is gone from the web is gone; a title read wrongly out of a page still in the archive is a bug that a later pass fixes for free. Every decision here follows from that asymmetry.

## Where it is stored

Beside the capture, not inside it.

```
items/<host>/<item-id>/captures/<capture-id>.json           the capture, what was observed
items/<host>/<item-id>/captures/<capture-id>.metadata.json  the reading of it, derived
```

Putting the fields on the capture record would have been one file fewer to read. It was rejected because the two have different lifetimes. A capture record is a statement about a fetch that happened, and rewriting it to improve a derived value means editing the archive's own evidence. A separate file makes the derived layer exactly what it is: rewritable in bulk by a better extractor, deletable in bulk without losing anything, and absent without meaning the archive is broken.

Three states are all ordinary, and none is an error:

- **A reading exists.** The capture was a page and the extractor read it.
- **No reading exists.** The capture is not a page, it was archived before this existed, or the derived layer was deleted on purpose.
- **A reading exists from an older extractor.** `extractor_version` says so. It is stale, not wrong.

`extractor_version` is bumped when the meaning of a field changes or a rule that fills one changes, not when a field is added. An added field is simply absent from older records, which already reads correctly as "that extractor did not look for it".

Adding this file to an archive is a compatible change and does not move the format version. A reader of format 1 that does not know about it walks past it, and `list_captures` still sees captures: the suffix makes the file stem no longer the shape of a capture id.

## What is read

- **Resolved fields**: title, description, author, site name, language, publication date. Each carries the source that produced it.
- **The address the page claims for itself**, from `<link rel="canonical">`.
- **Every `<meta>` tag**, as written.
- **Every JSON-LD block that parsed**, as written.
- **Outbound links**, absolute, deduplicated, each with its `rel` and whether it stays on the host.
- **Referenced subresources**, absolute, deduplicated, each with the role it was referenced in: image, stylesheet, script, media, icon.

The raw `<meta>` list and the JSON-LD blocks are kept on top of the resolved fields, and that is deliberate duplication. They are small, they answer "where did that come from" without re-reading the body, and they let a field nobody has thought of yet be recovered from records already written.

Nothing here has been fetched. The subresource list is an inventory of what the page referenced, which is the input a later asset capture works from.

## The precedence rules, and why each one is that way

Four tags routinely claim the same field and disagree. The order is applied uniformly and recorded on every value it produces, because these are judgements: without the source, a title that came out wrong offers nothing to look at.

| field | order |
|---|---|
| title | `og:title`, `twitter:title`, schema.org `headline` then `name`, `<title>` |
| description | `og:description`, `twitter:description`, schema.org `description`, `<meta name="description">` |
| author | schema.org `author`, `<meta name="author">`, `article:author` |
| site name | `og:site_name`, schema.org `publisher` |
| language | `<html lang>`, `og:locale` |
| publication date | `article:published_time`, schema.org `datePublished`, `<meta name="date">` |

- **Sharing metadata beats the document's own tags** for the title and the description, because `<title>` carries site furniture that the page never meant as its name: a trailing site name, a section, a separator. `og:title` is what the author wrote for a card.
- **The structured form wins for the author**, because it names a person. `<meta name="author">` is free text that sites fill with a byline, a company or a CMS user id, and `article:author` is most often a profile URL rather than a name, which is why it is last.
- **`<html lang>` wins for the language**, because it is the one a browser acts on, so it is the one an author notices being wrong.
- **The first of a repeated tag wins.** A page carrying `og:title` twice meant the first one, and a document with a second `<html>` tag, which template concatenation produces by accident, keeps the language the first one declared.
- **A `<title>` inside an inline `<svg>` is not the page's.** It is that graphic's accessible name, and a logo sits in the header of most pages, so document order would otherwise offer it first. The ancestor is what separates the two: the element's namespace does not, because an SVG `<title>` is an HTML integration point and the parser reports it in the HTML namespace exactly as it reports the page's own.

JSON-LD is read one level in: a bare object, a list of them, or a `@graph` holding the list. Nothing deeper is followed, because a field found at an arbitrary depth belongs to some sub-entity of the page and not to the page.

Within that level, the nodes that describe the site rather than the page are not read for the page's own fields: a `WebSite`, a `BreadcrumbList`, an `Organization`, a `Person`, a list or a navigation element. A graph almost always opens with one of them and every node answers to the same field names, so the first node holding a `name` is usually the name of the site. `WebPage` is not among them, because that node is the page. When a page's only structured data describes the site, it is read anyway: the site's name beats nothing. Properties are unaffected, since a publisher is a field on the page's own node.

## Dates

A publication date is stored twice, as the page wrote it and as an instant, and neither replaces the other. The parse is what makes the field sortable; the raw string is what survives a format this build cannot read yet. Dropping the raw form would lose the only evidence that the page carried a date at all.

A form written without an offset is read as UTC, and a form written without a time as midnight UTC. That is an assumption, it can be off by a day at the edges, and the raw string beside it is what keeps that recoverable.

## URLs

Every reference is resolved against `<base href>` when the page has one and against the address the capture came from otherwise, and then:

- **Anything that is not http or https is dropped.** A `javascript:`, `mailto:` or `data:` reference names something no capture can fetch, and keeping them would leave every consumer of the list to filter them again.
- **The fragment is dropped**, for the reason canonicalization drops it: it is resolved by the client against bytes the server already sent, so two links differing only there name one fetch.
- **The list is deduplicated**, links by address, subresources by address regardless of the role they appeared in, since the same file referenced twice is one fetch.

These are not canonicalized. Canonicalization decides the address an item is filed under, and applying it here would record a page's links as something other than what the page wrote. The rules are applied when a link becomes an item, not before.

The address from `<link rel="canonical">` is recorded and never acted on. Letting a remote document's claim about itself decide where it lands in the archive would hand it control over the layout of the tree.

## Hostile input

An archived page is hostile input for as long as it exists, not only while it is being fetched. The parse runs on remote markup at capture time and again on every later pass, so the guards are properties of the extractor rather than of the moment it runs.

- **Every collection has a ceiling**: 4 KiB per text field, 256 `<meta>` tags, 16 JSON-LD blocks and 64 KiB of them in total, 2048 links, 2048 subresources, 2048 bytes per URL.
- **Reaching any of them is recorded**, in the same spirit as a truncated body. A record that says it holds all the links is a different claim from one that holds as many as the extractor was willing to keep, and a page whose only link was too long to keep must not read as a page that linked nothing. A `<meta>` content that was cut is recorded apart from the list of tags stopping short, because every tag being present and one of them holding less is a different statement.
- **The parser is a streaming one.** It reads tokens out of the document in place rather than building a tree, so the parse costs the page and not a multiple of it.
- **Malformed markup is read as far as it goes** rather than refused. Strictness exists so a rewriter never emits markup whose meaning it guessed at, and this only reads. An unterminated `<title>` swallowing the rest of the document is not a bug to fix: a browser reads it the same way, and the ceiling on the field is what bounds the damage.
- **A JSON-LD block that is not JSON is dropped.** The record would hold a string nothing can read, and the body it came from is still in the archive.

## Encoding

The bytes are decoded before anything is parsed, and the rule is the standard's order: a byte order mark, then the `charset` the response declared, then a `<meta charset>` in the first kilobyte, then UTF-8.

An occurrence of the word only counts inside a `<meta>` tag, and every occurrence in that first kilobyte is tried rather than the first. Both are needed against ordinary pages: a stylesheet linked as `/s.css?charset=utf8` names a real encoding and would otherwise decide the document's, and a comment or a description mentioning the word would otherwise end the scan before the real declaration. This stays a substring scan and not a tokenizer, which the standard's own prescan is, since the remaining disagreements are pages that carry the word inside a `<meta>` tag ahead of their own declaration.

There is no statistical detection. A wrong guess writes a mangled title into a record that outlives the page, and it is indistinguishable from a right one once stored.

Decoding is a separate step from parsing on purpose. It keeps the rule auditable on its own, it survives a change of parser, and it keeps a page in an encoding the streaming parser refuses outright, UTF-16 among them, readable for metadata.

## What was deliberately left out

- **Article text.** Separating prose from page furniture is a different problem with a different failure mode, and it belongs beside this record rather than inside it.
- **Frames.** An `<iframe>` names a document, not a subresource. Whether to capture one is a decision about the scope of a crawl.
- **Telling inert content from live content.** What a `<template>` holds is recorded like anything else, even though the page never loads it until something clones it. That is deliberate: a template is markup the page ships and its script commonly instantiates, so an archive that skipped it would lose assets a restored page needs. Over-recording costs a fetch; under-recording is permanent.
- **`preload` and `alternate` links.** The first names bytes the page may never use, the second names a different document.
- **Re-extraction over an existing archive.** The record carries the version that would drive it, and the pass itself waits for a second extractor version to exist.
