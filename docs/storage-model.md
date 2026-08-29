# The storage model

An Archeion archive is a directory. Everything below is a description of what is in that directory, why it is shaped that way, and what was deliberately not done.

The premise the whole design hangs on: **the archive outlives the tool**. Raw responses can never be recovered once a site is gone, while every derived thing, metadata, readability text, an index, can be recomputed from them. So the format optimizes for being readable by a stranger with `find`, `jq` and a hex editor, and treats speed of query as a problem to solve later and elsewhere.

## The three records

- **Item.** A canonical URL, and the identity everything else hangs off. One item exists per canonical URL, forever, no matter how many times it is fetched. The rules that reduce every spelling of a page to that one URL are in [`canonicalization.md`](canonicalization.md); this layer only ever sees the result.
- **Capture.** One fetch of an item at a point in time: status, headers, media type, the address of the body that came back. An item has as many captures as it has been fetched.
- **Asset.** A subresource a capture needed to still make sense: an image, a stylesheet, a font.

The rule that decides where a field belongs: **anything that can differ between two fetches of the same URL belongs to the capture, and only what stays true across all of them belongs to the item.** A title, a status and a body hash are capture fields, because tomorrow the page may have a different title and answer 404.

An asset is stored inside its capture record rather than as a record of its own. It has no meaning apart from the page that referenced it, and its bytes are already shared through the content-addressed store, so a separate file would add a level of indirection and no information. That decision is cheap to revisit if assets ever need to be queried on their own.

An asset arrives at the capture already stored, as a record, rather than as bytes the write of the capture puts away. It is the one place the store asks the caller to write in two steps, and the reason is that assets are shared: one stylesheet belongs to every page of a site that links it, so the run stores those bytes once and hands the same record to each of those captures. Holding the bytes until the capture is written would mean carrying a site's subresources in memory for as long as a run lasts, to write files that are already on disk. [`asset-capture.md`](asset-capture.md) is the pass that does that.

## The layout

```
<archive>/
  archeion.json                                     format name and version
  extraction-rules.json                             per-host extraction rules, written by hand
  owed.json                                         addresses no run has archived yet, and why
  blobs/sha256/<ab>/<cd>/<hash>                     response bodies, addressed by content
  items/<host>/<item-id>/item.json                  one item
  items/<host>/<item-id>/captures/<capture-id>.json one fetch of it
  items/<host>/<item-id>/captures/<capture-id>.metadata.json  what was read out of that fetch
  items/<host>/<item-id>/captures/<capture-id>.article.md     its prose, if it had any
  items/<host>/<item-id>/captures/<capture-id>.article.json   what is known about that prose
  items/<host>/<item-id>/captures/<capture-id>.article-refused.json  prose that was not kept as one
  items/<host>/<item-id>/captures/<capture-id>.article-not-found.json  a page read and judged not to be an article
  items/<host>/<item-id>/captures/<capture-id>.assets-recovered.json  subresources fetched after the capture
```

A real archive holding a single capture of one page, with one stylesheet as its asset:

```
<archive>/archeion.json
<archive>/blobs/sha256/10/e1/10e1d06d3065fd8c1141a3c1c077215f74cf029d1afcf55c3a84e59f3a8d304b
<archive>/blobs/sha256/3c/70/3c70219d84c51d5e82c7fb8528de79e108aaf3d6cd4daaf0f8edb50f04ab2159
<archive>/items/blog.example.com/cc307a5a.../item.json
<archive>/items/blog.example.com/cc307a5a.../captures/20260725T140322Z-16384aa6.json
```

`extraction-rules.json` is the one file here a person writes rather than the program, and the only one whose absence is ordinary. It holds what a host has to be told about where its prose lives, and it sits in the archive because a re-pass over these stored responses has to read it to produce the same articles again. It is at the root rather than under `items/`, which the walk reads as records and nothing else. [`docs/readability.md`](readability.md) has the format and what a broken one costs.

It is also the one file that may be there before the archive is. An archive is created in a directory that is empty, absent, or holds this file and nothing else, so a host's rules can be written first and applied on the first capture rather than only on a re-pass after it. That tolerance is bounded by ownership: a name joins it only if it is a file this project itself writes, meaning one `Archive` answers a path for.

### What the archive is owed

