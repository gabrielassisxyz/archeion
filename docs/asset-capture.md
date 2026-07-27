# Asset capture

A capture of a page that keeps only the page is a capture that stops looking like itself. The stylesheet moves, the images go behind a domain that lapses, and what is left in the archive is markup pointing at addresses that answer nothing. So the subresources a page needs are fetched and stored beside it.

That means following addresses chosen by a remote document, which is the thing an archiver gets aimed at. Every number and every guard below exists because of that, and this document is the reasoning behind them.

## When it runs, and on what

The pass runs once per capture, between reading the page and writing the record.

Its input is the subresource list that metadata extraction already produced: absolute, deduplicated by address, http and https only, and capped where extraction caps it. Nothing here re-parses markup. [`metadata-extraction.md`](metadata-extraction.md) has how that list is built, including the rules about what counts as a subresource and what deliberately does not.

A capture with no reading beside it references nothing and the pass does nothing: a response that is not a page has no subresources, and neither does a page whose markup the extractor gave up on, since the references live in the part it failed on.

It runs before the capture record is written, because the record names what it holds and a capture id is derived from that. The cost is that a process dying mid-pass loses the page whole rather than leaving a record on disk that claims subresources nobody ever fetched. That is the trade taken on purpose.

## What is fetched

Every kind extraction reports: images, stylesheets, scripts, media and icons.

The kind does not decide. Size does, which is what keeps a film out of a capture by default without a rule about films, and what lets an unusual page keep an unusual subresource as long as it is small. A page that embeds a video is recorded as having referenced it, and the record says the file was over the ceiling and by how much.

## The bounds

Five of them, all per capture except the last, which is the run's.

The one that is easiest to miss is the pass being inside the callback the crawl hands pages through. While it waits, the engine keeps fetching pages that nothing is reading: they fill the queue between the two, and what overflows was paid for and thrown away, counted into the run as pages dropped. So a bound here is not only about bytes and requests, it is about how long one page may hold the pipeline.

- **A hundred and twenty-eight references dealt with.** It counts references dealt with and not files kept, and that difference is the whole bound. A page listing two thousand addresses that answer nothing stores nothing, so a ceiling on what is stored would never be reached, and the run would spend two thousand requests on somebody else's behalf learning that. Whoever wrote the page picks those addresses.
- **Eight megabytes for one subresource.** Above every stylesheet, script, font and photograph, and below a video.
- **Thirty-two megabytes transferred for one capture's subresources.** It counts what came over the wire and not what was kept, drawn on the same distinction as the count above. A body is paid for in full before its size is known, so a ceiling on stored bytes would sit at zero for a page whose files are all just over the per-file limit, while the run transferred a hundred and twenty-eight of them per page and stored none. What is served out of what the run already learned costs nothing and counts as nothing. This is where the pass stops asking rather than a total it guarantees: the size of the next subresource is unknown until it arrives, so the last one can cross the line, and the overshoot is bounded by the response ceiling below.
- **Three requests in a row that produce no response at all.** Then the capture stops asking. Silence in a row is a fact about what is answering rather than about each file, and it is the expensive kind of failure: a host that accepts connections and then says nothing costs a full request timeout every time, which at the default numbers is a page holding the pipeline for the rest of the run one file at a time. Three survives a page referencing an unreachable host a couple of times and bounds the wait at three timeouts. What it costs is a page whose first three references are on a dead host losing the rest, which is recorded rather than silent, and recoverable from the reference list without a crawl. A refusal that never became a request is not silence, since it cost no wait, and an answer of any kind clears the count, a 404 included.
- **The run's wall-clock deadline.** Once it is gone nothing more is asked for. The budget is read plainly here, without the margin the backstop over the crawl engine allows itself: that margin exists so an engine handing over pages it already fetched is not cut off mid-handover, and a subresource nobody has asked for yet is the opposite case. It is also the cheapest thing in a run to give up on, since the page is archived either way.

The response ceiling that already covers a page covers a subresource too, and it is the guard against a body that expands rather than arrives: the engine counts what it decompressed, not what came over the wire, so a small response that unpacks into gigabytes is cut at sixty-four megabytes and marked as short. [`crawl-boundary.md`](crawl-boundary.md) has that ceiling and why it reaches the engine through the environment.

## The guards on an address

A reference is a remote document choosing where the archive's next request goes, so it is screened exactly like a seed and by the same code.

- **Before it is asked for.** An address that exists only inside a network is refused by the pass itself: loopback, a private range, link-local, or a name a cloud metadata service answers on. The engine refuses those too, and asking it to would spend a request that never leaves the machine and come back as a reason to be read out of an error string.
- **Before it is dialled.** The engine screens it again, on the scheme as well as the address, because that is the guard a seed passes through and a subresource is not a safer kind of URL.
- **Where it ended up.** The same predicate is applied to the final URL before anything is written. The engine screens every redirect hop, and the archive checks anyway: the engine is the replaceable part, while bytes in the archive are the durable half of the harm.
- **Nowhere else.** No filename is ever derived from the address or from a header. A subresource body is stored under the hash of its bytes, like every other body.

