# The crawl boundary

The crawl engine was chosen by a benchmark. Two candidates ran the same seeds under the same nominal limits, one covered roughly twice the pages and surfaced non-200 responses, and it won on that evidence. Evidence expires: a benchmark is a measurement of one afternoon, engines change, and the next site that has to be archived may be the one this engine handles worst.

So the engine is a dependency behind a line, not a foundation the archive is built on. This document is what the line is made of, and what a second engine would have to provide to replace the first.

## What crosses the line

Three types, all of them written in the archive's terms rather than an engine's.

- **`Seed`**, going down: where a crawl starts and the limits it stays inside, which are a page count, a depth, a concurrency and a delay. Its defaults are the settings the benchmark ran under, so a run started with them produces a known result.
- **`PageEvent`**, coming up: the requested URL, the final URL, the status, the response headers in the order and multiplicity they arrived, the body, and when the fetch happened.
- **`CrawlOutcome`**, at the end: what the crawl lost. Today that is the count of pages the engine fetched and the archive never received.

Everything else an engine knows about, its configuration surface, its concurrency model, its idea of what a page is, stops at the adapter.

## The decisions in it

**The call blocks and the core stays synchronous.** The engine is asynchronous underneath, and its runtime is built inside the adapter and never escapes. An archive is a directory being written; making canonicalization, storage and every test above them async so that one dependency feels at home is the dependency dictating the shape of the program.

**Pages arrive one at a time, through a callback, while the crawl runs.** A crawl that returns a list has already held a run's worth of response bodies in memory, and has produced nothing if it is interrupted at page 199.

**The callback answers with a `ControlFlow`.** The caller is the one who knows when continuing is pointless. The case that exists today is a failed write: the disk that refused this capture will refuse the next one, and a crawl that keeps fetching after that spends a site's bandwidth on nothing. Breaking cancels the crawl rather than draining it. This is also where a per-seed deadline will cut, when there is one.

**A non-200 is a page event like any other.** An archive that keeps only successes cannot answer why something is missing from it, and a 404 recorded on a date is the evidence that the page was already gone then. The benchmark's losing candidate reported only successes, and that counted against it.

**The identity of a capture comes from the final URL, not the requested one.** After a redirect the content is at the destination, and filing it under the address that pointed there gives one page a second identity for every link that reaches it. Both URLs are kept in the record, so nothing about the fetch is lost. [`docs/canonicalization.md`](canonicalization.md) has the rules that turn that URL into an address.

**A page the canonical rules refuse is skipped and reported, not fatal.** One URL the archive has no address for says nothing about the other two hundred. The run carries the URL and the reason, so the refusal can be looked at rather than counted.

**A lost page is counted, never inferred.** The queue between the engine and the archive is bounded: sized to the fetch concurrency alone it would drop pages as soon as a write is slower than a fetch, and sized to the page limit it would hold a whole crawl's bodies in memory. It is sized between the two, and what overflows anyway is counted into the outcome. Silently archiving less than was fetched is the failure this project can least afford.

**The seed's scheme is checked before the engine dials anything.** `file:` or `data:` reaching a crawler is the archive reading the local machine. This is the cheap half of the guard: redirects into private ranges happen inside the engine, after that check, and hardening them is separate work.

## What is deliberately not here

- **Deadlines, retry and backoff.** The benchmark's clearest finding was that a per-page limit is not an execution policy: single domains dominated the run, one taking 402 seconds of 573. That policy belongs to the archive, above this line, not to an engine's settings.
- **SSRF beyond the seed.** See above.
- **Assets.** A page event carries the page. The subresources it needs are their own pass.

## Testing it

The pipeline above the line is tested against a scripted engine that replays written-down page events, so what the archive does with a 404, with a redirect or with a URL it cannot address is decided by the test rather than by whatever the web answered today.

The adapter below the line is the part no test covers, because a test that reaches the web is a crawl and not a test. `cargo run --example capture_seed` runs the real engine into a real archive; point it at a server running on localhost and the whole path is exercised without leaving the machine.
