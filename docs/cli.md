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
| `--delay <SPAN>` | the wait between requests, which slows the crawl rather than bounding it |
| `--deadline <SPAN>` | the wall clock the whole run gets, or `none` |
| `--request-timeout <SPAN>` | how long one request may take before it counts as no response |
| `--max-retries <N>` | how many times a request worth repeating is repeated |
| `--max-response-bytes <BYTES>` | the ceiling on one response body |
| `--allow-private-addresses` | let the run reach addresses that exist only inside a network |
| `--from-sitemap [<URL>]` | additionally archive what the site's sitemap lists |

A span carries its unit: `250ms`, `30s`, `5m`, `1h`. A bare number would have to mean seconds on the deadline and milliseconds on the delay, and a run whose budget was read in the wrong unit is either over before it starts or never over at all.

A zero is refused everywhere it would not mean zero, which is everywhere except `--delay`. On `--max-pages`, `--max-depth` and `--concurrency` the engine underneath reads it as no limit at all, so a run asking for the smallest possible crawl would get an unbounded one. On `--deadline` and `--request-timeout` it is the opposite mistake: a budget nothing can finish inside, so every URL is reported as a server that answered nothing, which is what a site being down looks like and leaves with the code that says the web misbehaved. `--delay` keeps zero because a wait of none is a wait, and it is the library's own default.

A crawl that really should take everything says so with a large number. The one unbounded thing that can be asked for by name is the wall clock, with `--deadline none`.

The archive is created when the path holds no archive yet, since `capture` is the only verb that writes and a first collection has to start somewhere. It says so on the line above the report, because a path typed wrong is otherwise a new empty archive nobody was told about. The seed is screened before that happens: a run the engine will not dial leaves no directory behind on the path it was pointed at.

The per-host extraction rules that sit in the archive are read at the start of the run. A rule file that cannot be used is a warning and not a refusal: it costs the extractions it would have improved, and the response is the part that cannot be fetched again. [`readability.md`](readability.md) has the file and its directives.

### Addresses inside a network

A seed naming loopback, a private range, a link-local address or one of the names a cloud metadata service answers on is refused before anything is dialled, and a page that ends on one of those addresses after a redirect is refused before it is stored. `--allow-private-addresses` turns both off for the run, which is how a locally served site is archived at all.

That flag is the one that decides whether the archive can be talked into reading the machine it runs on. It exists because pointing the crawler at a server on localhost is also the only way the fetch path is exercised at all.

### The response byte ceiling

`--max-response-bytes` is settled for the whole process before anything is fetched, because the engine reads the ceiling from an environment variable on its first fetch and keeps that value for the rest of its life. One run is one process here, so per-run and process-wide are the same thing, and the flag does not promise anything the engine cannot hold.

It refuses anything under a mebibyte, which is the smallest ceiling the engine honours: below that it raises the number to that floor and says nothing, so a smaller value accepted here would be a ceiling the tool reports and no run applies.

The flag wins over `SPIDER_MAX_SIZE_BYTES` already in the environment, which is the escape hatch below it: a variable set there stands when the flag is absent, including a zero meaning no ceiling at all, which the flag itself will not accept.

### The sitemap

`--from-sitemap` archives what a site's sitemap lists, additionally to the ordinary crawl from the seed, for a site whose pages do not link to each other: an index rendering a handful of posts and loading the rest through an API a crawl has no reason to call is the case this answers.

With no address given, the sitemap read is the one named by a `Sitemap:` directive in the host's `robots.txt`, read case insensitively since real files spell it in every case; the first directive found is the one read. With no directive found, `/sitemap.xml` is tried, which is where a browser would look next. Both are fetched through the same guards a page gets: the private address refusal, the response byte ceiling, the redirect screening.

A URL the sitemap lists enters the run at depth zero, exactly like the seed, and by default nothing is followed out of it: a depth bound has no meaning for a page nobody linked to. Giving `--max-depth` explicitly changes that, since it is then a decision made on purpose rather than left at its default: the same depth that already bounds the ordinary crawl also bounds how far a listed URL is traversed from, taking each one as a seed of its own and sharing what is left of the run's own page count and deadline rather than starting over with a fresh budget.

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

`capture` and `export` answer with one object each, because each reports on a run rather than listing a collection. The capture object carries the counts the human report shows plus every URL the run did not archive, grouped by why: `failed_fetches`, `unaddressable_pages`, `pages_inside_a_network`, `unreadable_pages`, `unreadable_articles` and `links_never_followed`. With `--from-sitemap`, it also carries a `sitemap` object: the address read, how many URLs it listed, how many were taken and how many a bound refused, which is where a sitemap listing 247 posts against a run that archived 200 of them is made visible. The export object carries the number of notes written and the paths it could not read.

Both objects are declared by the command line rather than serialized off the library's own report. A field added to a record inside the crate is then not accidentally a promise to everything already parsing this output.

## Deliberately not here

- **A way to ignore `robots.txt`.** The crawl respects it, and a flag to stop doing that changes what the tool is rather than how it is run.
- **A configuration file.** Every number the execution policy has is a flag with a default that is already the considered value. A file would be a second place for those numbers to disagree.
- **A verb that creates an empty archive.** `capture` creates one when it needs one, and a collection with nothing in it is not a thing anyone has asked to make.
- **Capturing more than one seed per run.** A seed is one host and one budget. Two seeds are two runs, which a shell already knows how to write.
- **Expanding a sitemap index into the sitemaps it lists.** Both sites this feature was measured against publish one plain sitemap, and a bound on how deep an index may point at further indexes is a number with nothing real yet to calibrate it against. A sitemap index is refused with a message saying so rather than read as an empty sitemap.