A page response the host refused, status 400 or above, is not an item: an item is a page that was served, and a capture that never happened records nothing in `items/`. What replaces it is `owed.json`, a record at the root, beside `extraction-rules.json` and for the same reason: the walk under `items/` reads that directory as pages that were served, and an address with no capture behind it is not one. This rule is the page's own: a subresource is stored under whatever status it answered with, refusals included, since [`asset-capture.md`](asset-capture.md) reasons about what a page needed to still make sense and not about what the host said when it sent it.

```json
{
  "format": "archeion-owed",
  "version": 1,
  "addresses": [
    { "url": "https://blog.example.com/2026/07/a-page", "reason": "refused", "status": 429, "retry_after": "120" },
    { "url": "https://blog.example.com/2026/07/gone", "reason": "refused", "status": 404 },
    { "url": "https://cdn.example.net/timeout", "reason": "no_response", "detail": "error sending request: operation timed out" },
    { "url": "javascript:void(0)", "reason": "unaddressable", "detail": "javascript:void(0) is a javascript URL, and the archive captures over http and https" },
    { "url": "http://169.254.169.254/latest/meta-data", "reason": "inside_a_network" },
    { "url": "https://blog.example.com/2026/07/never-fetched", "reason": "never_followed" }
  ]
}
```

`format` and `version` exist so a reader who has found this file on its own, with nothing else open, can still tell what it is and whether the shape it is reading is one this build understands, the same reasoning `archeion.json` follows for the archive as a whole.

Every reason matches a field `CaptureRun` already carries in memory while a run is in progress, and its own comment says every URL the engine reported is in exactly one of them: `refused` for a status the host itself sent, `no_response` for a URL nothing answered, `unaddressable` for one the canonical rules refuse, `inside_a_network` for an address that resolved somewhere a run without `--allow-private-addresses` had no business reaching, and `never_followed` for a link the crawl discovered and never fetched at all. A refused address is named by its canonical spelling, the same one the page would have been filed under had it been served, and not by the literal address the response carried: that is what lets this file's own write compare an address against `Item`'s identity, below, to tell a page that is now archived from one still owed.

`retry_after` is present only when the host sent the header on a refusal, carrying its value exactly as it arrived. Its absence is the record saying so, rather than a value nothing sent standing in for one. Nothing else about a refused response is kept: the body is discarded, on the measurement in `arch-9j5` that a run's refusals are typically identical short error pages carrying nothing worth an archive slot.

The file is merged, not overwritten. An archive is not one seed: `items/<host>/...` is sharded by host precisely because one archive holds however many hosts were ever pointed at it, and a run against one of them says nothing about whether another's debt is still owed, so replacing the whole file with only this run's list would erase what an unrelated run left here. A write reads what is already on file, drops every address this run's own `archived_urls` says it just turned into an item, adds whatever this run newly learned, and keeps one entry per address if the same one appears twice.

Only a `refused` entry is guaranteed to be dropped once it is archived. Its address is canonical, so it compares directly against `archived_urls`; the other four reasons carry whatever address the engine reported at the point the run gave up on it, before this project's own canonicalization had a reason to run, and are not guaranteed to match the spelling a later capture of the same page actually files under. An address stuck in one of those four keeps reappearing in this file until a run happens to archive it under the exact spelling recorded here.

### Bodies are addressed by their content

A response body is written to a file whose name is the SHA-256 of the bytes in it. The hash covers the body and nothing else: not the headers, not the URL, not the time of the fetch.

- **Dedupe falls out of it.** Ten captures of a page that never changed reference one file. A re-capture of unchanged bytes is a new capture of the same content, which is what an archive should record, and not a duplicate.
- **Shared assets stay single.** One stylesheet used by two hundred pages of a site is one file.
- **Integrity is checked rather than assumed.** Every read rehashes the bytes and compares them with the name they were filed under, so silent corruption surfaces as an error instead of as content. The check needs no external manifest, because the name is the manifest.
- **No filename ever derives from remote data.** A hostile `Content-Disposition` header, a path in a URL, a redirect target: none of them can influence where bytes land, because the name is always the hash. For a tool whose whole job is fetching attacker-influenced URLs, that closes the obvious hole rather than trying to sanitize it.

An archive is untrusted input for as long as it exists, not only while it is being written, so a hash read back out of a stored record is parsed before it is used. Anything that is not 64 lowercase hex digits is refused, which is what keeps an edited record from turning the read path into an arbitrary file read. The same holds for the capture ids in the next section.

The two intermediate directories are the first four hex digits of the hash. A flat directory of hundreds of thousands of entries is slow to list and unpleasant to work in on most filesystems, and two levels of two digits cap any one directory at a few hundred entries for a collection of realistic size.

