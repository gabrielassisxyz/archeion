# The command line

Four verbs, one rule about where the archive goes, and one rule about what an exit code means.

```sh
archeion capture <archive> <seed-url> [options]
archeion repass  <archive> [--allow-private-addresses]
archeion list    <archive>
archeion export  <archive> <destination> [--all-captures]
```

The archive is always the first argument. `export` then reads as source and destination, and `capture` reads as the collection first and what to put in it second, which is the order that stays learnable once there are more verbs than these. `--json` is the tool's flag rather than one verb's, so it is accepted before or after the verb.

## Capture

`capture` crawls one seed into an archive. A seed is one host: subdomains and other TLDs of the same name are separate seeds, and a crawl never leaves the host it was pointed at, so the budget one seed gets is also the budget that host gets.

Every option is one field of the seed the library crawls with, spelled the same, and the default the help text prints is read off that seed rather than repeated here. What each number is for, and why it has the value it has, is in [`crawl-boundary.md`](crawl-boundary.md).

| option | what it bounds |
|---|---|
| `--max-pages <N>` | how many pages the run may archive |
| `--max-depth <N>` | how far from the seed links are followed |
| `--concurrency <N>` | requests in flight against the host at once |
| `--delay <SPAN>` | the wait between page requests, which slows a run rather than bounding it |
| `--deadline <SPAN>` | the wall clock the whole run gets, or `none` |
| `--request-timeout <SPAN>` | how long one request may take before it counts as no response |
| `--max-retries <N>` | how many times a request worth repeating is repeated |
| `--max-response-bytes <BYTES>` | the ceiling on one response body |
| `--allow-private-addresses` | let the run reach addresses that exist only inside a network |
| `--cookie-file <PATH>` | the `Cookie` header of an authenticated request, sent to the seed's own origin |
| `--from-sitemap [<URL>]` | additionally archive what the site's sitemap lists |

A span carries its unit: `250ms`, `30s`, `5m`, `1h`. A bare number would have to mean seconds on the deadline and milliseconds on the delay, and a run whose budget was read in the wrong unit is either over before it starts or never over at all.

A zero is refused everywhere it would not mean zero, which is everywhere except `--delay`. On `--max-pages`, `--max-depth` and `--concurrency` the engine underneath reads it as no limit at all, so a run asking for the smallest possible crawl would get an unbounded one. On `--deadline` and `--request-timeout` it is the opposite mistake: a budget nothing can finish inside, so every URL is reported as a server that answered nothing, which is what a site being down looks like and leaves with the code that says the web misbehaved. `--delay` keeps zero because a wait of none is a wait, and it is the library's own default.

A crawl that really should take everything says so with a large number. The one unbounded thing that can be asked for by name is the wall clock, with `--deadline none`.

The archive is created when the path holds no archive yet, since `capture` is the only verb that writes and a first collection has to start somewhere. It says so on the line above the report, because a path typed wrong is otherwise a new empty archive nobody was told about. The seed is screened before that happens: a run the engine will not dial leaves no directory behind on the path it was pointed at.

The per-host extraction rules that sit in the archive are read at the start of the run. A rule file that cannot be used is a warning and not a refusal: it costs the extractions it would have improved, and the response is the part that cannot be fetched again. [`readability.md`](readability.md) has the file and its directives.

### Addresses inside a network

A seed naming loopback, a private range, a link-local address or one of the names a cloud metadata service answers on is refused before anything is dialled, and a page that ends on one of those addresses after a redirect is refused before it is stored. `--allow-private-addresses` turns both off for the run, which is how a locally served site is archived at all.

That flag is the one that decides whether the archive can be talked into reading the machine it runs on. It exists because pointing the crawler at a server on localhost is also the only way the fetch path is exercised at all.

### A subscription the run carries

A publication whose posts are paid for is otherwise archived as its teasers: measured on one publication of 241 posts, 107 of them, 44 percent, were stored as a few hundred words ending in an appeal to subscribe. `--cookie-file` is what closes that. It names a file holding the whole `Cookie` header of a request made by a browser that is signed in, which is the same thing the reader's own session is, and the run sends it with the pages it asks for.

