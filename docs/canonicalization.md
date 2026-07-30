# Canonicalization

One page is linked around the web under a dozen addresses. `www.rust-lang.org` and `rust-lang.org`, the same URL with a campaign parameter glued on, the same URL with a fragment pointing at a heading. An archive that treats each of those as a separate address stores the same site once per spelling, and its history of any given page is split across all of them.

Canonicalization is the reduction of every spelling to the one address the archive keys on. It is one half of dedupe; the other half is the content hash, which makes identical bytes one stored file no matter how many captures reference them.

## The boundary

**Canonicalization decides identity, not what gets fetched.** A fetch uses the URL as it was found, and the capture record keeps both `requested_url` and `final_url`, so nothing is lost by rewriting an address here and no rule can make a page unreachable. This is what makes the lossy rules safe to apply: dropping `www` cannot break a fetch, because the fetch never reads the canonical form.

`CanonicalUrl` is the type that carries the result, and its constructor is the only way to obtain one. A type whose name claims an invariant, with a second way in that skips the rules, is a comment rather than an invariant.

The rules are idempotent: canonicalizing an already canonical URL returns it unchanged. A URL read back out of a stored record therefore goes through them again on the way in without drift, which is also what makes a stored record safe to trust as an address. This is a property to check when adding a rule, not a nicety. A rule that reduces one label per pass rather than reducing to a fixed point writes records whose stored URL no longer names the directory they were filed in, and two of those records can end up claiming the same address.

## The rules

The scheme, the case of the host, the default port, the resolution of `.` and `..` segments, the empty path and the mapping of an internationalized host onto its ASCII form are settled by the URL parser itself, in the form the WHATWG URL standard defines. It also percent-encodes bytes that are not legal where they appear, so `/a b` becomes `/a%20b`, and reads a backslash as a slash. All of it is listed here anyway, because a reader of an archive needs the whole rule set in one place and not the part this project happened to write by hand.

| rule | in | out |
|---|---|---|
| The scheme and the host are compared in lowercase. | `HTTPS://Example.COM/a` | `https://example.com/a` |
| The DNS root dot is not a second host. | `https://example.com./a` | `https://example.com/a` |
| `www` is the apex under another name, however many times it is repeated. | `https://www.www.rust-lang.org/learn` | `https://rust-lang.org/learn` |
| An internationalized host has one ASCII spelling. | `https://bücher.example/a` | `https://xn--bcher-kva.example/a` |
| The default port is not part of the address. | `https://example.com:443/a` | `https://example.com/a` |
| A port that is not the default is. | `https://example.com:8443/a` | unchanged |
| An empty path is the root path. | `https://example.com` | `https://example.com/` |
| Dot segments are resolved, not stored. | `https://example.com/a/b/../c` | `https://example.com/a/c` |
| A fragment is resolved by the client, not fetched. | `https://example.com/a#section-2` | `https://example.com/a` |
| Credentials name a requester, not a resource. | `https://someone:secret@example.com/a` | `https://example.com/a` |
| Tracking parameters identify a campaign, not a page. | `https://example.com/a?utm_source=news&id=7` | `https://example.com/a?id=7` |
| A query that was only tracking leaves no question mark. | `https://example.com/a?utm_source=news` | `https://example.com/a` |
| Parameter order is not part of the address. | `https://example.com/a?b=2&a=1` | `https://example.com/a?a=1&b=2` |
| An ampersand a page had to escape is a separator, not part of the name behind it. | `https://example.com/a?id=7&amp;utm_source=news` | `https://example.com/a?id=7` |

Two rules refuse the URL outright rather than rewriting it.

- **Only `http` and `https`.** Every other scheme is either something this archive cannot capture or, in the case of `file`, something that reads the local machine. Both arrive from remote pages by the thousand, and refusing them at the address is one guard instead of one guard per call site.
- **A host that would not survive as a directory name.** The host becomes a directory in the archive, which makes it the one place remote data could climb out of the archive root. Anything outside a conservative character set is refused rather than escaped, since an archive has no use for a host that needs escaping.

### The parameters treated as tracking

`utm_` and anything after it, plus `fbclid`, `gclid`, `dclid`, `msclkid`, `twclid`, `igshid`, `mc_cid` and `mc_eid`. Every entry is a campaign or click identifier that no server reads to decide what to serve.

The list is short and hardcoded on purpose. A configurable list is a setting with no second user to set it, and a long list is a growing set of guesses about parameters that might be significant to some site. Names are matched case insensitively.

### What sorting the query does not do

The parameters are sorted by name, stably, so repeated names keep the order they arrived in: a server reading both values of `?a=1&a=2` is reading a different request from `?a=2&a=1`.