## What arrives short is not stored

This is the one place a subresource is treated unlike a page.

A page that arrived short is stored and marked as short, because a page cut short is still the page and the mark keeps the record honest. A subresource exists so that its page still works, a stylesheet missing its end does not, and the subresource record has nowhere to say that the bytes are partial. Storing it would put bytes in the archive that read as a whole file. So it is refused, and the record says it arrived short and how much of it did.

## The record says what is missing

Every reference that did not become a subresource is in the capture record with its reason: no response, too large, arrived short, a ceiling reached, the capture having stopped asking, the deadline gone, or an address inside a network.

Without that the absence is readable and mute. A reader can compare the twenty references in the derived metadata against the twelve subresources in the capture and see that eight are gone, and has no way to tell an archive that refused them from a server that never sent them. The distinction is the one that matters: a ceiling reached says raising a number would have kept the page whole, and a response that never came says no number would have. The run reports the same thing while it runs, and a run is over by the time anyone asks why a page looks wrong.

A capture that got everything carries no such list, which is also the shape of every record written before this existed.

## One file, one request

The answer to a subresource is remembered for the whole run, including the answer that there is none.

One stylesheet belongs to every page of a site that links it. A run that asked the server for it once per page would spend two hundred requests on one file, which is not something an archive gets to do to somebody else's host, and a URL that answered nothing answers nothing for every page that references it. What is remembered is the answer and not the bytes, so a second page referencing a file gets the record that is already there, without a request and without a copy. The content-addressed store would have kept the bytes single either way; only the pass can keep the request single.

Ceilings and deadlines are never remembered. A ceiling one capture reached says nothing about the next capture, and a run out of time is not a fact about a stylesheet.

## Deliberately not here

- **Concurrency.** The pass fetches one subresource at a time. It is what stands in for the politeness delay, which belongs to the crawl loop and does not reach a single fetch, and it bounds what one page's subresources add to the load the crawl is already putting on a host. The cost is real and worth stating exactly: because the pass blocks the callback pages arrive through, a run with subresources archives fewer pages inside the same deadline, and a run that meets slow subresources can have pages fetched and then dropped for want of anyone reading them. The bound above is what keeps the pathological case from eating a run; the ordinary case, tens of files answering in tens of milliseconds, fits inside the queue. Fetching a page's subresources in parallel would cut the block and multiply what the archive asks of a host at once, so it is a measurement to make against a real site rather than a guess to ship.
- **Retry.** Retrying is fetching again, which is the crawl loop's, so a subresource that failed is reported rather than asked twice. A later pass over the archive is the right place to try again, since the page is already stored and nothing has to be crawled to find the reference.
- **A robots file for a subresource.** Robots governs crawling: discovering addresses and traversing them. A browser rendering a page it has already been given fetches that page's images without consulting anything, and a capture of the requisites of a page the crawl was allowed to fetch is the same act. The crawl itself respects robots, which is where the traversal happens.
- **A connection kept open across a page's subresources.** Each fetch builds its client around the subresource's own host, so a redirect of it is judged against the host it is on rather than against the page that named it. What that costs is a handshake per subresource. Caching a client per host is the obvious fix and is not built.
- **Subresources named inside a subresource.** A stylesheet that imports another stylesheet, or names a font in a `url()`, is stored as the bytes it is, and nothing reads it to find what it refers to. Only the page's own markup is read. Same for what a script fetches when it runs, which nothing here executes.
- **Rewriting the page.** The stored markup is exactly what arrived, pointing at the addresses it always pointed at. Turning an archived page plus its subresources back into something a browser renders offline is a replay concern, and replay is not built.
- **A pass over an existing archive.** Subresources are acquired at capture time only. A capture already on disk with references nobody fetched stays that way. The list is in the derived record, so the pass that fixes it needs no crawl, and it does not exist yet.

## Testing it

The pass is driven against the same scripted engine the rest of the capture path uses: written-down answers for written-down URLs, no network, and a count of the requests it made. That count is what proves the shared file was asked for once, and it is what the ceilings are read through, since both of them are about requests that were never made: the page whose thousand references all answer nothing, and the page whose every file is just over the size one may spend.

Each bound has a test that fails when it is written the plausible wrong way: counting files kept rather than references dealt with, or stored bytes rather than transferred ones, or never giving up on a host that has stopped answering. All three wrong versions pass every other test in this project while bounding nothing. The page of a thousand dead addresses is where all of them meet, so that test asserts what each one refused rather than only the total.

One test opens a socket, and it is not a formality. A subresource is fetched from inside the page callback of a crawl that is already running, which is a thread already driving the engine's runtime, and a runtime cannot be entered from there. That rule lives inside a dependency, nothing about it can be asserted without running one, and getting it wrong is a panic on the first subresource of the first page that no other test in this project would see. So a server on loopback serves a page, a stylesheet and an image whose bytes are not text, and the test asserts the archive holds them and that the only paths asked of that server were the four it should have been.