### Items are sharded by host, named by hash

The item directory name is the full SHA-256 of the canonical URL. Deriving it means dedupe by canonical URL is a path lookup and needs no index, and two captures of the same page cannot land in two places.

The host above it exists for the human: it makes the tree browsable by domain, and it makes "everything archived from this site" a directory rather than a query. It is the one part of a path built from remote input, so it is validated instead of escaped. A domain must be ASCII alphanumerics, dots, dashes and underscores, and may not start with a dot, which also rules out `.` and `..`. An IPv6 host has its colons replaced, since a colon is not a legal filename character on Windows. Anything else is refused, on the grounds that a host too strange to be a directory name is also too strange to archive.

The full hash is used rather than a truncated prefix. A collision in a truncated item id would silently merge two unrelated URLs into one item, which is data loss that no later check would catch, and the only cost of the full form is a long directory name.

### Captures are named by the instant they were fetched

A capture id is a UTC timestamp in basic ISO 8601 form plus the first eight hex digits of a fingerprint of the response, for example `20260725T140322Z-16384aa6`. Sorting the directory listing therefore sorts the history. The basic form is used rather than the extended one because `14:03:22` contains colons, which the filesystem rules above already exclude.

The fingerprint covers the requested and final URLs, the status, the media type, the response headers, the hash of the body, and for every asset the same fields over again including the hash of its own body. An asset contributes what it is and not only what it holds, because hashing the bytes alone would name two captures alike whenever the bytes match and the records do not, and a page referencing two addresses that serve identical bytes, which every tracking pixel on a site is, is enough for that. Naming a capture after its body alone would be enough only if bytes were the only thing that could differ, and they are not: a retry inside the same second can return identical bytes under a different status, and the second capture would then be filed on top of the first and lost. Two fetches that agree on every recorded field are, by that definition, the same capture, so they share a name deliberately and re-writing one is idempotent rather than duplicating it.

The fields are hashed length-prefixed rather than serialized to JSON first. A capture id is a filename that has to stay stable forever, and a renamed field or a different formatter would otherwise rename every capture written after the change.

### The item record

```json
{
  "id": "cc307a5a1382f3162fe19efb21cfbd50d9d4f5a0a33c69723b1ec3a28b6aa9c2",
  "canonical_url": "https://blog.example.com/2026/07/a-page",
  "first_captured_at": "2026-07-25T14:03:22Z",
  "last_captured_at": "2026-07-25T14:03:22Z"
}
```

This record is thin on purpose, and one field carries it. The directory it lives in is a hash, so `canonical_url` is the only path back from the tree to the addresses it was built from. Without it, reading an archive means opening capture records and guessing. The two timestamps are a cheap convenience on top of that, not the reason the file exists.

Both ends of that window widen as captures arrive, rather than only the last one moving. Captures are not always written in the order they were fetched, and a backfilled older capture has to move the start.

### The capture record

```json
{
  "id": "20260725T140322Z-16384aa6",
  "item_id": "cc307a5a1382f3162fe19efb21cfbd50d9d4f5a0a33c69723b1ec3a28b6aa9c2",
  "requested_url": "http://blog.example.com/2026/07/a-page",
  "final_url": "https://blog.example.com/2026/07/a-page",
  "status": 200,
  "media_type": "text/html; charset=utf-8",
  "response_headers": [
    { "name": "content-type", "value": "text/html; charset=utf-8" },
    { "name": "etag", "value": "\"9c1e-63f\"" }
  ],
  "body": { "sha256": "3c70219d84c5...", "byte_len": 34 },
  "fetched_at": "2026-07-25T14:03:22Z",
  "assets": [
    {
      "requested_url": "https://blog.example.com/style.css",
      "final_url": "https://blog.example.com/style.css",
      "status": 200,
      "media_type": "text/css",
      "body": { "sha256": "10e1d06d3065...", "byte_len": 29 }
    }
  ]
}
```

`requested_url` and `final_url` differ exactly when the fetch redirected. Response headers are a list and not a map, because a map drops the repeated ones, and `set-cookie` and `link` repeat.

