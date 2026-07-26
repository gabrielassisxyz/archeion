# The crawl boundary

The crawl engine was chosen by a benchmark. Two candidates ran the same seeds under the same nominal limits, one covered roughly twice the pages and surfaced non-200 responses, and it won on that evidence. Evidence expires: a benchmark is a measurement of one afternoon, engines change, and the next site that has to be archived may be the one this engine handles worst.

So the engine is a dependency behind a line, not a foundation the archive is built on. This document is what the line is made of, and what a second engine would have to provide to replace the first.

## What crosses the line

Three types, all of them written in the archive's terms rather than an engine's.

- **`Seed`**, going down: where a crawl starts and the limits it stays inside, which are a page count, a depth, a concurrency, a delay, and the execution policy below. The page count, depth, concurrency and delay are the settings the benchmark ran under, so a run started with them produces a known result.
- **`PageEvent`**, coming up: either a response or the report that there was none. A response carries the requested URL, the final URL, the status, the response headers with every repeat a name sent, the body, and when the fetch happened.
- **`CrawlOutcome`**, at the end: what the crawl lost, and why it ended. Today the loss is the count of pages the engine fetched and the archive never received, and the ending is one of three: nothing left to fetch, the budget gone, the caller having asked to stop.

Everything else an engine knows about, its configuration surface, its concurrency model, its idea of what a page is, stops at the adapter.

## The decisions in it

**The call blocks and the core stays synchronous.** The engine is asynchronous underneath, and its runtime is built inside the adapter and never escapes. An archive is a directory being written; making canonicalization, storage and every test above them async so that one dependency feels at home is the dependency dictating the shape of the program.

**Pages arrive one at a time, through a callback, while the crawl runs.** A crawl that returns a list has already held a run's worth of response bodies in memory, and has produced nothing if it is interrupted at page 199.

**The callback answers with a `ControlFlow`.** The caller is the one who knows when continuing is pointless. One case is a failed write: the disk that refused this capture will refuse the next one, and a crawl that keeps fetching after that spends a site's bandwidth on nothing. Breaking cancels the crawl rather than draining it. The other case is the seed's deadline, which is described below.

**A non-200 is a response like any other.** An archive that keeps only successes cannot answer why something is missing from it, and a 404 recorded on a date is the evidence that the page was already gone then. The benchmark's losing candidate reported only successes, and that counted against it.

**A fetch that reached no server is not a response, and the boundary says so.** This is not a detail of one engine, it is the reason `PageEvent` has two shapes. A DNS failure, a refused connection and a TLS error produce no status, no headers and no body, and an engine with nowhere to report that invents them: this one answers 599 for a failure to send and 524 for a connection timeout. Archiving that would put a status in the record that no server ever sent, and a later reader has no way to tell it from a proxy that really answered 599. The URL and the reason are reported instead, and nothing is stored.

**The identity of a capture comes from the final URL, not the requested one.** After a redirect the content is at the destination, and filing it under the address that pointed there gives one page a second identity for every link that reaches it. Both URLs are kept in the record, so nothing about the fetch is lost. [`docs/canonicalization.md`](canonicalization.md) has the rules that turn that URL into an address.

**A page the canonical rules refuse is skipped and reported, not fatal.** One URL the archive has no address for says nothing about the other two hundred. The run carries the URL and the reason, so the refusal can be looked at rather than counted.

**A lost page is counted, never inferred.** The queue between the engine and the archive is bounded: sized to the fetch concurrency alone it would drop pages as soon as a write is slower than a fetch, and sized to the page limit it would hold a whole crawl's bodies in memory. It is sized between the two, and what overflows anyway is counted into the outcome. A caller that breaks out early counts what it leaves queued the same way, since those pages cost a fetch each and the archive does not have them; that count is a floor, because a fetch still in flight can queue another page after the queue was measured. Silently archiving less than was fetched is the failure this project can least afford.

**The execution policy is three numbers on the `Seed`: a deadline, a request timeout, and a retry budget.** The benchmark's clearest finding was that a per-page limit is not an execution policy, since one of its domains spent 402 seconds of a 573 second run. What bounds that is wall-clock time, and the number is an archival decision: it says how much of an afternoon one site is worth, which no engine can know.

**A seed is one host, so the per-seed budget is the per-domain budget.** Subdomains and other TLDs of the same name are separate seeds by construction, and a crawl started at one address never leaves it. There is no second, narrower per-domain knob because there is no second domain for it to apply to. What was genuinely missing underneath the deadline is a ceiling on a single request, which is a different thing entirely: at the default concurrency, one connection a server accepts and then holds open owns a sixteenth of the run until something closes it. The engine's own default for that was 120 seconds, which is a third of the default deadline spent by one dead socket.