**The credential is never an argument.** There is no flag that takes the value, and there will not be: an argument lands in shell history and in the process table, where every other process of the same user can read it for as long as the run lasts. A file and an environment variable are both readable by this user's other processes too; what they are not is written down by a shell and shown to everybody on the machine.

- **The file must be readable by its owner alone.** One readable by its group or by anybody else is refused before anything is fetched, and the refusal says which of the two it is because that is how far the exposure already went: a file its group can read was available to the accounts in that group, and one anybody can read was available to every account on the machine. `chmod 600` is the answer either way, and a credential that was exposed is worth replacing rather than only protecting.
- **`ARCHEION_COOKIE_HEADER` is the alternative**, holding the header value rather than a path. Neither source is required, and a run given neither archives what an anonymous reader is served. With both present the file wins, since a path typed for this run is a decision made now and a variable may have been exported hours ago.
- **The flag knows nothing about any publisher.** The variable names this tool and the header, and the origin the credential belongs to comes from the seed, so one variable serves every site.
- **A value carrying a character no header can hold is refused**, a newline being the one that matters: inside a header value it is a second header nobody wrote.

**The cookie is bound to the seed's own origin and is sent nowhere else.** Scheme, host and port together: a run pointed at `https://example.com/archive` sends it to that site and to nothing that site names. A picture on a content network and a redirect that leaves the host are each a request to somewhere else, and handing a session to whatever address a page points at is handing a credential to a third party. Inside a redirect chain the strip is the HTTP client's own, which compares the next hop's host, port and scheme against the previous one's and drops `Cookie` when any of the three differs.

**The run says what its session reached.** A credential can apply to nothing at all while the run still exits zero: a seed spelled with a trailing dot, an `http` seed whose host redirects to `https`, a credential bound to an origin the run never asks for. Each of those archives the paid half of a publication as teasers and looks exactly like a run nobody gave a session to, so the report carries a `session` row naming the origin the credential was bound to and how many captures it reached, and `--json` carries the same two numbers. A count of zero against a run of hundreds is the whole reason the row exists, and the origin beside it says whether the seed was the address that was meant.

**What the archive stores of it is nothing.** The value of a `set-cookie` header is dropped from every capture, with a session and without one, and the header stays in the record saying it was sent and how many times. That is a repair rather than a precaution: 247 of the 250 captures of one publication already held 930 of these values between them, being the tracking identifiers a site issues to an anonymous reader. [`storage-model.md`](storage-model.md) has what the record says instead, and the capture also says that the run carried a session, so two captures of one page, one anonymous and one paid for, can be told apart by a reader who has both.

### The response byte ceiling

`--max-response-bytes` is settled for the whole process before anything is fetched, because the engine reads the ceiling from an environment variable on its first fetch and keeps that value for the rest of its life. One run is one process here, so per-run and process-wide are the same thing, and the flag does not promise anything the engine cannot hold.

It refuses anything under a mebibyte, which is the smallest ceiling the engine honours: below that it raises the number to that floor and says nothing, so a smaller value accepted here would be a ceiling the tool reports and no run applies.

The flag wins over `SPIDER_MAX_SIZE_BYTES` already in the environment, which is the escape hatch below it: a variable set there stands when the flag is absent, including a zero meaning no ceiling at all, which the flag itself will not accept.

### The sitemap

`--from-sitemap` archives what a site's sitemap lists, additionally to the ordinary crawl from the seed, for a site whose pages do not link to each other: an index rendering a handful of posts and loading the rest through an API a crawl has no reason to call is the case this answers.

With no address given, the sitemap read is the one named by a `Sitemap:` directive in the host's `robots.txt`, read case insensitively since real files spell it in every case; the first directive found is the one read. With no directive found, `/sitemap.xml` is tried, which is where a browser would look next. Both are fetched through the same guards a page gets: the private address refusal, the response byte ceiling, the redirect screening.