They are also kept as the raw text they arrived as, rather than decoded into pairs and re-encoded. A round trip through key and value rewrites the escaping and turns a valueless `?print` into `?print=`, which would canonicalize a URL into one that was never requested.

### The one thing rewritten inside the query

An `href` attribute spells `&` as `&amp;`, which is what the HTML standard asks a page for, and nothing between the attribute and here decodes it. The query then splits on the literal `&` and every parameter behind the escape arrives carrying the rest of the reference on the front of its name: `amp;utm_medium` where the page wrote `utm_medium`. That name is not a name any rule below matches, so the campaign parameters a link was built out of survive the rules written to drop them, and the page is filed under an address no reader will ever type. On a thirty page run of one publication, four pages were filed a second time this way. The percent-encoded spelling, `&amp%3B`, is the same link escaped twice and arrives just as often.

The tail is therefore stripped off a parameter before anything reads its name, repeatedly, since a name escaped twice that shed one layer per pass would name a different address each time a stored record was read back. A parameter genuinely named `amp` is untouched: what is stripped is the reference plus the semicolon that ends it.

It is undone here rather than at the href it came off. An address also reaches the archive from a sitemap and from an operator's command line, and a rule about what a URL means belongs where every source of one passes. What this does not fix is the request: a fetch is aimed at the URL as it was found, so a crawl still spends a request on the escaped spelling and stores its capture under the corrected address, and `requested_url` records honestly what went out on the wire.

## Rules that were considered and rejected

- **Collapsing `http` into `https`.** They are different addresses, and which one a site serves is exactly the kind of fact an archive exists to record. When a site redirects one to the other, the redirect says so, and the capture records both ends of it.
- **Removing a trailing slash below the root.** `/a` and `/a/` are routinely different resources, and which one a server considers canonical is the server's business. The root is the exception the parser already handles, since an empty path and `/` are the same request on the wire.
- **Stripping other host prefixes such as `m` or `amp`.** Unlike `www`, these commonly serve genuinely different documents. Collapsing them would lose an archive of what was actually served.
- **Removing `index.html` or a default document name.** Which name a server maps to a directory is configuration, not a property of URLs.
- **Rewriting percent-encoding or the case of the path.** Path case is significant to most servers, and re-encoding risks changing what is requested for no dedupe worth having. The parser's own encoding of bytes that are illegal where they appear is a different thing: it is what makes the string a URL at all, and it happens before any rule here runs.

## Known limits

- **`www` is stripped without consulting a public suffix list.** `www.co.uk` reduces to `co.uk`, which is a registry name and not a site. No rule about the shape of the string can avoid this, since `co.uk` and `example.com` are the same shape and only a list knows which of them is registrable. The list is the problem: public suffixes are added and removed continuously, so canonicalization built on one produces a different address for the same URL depending on when it ran, and that address is the item's identity, its directory name and the id stored inside its record. An archive whose identities move under it between two versions of the binary corrupts itself quietly. If this is ever worth fixing, the way to do it is to vendor the list into the repository, treat it as part of the archive format and treat updating it as a format version change with a migration, rather than to depend on a list that changes on its own.
- **Undoing the escaped separator moved the address of items already stored under it.** A rule change is a migration that rewrites the tree, and there is none: an item filed under an address holding `amp;` in a parameter name no longer hashes to the directory it sits in, so the walk reports it as misfiled and it drops out of listings and exports while its files stay on disk. Only an address carrying the escape moves, which is only an address that was the defect, so nothing correctly filed is touched. The item that address duplicated is unaffected and still holds the page. A page reachable under no other spelling is the loss, and capturing it again files it correctly.
- **An address literal and a domain can share a grouping directory.** `http://[::1]/` and a domain named `--1` both group under `items/--1/`, since the colons of an IPv6 literal become dashes. Nothing merges, because the item id is derived from the full canonical URL and those differ, but the directory a human browses is ambiguous.
- **Nothing here is a guard against fetching a private address.** `http://127.0.0.1/`, `http://[::1]/` and `http://169.254.169.254/` are all valid addresses to canonicalize, and refusing them is a decision about what may be fetched, not about what a URL means. That guard belongs on the fetch path, which does not exist yet.

## What is left for the fetch path

The strongest form of canonicalization is not a rule at all: it is following the redirect and keying on the canonicalized final URL. That collapses `www`, `http` to `https`, trailing slashes and site specific spellings exactly as the site itself defines them, without a single guess.

It requires having fetched, which is why the static rules above still exist: the crawl frontier has to deduplicate links before spending a request on them, and it is that pre-fetch dedupe that keeps a run from archiving one site twice. Keying an item on the canonicalized final URL belongs with the fetch path, and this document will gain the rule when there is a fetch to attach it to.