**The request timeout reaches the HTTP client, and a request that outlives it is reported as the failure it was.** The engine cancels it and surfaces a transport error, which crosses this line as a `NoResponse` like a DNS failure does. That is worth saying because the engine has a second mechanism for the same symptom, its first-byte watchdog, which answers a stalled request with a 504 it invented, carrying no body and no error mark. That one is off, and a test asserts it stays off: the boundary would read its output as a response a server sent.

**Retry belongs to the engine, its budget belongs here.** Retrying is fetching again, and the engine is the only thing that fetches, so what it repeats and how long it waits are its rules: a 429, a 408 or a server error, never a DNS failure or a redirect loop, and between attempts the longer of an exponential backoff and what the response asked for, which for a 429 is its `Retry-After`. How much of the seed's budget to spend on repeating rather than on new pages is the archive's call, and that is the number on the `Seed`.

**The deadline is enforced in two places, and they reach different failures.** The adapter cancels the crawl when the budget is gone, and the pipeline above the boundary stops on the next page it is handed. This is not the same guard written twice. A host that accepts the connection and then says nothing produces no page at all, so nothing above the boundary ever gets a turn to notice, and only the adapter can end it. An engine that ignores the field is ended from above instead, on whatever it does produce. The page in hand when the budget runs out is archived and the run ends after it, because that page was already fetched and refusing to write it spends the bytes without keeping anything.

**The deadline bounds fetching, not the writing of what was already fetched.** When it fires, the pages sitting in the queue are handed over and then the run ends. They are in memory, they cost their bytes before the budget ran out, and the work left is local. What they are not is waited for: a cancelled crawl leaves senders in tasks that are still winding down, so the queue is read for what is in it rather than read until it closes, which would hand the end of the run back to the thing the deadline just took it from.

**The engine's own crawl timeout was rejected for the deadline.** It stops the engine from queueing new links once the budget is gone, but it also applies that same budget again as a ceiling on each individual fetch, so a run can finish at roughly twice its deadline. A deadline that is approximately a deadline is not one. The adapter races the crawl against a timer it owns instead, which ends the run at the number that was asked for.

**The engine is configured against the archive, not against its own defaults.** Two of its features had to be turned off rather than inherited: one spools a large response body to a temporary file and then reports an empty body, which would store a page as though it had arrived, and one attaches a browser fingerprint to every request, which is the opposite of a crawler that says who it is. Both are silent, and both would have been shipped by taking a bundle. `Cargo.toml` carries the list and the reason.

**The seed's scheme is checked before the engine dials anything.** `file:` or `data:` reaching a crawler is the archive reading the local machine. The engine already refuses to follow a redirect into an internal address, under either of its redirect policies, so that half is covered. What is not checked is a seed that names a private address outright, and that is where hardening starts.

## What is deliberately not here

- **A cap on how many bytes one response may spend.** Nothing today stops an endless response from filling memory until the run dies with it. The engine can enforce a limit, but the number is an archival decision, and a cap that truncates would store a partial body under a status that promises a whole one.
- **A seed that names a private address, and a redirect that leaves the seed's host.** The first is unchecked. The second is allowed, so a page can be archived under a host the run was never pointed at, since identity comes from where the content ended up.
- **Assets.** A page event carries the page. The subresources it needs are their own pass.

## Testing it

The pipeline above the line is tested against a scripted engine that replays written-down page events, so what the archive does with a 404, with a redirect or with a URL it cannot address is decided by the test rather than by whatever the web answered today.

In the adapter, everything that does not need a socket is tested the same way: the queue that can lose pages is driven directly with a channel and more pages than it holds, and the translation from the engine's page to a page event is checked on a page built by hand. The deadline is the same trick applied to time. The crawl reaches the place that enforces it as a future rather than as a website, so the stalled host that the whole policy exists for is a future that never finishes, and the run that has to end at its budget can be proven to in fifty milliseconds. The numbers that only the engine can act on, the request timeout and the retry budget, are checked where they land in its configuration, since a change to how the engine is configured otherwise compiles and passes every gate while being broken. What is left uncovered is the network path itself, because a test that reaches the web is a crawl and not a test. `cargo run --example capture_seed` runs the real engine into a real archive; point it at a server running on localhost and that path is exercised without leaving the machine.
