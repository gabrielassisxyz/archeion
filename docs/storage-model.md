# The storage model

An Archeion archive is a directory. Everything below is a description of what is in that directory, why it is shaped that way, and what was deliberately not done.

The premise the whole design hangs on: **the archive outlives the tool**. Raw responses can never be recovered once a site is gone, while every derived thing, metadata, readability text, an index, can be recomputed from them. So the format optimizes for being readable by a stranger with `find`, `jq` and a hex editor, and treats speed of query as a problem to solve later and elsewhere.

## The three records

- **Item.** A canonical URL, and the identity everything else hangs off. One item exists per canonical URL, forever, no matter how many times it is fetched. The rules that reduce every spelling of a page to that one URL are in [`canonicalization.md`](canonicalization.md); this layer only ever sees the result.
- **Capture.** One fetch of an item at a point in time: status, headers, media type, the address of the body that came back. An item has as many captures as it has been fetched.
- **Asset.** A subresource a capture needed to still make sense: an image, a stylesheet, a font.

The rule that decides where a field belongs: **anything that can differ between two fetches of the same URL belongs to the capture, and only what stays true across all of them belongs to the item.** A title, a status and a body hash are capture fields, because tomorrow the page may have a different title and answer 404.

An asset is stored inside its capture record rather than as a record of its own. It has no meaning apart from the page that referenced it, and its bytes are already shared through the content-addressed store, so a separate file would add a level of indirection and no information. That decision is cheap to revisit if assets ever need to be queried on their own.

## The layout

```
<archive>/
  archeion.json                                     format name and version
  blobs/sha256/<ab>/<cd>/<hash>                     response bodies, addressed by content
  items/<host>/<item-id>/item.json                  one item
  items/<host>/<item-id>/captures/<capture-id>.json one fetch of it
```

A real archive holding a single capture of one page, with one stylesheet as its asset:

```
<archive>/archeion.json
<archive>/blobs/sha256/10/e1/10e1d06d3065fd8c1141a3c1c077215f74cf029d1afcf55c3a84e59f3a8d304b
<archive>/blobs/sha256/3c/70/3c70219d84c51d5e82c7fb8528de79e108aaf3d6cd4daaf0f8edb50f04ab2159
<archive>/items/blog.example.com/cc307a5a.../item.json
<archive>/items/blog.example.com/cc307a5a.../captures/20260725T140322Z-16384aa6.json
```

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

The fingerprint covers the requested and final URLs, the status, the media type, the response headers and the hashes of the body and of every asset. Naming a capture after its body alone would be enough only if bytes were the only thing that could differ, and they are not: a retry inside the same second can return identical bytes under a different status, and the second capture would then be filed on top of the first and lost. Two fetches that agree on every recorded field are, by that definition, the same capture, so they share a name deliberately and re-writing one is idempotent rather than duplicating it.

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

Records are JSON, pretty printed. The format costs some bytes against the raw bodies it sits beside, and buys a file that `diff` and `grep` can work with and that any language will still parse in twenty years.

## Writing

Two properties matter more than throughput, since a capture is cheap and re-fetching it may be impossible.

- **Every file lands atomically.** A record is written to a temporary file in its destination directory, flushed with `fsync`, then renamed into place, and the directory itself is flushed after the rename. A reader sees the old record or the whole new one, never a half-written one. Syncing the file alone would make the content durable but not the name it was given, and a record whose name did not survive is a record the archive lost.
- **The write order is chosen, not incidental.** Bodies go first, since a run cut short then leaves an unreferenced blob, which costs disk space and nothing else, rather than a record pointing at bytes that were never stored. The item record goes next, because it carries the canonical URL that the hashed directory name does not: a capture written into a directory with no `item.json` beside it cannot be read back to the address it came from. The capture record goes last.

## The index

There is none, and that is the decision, not an omission.

The tree is the record. An index is a derived, disposable thing: a database built by walking the archive, deleted and rebuilt whenever it is wrong or a new field is wanted. Nothing may live only in it. Until the collection is large enough for scanning to actually hurt, the tree answers the questions there are, and the archive is one less format to keep consistent.

The alternative, holding the records in SQLite and keeping only blobs on disk, was rejected. It buys transactions and fast joins that nothing needs yet, and it costs the property the whole design exists for: the metadata would live in one binary file, in a schema from a particular year, and losing that file would leave a directory of anonymous hashes.

## Known limits

- **One writer at a time.** The item record is a read, a widening and a write, with no lock around it. Two processes capturing the same item at the same moment can lose one end of the window. Nothing else in the layout is order sensitive, and a single local operator is the case being built for, so the fix waits for a second writer to actually exist.
- **Bodies are read whole.** Nothing streams and nothing is capped, so a body costs its full size in memory when it is read back. The bound belongs where bytes first arrive, which is the fetch path, and it is noted here so that it is not mistaken for a property this layer already provides.

## What was deliberately left out

- **The redirect chain.** `requested_url` and `final_url` already record that a redirect happened. The intermediate hops matter for the fetch policy work, which is where the field will be added when there is something to write into it.
- **Extracted metadata, readability text and an item-level tag set.** All derived from bodies that are already stored, and all cheaper to add once the extraction that produces them exists.
- **Deleting anything.** Nothing in this layer removes a capture or reclaims an unreferenced blob. A retention policy is a decision about what an archive is allowed to forget, which deserves its own design rather than a default.

## The format version

`archeion.json` names the format and its version. Opening a directory that holds something else fails rather than scattering records into it, and opening an archive written by a newer format fails rather than misreading it. A field added to a record is a compatible change; a field whose meaning changes is a version bump plus a migration that rewrites the tree.