**One header is kept without its value.** A `set-cookie` reaches the record as `(dropped by the archive)`, once per occurrence, so the record says the header was sent and how many times and never what it set. It holds with a session and without one, because a rule that depended on which kind of run was in progress would be wrong once, and an archive written under the wrong answer cannot be repaired by changing the answer later. Anonymously the same header is a tracking identifier rather than a credential, and this is a repair and not a precaution: 247 of the 250 captures of one publication were already holding 930 of these values between them. The cost is stated rather than hidden, and it is the only place this format knowingly pays it: a stored response is no longer byte for byte what arrived. It is paid at the crawl boundary, so nothing above that line ever holds a session token, and [`crawl-boundary.md`](crawl-boundary.md) makes it part of what an engine adapter owes the archive.

Because the response headers are part of the capture fingerprint, the value that is hashed is the dropped one. Two fetches of a page that differ only in what their cookies said are therefore the same capture, which is what the definition above already says about two fetches alike in every recorded respect. Nothing on disk moves: a fingerprint is computed when a capture is written and never again.

A capture that did not get everything the page referenced carries one more field, and a capture that got everything does not, which is also the shape of every record written before the field existed:

```json
  "assets_missed": [
    { "url": "https://blog.example.com/hero.png", "reason": "too_large", "byte_len": 41943040 },
    { "url": "https://cdn.example.net/app.js", "reason": "no_response", "detail": "error sending request: dns error" }
  ]
```

The reason is the point of the field. Comparing what the derived record says the page referenced against what the capture holds already shows that something is gone, as long as the same extractor wrote both; only this says whether the archive refused it, and therefore whether a different number would have kept the page whole. It stays out of the capture fingerprint, and can only stay out because each asset contributes its address to that fingerprint as well as its bytes: two captures that agree on every asset they hold, address by address, referenced the same subresources and therefore missed the same ones. Hashing a failure would mean hashing a reason string that varies between two attempts at the same failure.

A capture whose run departed from the archive's default policy carries one more, and a capture from an ordinary run does not:

```json
  "policy_departures": ["session"]
```

One field with one entry per departure, rather than a boolean per decision, so the next thing a run can be told to do differently costs one entry and no new field. `session` says the request carried a subscription the operator holds, which makes the capture a different observation of the page from an anonymous one: the body is longer and nothing else in the record would say why. What the credential was is not here and is nowhere else either.

It describes the capture and not the whole run, because a session belongs to one origin: a run holding one for a publication asks another host's pages without it, and those captures carry nothing. It is decided from `final_url` rather than from `requested_url`, since the field says what the stored response is and the HTTP client drops the credential when a redirect leaves its origin: an apex redirecting to its `www` form would otherwise store the teaser an anonymous reader was served under a record claiming a paying one. It stays out of the capture fingerprint, for the reason `assets_missed` does, and adding it needed no format version bump under the rule at the end of this document, since a field added to a record is a compatible change.

Records are JSON, pretty printed. The format costs some bytes against the raw bodies it sits beside, and buys a file that `diff` and `grep` can work with and that any language will still parse in twenty years.

### The derived records

Beside each capture there may be a `<capture-id>.metadata.json`, holding what was read out of that response: its title, its author, its date, the tags behind them, its outbound links and the subresources it referenced. [`metadata-extraction.md`](metadata-extraction.md) has the fields and the rules.

Beside a capture that turned out to be an article there is also a `<capture-id>.article.md`, its prose with the navigation, sidebars and banners taken out, and a `<capture-id>.article.json` describing it. Most captures have neither, because most of the web is not an article. A capture the site served as Markdown has the same pair, holding what it published rather than anything taken out of markup, and the record says which of the two it is. [`readability.md`](readability.md) has the rules and the ceilings.

A capture that produced prose the extractor then refused to call an article gets a `<capture-id>.article-refused.json` in place of that pair: the measurements the refusal was made on, so the rule that made it can be checked against real pages later. It is one file and not a pair on purpose, since the document beside it is the claim the refusal exists to avoid making, and it stays derivable from the stored response. Only pages the extractor turned down are written here, never the many that simply held no prose.

A capture whose HTML was read and judged not to be an article gets `<capture-id>.article-not-found.json`. It is a mark rather than a document: the pass has paid the parse and found no article, so a later pass can skip the same work until the extractor version or the host rules make the answer stale. It is not written for responses that were not pages at all, since an image or a PDF has nothing the article extractor reads.

Subresources retried after the original capture are written in `<capture-id>.assets-recovered.json`. The capture record itself is not rewritten, because its id names the assets that were present when the capture was filed. A late asset, or a late answer that the asset is still missing, is a supplement to that record, not a different spelling of it. `Archive::read_capture` folds the sidecar into the returned capture, removes misses covered by recovered assets, and replaces old misses with the latest retry result, while the original capture JSON stays the observation it was.