A URL the sitemap lists enters the run at depth zero, exactly like the seed, and by default nothing is followed out of it: a depth bound has no meaning for a page nobody linked to. Giving `--max-depth` explicitly changes that, since it is then a decision made on purpose rather than left at its default: the same depth that already bounds the ordinary crawl also bounds how far a listed URL is traversed from, taking each one as a seed of its own and sharing what is left of the run's own page count and deadline rather than starting over with a fresh budget.

**`--max-pages` and `--deadline` bound the run, not a phase of it.** Giving `--max-depth` beside `--from-sitemap` is what makes a run two phases, an ordinary crawl from the seed and then the listed URLs, and the ceiling and the clock carry across the boundary: what the crawl archived is already spent when the sitemap phase starts, and the deadline is measured from when the run began rather than from when the phase did. A run given four pages archives four in total, however they divide between the two.

**`--delay` is what paces this phase, and it is worth passing.** A sitemap phase asks a host for page after page with nothing between the requests: it exists precisely for a site whose pages do not link one another, so there is no traversal to slow it down and no natural gap. Measured against a real publication, a 250 page sitemap run with no delay asked at 2.2 pages a second and the host refused 160 of them with a 429. The wait is paid per request, so a URL the run already filed costs nothing, and it comes out of the same wall clock the deadline is measured on: a host asked slowly is a host fewer of whose pages fit in a given hour. The deadline is read again on the far side of the wait, so a run does not sleep past its own end and then ask for one more page.

It is paid whether a listed URL is fetched or, with `--max-depth` given explicitly, crawled. The engine applies the same delay inside a crawl, but only around the links that crawl discovers for itself, and a sitemap sub-crawl discovers none: a sitemap exists for a site whose pages do not link one another, so its seed is fetched with no wait of the engine's own. Leaving that branch to the engine would leave it exactly as unpaced as it was.

What the delay does not pace, on this path or on a crawl, is a page's own subresources. Those are fetched from inside the pass that acquires them, one at a time, and [`asset-capture.md`](asset-capture.md) has why that pass is serial and what it does and does not stand in for.

A URL the sitemap lists for a host other than the seed's is refused rather than fetched, since a sitemap is read for one host's sake and taking an address it names for another would let that site decide what this run fetches next. The sitemap itself is capped at fifty thousand listed URLs, the sitemap protocol's own ceiling for one file, counted while the file is still being read rather than after. A compressed sitemap and a sitemap index, one that lists further sitemaps rather than pages, are both refused with a message saying so rather than read.

None of this is fatal to the run. A sitemap that cannot be found, fetched or parsed is reported as a warning: the ordinary crawl's captures already happened, and a run that turned those into a failure over a sitemap that happens not to be one would be discarding a working archive over the wrong page.

## Reading a collection

`list` walks the archive and prints one line per item: the canonical URL, how many captures it has, when the most recent one was taken, and whether that capture produced an article.

`repass` walks the archive and refreshes derived records from responses already stored there. It re-runs metadata and readability extraction where a record is stale or absent, reads the archive's current `extraction-rules.json`, and can fetch only subresources a capture already recorded as missed by archive policy. It never fetches a page.

`export` writes the article captures as a Markdown vault. [`export.md`](export.md) has the front matter, the slug rules and what is deliberately not part of it.

## Repass

`repass` exists because the response body is authoritative and the derived layer is disposable. A better extractor, or a rule written for a host after the capture was taken, can therefore be applied to captures already on disk.

The pass opens an existing archive only. A missing path is an error rather than a new empty archive, because a repass with no captures to read is almost always a path typed wrong.

The command writes new derived records before removing conflicting old ones. An interrupted pass therefore leaves the same kind of mixed archive that a normal capture already can: some captures carry older readings, some carry newer ones, and all intact captures stay readable.

For article extraction, an existing article can become an article again, a refusal, or a not-article marker. A page refused by a cost guard does not erase an existing article, because lowering a ceiling silently would drop content the archive had already admitted.