They are separate files because they have different lifetimes from the capture. The capture record is what the archive observed and is the part that cannot be recovered; these are readings of it, and a better extractor is expected to replace every one of them without touching a single recorded file. A capture with no derived file beside it is an ordinary state: the response may not be a page at all, or nothing has read it yet.

**The prose is a document and not a field.** It could have been a string inside the record beside it, and it is not, because a JSON string escapes every newline and quote in it: the artifact whose whole promise is that it outlives this tool would then be readable only by a program that decodes it first. A `.md` file opens in anything.

Their presence changes nothing else in this layout. Reading an archive that has none works exactly as before, the format version does not move, and the listing that walks an item's captures still sees captures, because the extra suffix keeps the file stem from being the shape of a capture id.

## Writing

Two properties matter more than throughput, since a capture is cheap and re-fetching it may be impossible.

- **Every file lands atomically.** A record is written to a temporary file in its destination directory, flushed with `fsync`, then renamed into place, and the directory itself is flushed after the rename. A reader sees the old record or the whole new one, never a half-written one. Syncing the file alone would make the content durable but not the name it was given, and a record whose name did not survive is a record the archive lost.
- **The write order is chosen, not incidental.** Bodies go first, since a run cut short then leaves an unreferenced blob, which costs disk space and nothing else, rather than a record pointing at bytes that were never stored. The asset bodies go before the page's own, one step earlier and by the same rule, because the capture record has to name them. The item record goes next, because it carries the canonical URL that the hashed directory name does not: a capture written into a directory with no `item.json` beside it cannot be read back to the address it came from. The capture record goes last.

## The index

There is none, and that is the decision, not an omission.

The tree is the record. An index is a derived, disposable thing: a database built by walking the archive, deleted and rebuilt whenever it is wrong or a new field is wanted. Nothing may live only in it. Until the collection is large enough for scanning to actually hurt, the tree answers the questions there are, and the archive is one less format to keep consistent.

The alternative, holding the records in SQLite and keeping only blobs on disk, was rejected. It buys transactions and fast joins that nothing needs yet, and it costs the property the whole design exists for: the metadata would live in one binary file, in a schema from a particular year, and losing that file would leave a directory of anonymous hashes.

## Known limits

- **One writer at a time.** The item record is a read, a widening and a write, with no lock around it. Two processes capturing the same item at the same moment can lose one end of the window. Nothing else in the layout is order sensitive, and a single local operator is the case being built for, so the fix waits for a second writer to actually exist.
- **Bodies are read whole.** Nothing streams and nothing is capped, so a body costs its full size in memory when it is read back. The bound belongs where bytes first arrive, which is the fetch path, and it is noted here so that it is not mistaken for a property this layer already provides.

## What was deliberately left out

- **The redirect chain.** `requested_url` and `final_url` already record that a redirect happened. The intermediate hops matter for the fetch policy work, which is where the field will be added when there is something to write into it.
- **Readability text and an item-level tag set.** Both derived from bodies that are already stored, and both cheaper to add once the extraction that produces them exists. Extracted metadata was the first of these to arrive: it is a file of its own beside each capture, and [`metadata-extraction.md`](metadata-extraction.md) has why it is not a field on the capture record.
- **Deleting anything.** Nothing in this layer removes a capture or reclaims an unreferenced blob. A retention policy is a decision about what an archive is allowed to forget, which deserves its own design rather than a default.

## The format version

`archeion.json` names the format and its version. Opening a directory that holds something else fails rather than scattering records into it, and opening an archive written by a newer format fails rather than misreading it. A field added to a record is a compatible change; a field whose meaning changes is a version bump plus a migration that rewrites the tree.

**One field's meaning changed without a bump, and this is the exception rather than an oversight.** `response_headers` went from every value as it arrived to every value except what a `set-cookie` said, which is a change of meaning by the rule above. What a bump plus a migration buys a reader is the ability to tell a record written under the old meaning from one written under the new, and here the record already says so itself: a dropped value is the literal string `(dropped by the archive)`, per occurrence, which is greppable and which no host has ever sent. So the version stays at 1, because moving it would cost a rewrite of every tree in existence and tell a reader something the record in their hand already told them. Every archive written before this keeps the values it holds; nothing rewrites a capture record. That reasoning is what makes it an exception: a field whose old and new meanings are indistinguishable inside a single record is still a bump and a migration.