A capture with no article beside it is re-read when it holds something the extractor now reads, which is how an archive filled before a media type was understood catches up. A response the site served as Markdown is the case that exists today: captures already on disk produce their articles on the next pass, from the stored bytes and without fetching anything.

For subresources, the pass only asks for URLs already listed in `assets_missed` where the archive's own policy stopped the original asset capture, such as a count ceiling, byte ceiling, deadline or a host that had stopped answering before that URL was tried. A URL that directly answered nothing is not retried blindly. Recovered assets, and retry results that are still missing, are written beside the capture and folded into `Archive::read_capture`; the original capture record is not rewritten because its id includes the assets present when it was filed.

`--allow-private-addresses` has the same meaning as it does on `capture`, but only for recovered subresources. It is off by default, so a stored page still cannot make a later pass read the local machine or network around it.

There is no `--cookie-file` here, and that is a decision rather than an omission. A credential is bound to the origin of the address that was typed, and a repass is given no address: it walks an archive that may hold captures of any number of hosts, so there is nothing for a binding to come from. A flag here would have to carry its own origin, which is a second surface for a need nobody has: a subresource on the host that issued the session is asked for with it during the capture, and a page behind a paywall is fetched again by capturing it again.

## Exit codes

Non-zero means the archive is not what it should be. It never means the web misbehaved.

| code | what happened |
|---|---|
| 0 | the command did what it was asked |
| 1 | the archive is missing or damaged, a seed was refused, a write failed, a run ended up holding less than it fetched, or the crawl discovered a link it never fetched at all |
| 2 | the command line could not be read |

The distinction is the point. A URL nobody answered, a page whose address the canonical rules refuse, a page that ended inside a network and markup the extractor could not read are all reported on stderr and none of them is a failure: a crawl of two hundred pages meeting three dead links did its job, and a pipeline that stopped there would stop on nearly every site. A refused seed, a failed write, an archive that is not one, pages the crawl fetched that never reached the archive, and a link the crawl found and never fetched at all are failures, because each of those is the collection being smaller or stranger than the run claimed.

`list` and `export` follow the same rule from the other side: an item the walk could not read is reported, the intact items are still printed, and the code is what keeps a script from reading a short answer as a complete one.

## Records instead of prose

`--json` replaces the human output with records. Warnings stay on stderr in both modes, since a reader consuming stdout is exactly the one that cannot see them.

`list` answers with one object per line rather than one array, so a collection of any size can be read without holding all of it and `grep` stays a legitimate way to ask a question of it.

`capture` and `export` answer with one object each, because each reports on a run rather than listing a collection. The capture object carries the counts the human report shows, `responses_refused` among them, a count by status of the captures a host answered with an error, plus every URL the run did not archive, grouped by why: `failed_fetches`, `unaddressable_pages`, `pages_inside_a_network`, `unreadable_pages`, `unreadable_articles` and `links_never_followed`. With `--from-sitemap`, it also carries a `sitemap` object: the address read, how many URLs it listed, how many were taken and how many a bound refused, which is where a sitemap listing 247 posts against a run that archived 200 of them is made visible. The export object carries the number of notes written and the paths it could not read.

Both objects are declared by the command line rather than serialized off the library's own report. A field added to a record inside the crate is then not accidentally a promise to everything already parsing this output.

## Deliberately not here

- **A way to ignore `robots.txt`.** The crawl respects it, and a flag to stop doing that changes what the tool is rather than how it is run.
- **A configuration file.** Every number the execution policy has is a flag with a default that is already the considered value. A file would be a second place for those numbers to disagree.
- **A verb that creates an empty archive.** `capture` creates one when it needs one, and a collection with nothing in it is not a thing anyone has asked to make.
- **Capturing more than one seed per run.** A seed is one host and one budget. Two seeds are two runs, which a shell already knows how to write.
- **Expanding a sitemap index into the sitemaps it lists.** Both sites this feature was measured against publish one plain sitemap, and a bound on how deep an index may point at further indexes is a number with nothing real yet to calibrate it against. A sitemap index is refused with a message saying so rather than read as an empty sitemap.
