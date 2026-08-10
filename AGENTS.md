# Archeion, agent briefing

> Read before every interaction. Living spec: short, imperative. On every gotcha or decision, append one line here.

> **What it is:** a local-first archival tool that captures web content and its metadata into a durable personal collection. **Calibration:** Tier 2 · Phase: work. External stakes are contained (single operator, no hosted service, no third-party data), personal stakes are high: this is a public developer tool meant to grow. Update the phase as the project moves work, then right, then fast; an agent reads this line to decide how much rigor a change deserves. **Review gate:** standard. One independent opinion over the whole branch diff, exactly once, pre-push, run the way the Review section below describes. No per-commit reviews.

## Stack and commands

- **Stack:** Rust, 2024 edition, toolchain 1.96 or newer. Single binary crate.
- **Build:** `cargo build`
- **Run:** `cargo run -- capture <archive> <url>` to fill a collection, `cargo run -- list <archive>` to read one. `--json` works before or after any verb. `docs/cli.md` is the surface and what each exit code means.
- **Test:** `cargo test`
- **Every gate at once:** `bin/ci`, which is exactly what CI runs. Green locally means green in CI.
- **Planned work:** `br ready` for what can be started now, `br` for the rest. The queue is maintainer state and is not committed; `ROADMAP.md` is its public counterpart.
- **Once after cloning:** `bin/install-hooks`
- **Before the first write of a session:** `bin/worktree new <type>/<kebab-description>`

## Scope (current)

- **Current scope:** the archival core and nothing above it. Capture a URL or crawl from it, store the raw response, canonicalize and dedupe, extract metadata, keep the collection queryable and exportable. Don't expand beyond it without a present need; if a change drifts past it, STOP and flag it.
- Crawling is a dependency, not the product. The crawl engine stays behind an interface this project owns, and the archival semantics belong here: canonical URL rules, dedupe by canonical URL and content hash, per-seed deadlines, timeout and retry policy, rate limit and backoff policy, storage layout, metadata extraction, raw snapshot retention, indexing and export.
- Out of scope for now, on purpose: a hosted service, browser rendering as the default capture path, AI summarization, and a bookmark-manager interface.

<!-- BEGIN universal-principles v3 -->
## Working principles

- **The human defines the WHAT; the agent decides the HOW.** Don't wait for line-by-line dictation. Plan first for non-trivial tasks: show the plan + to-do list, wait for approval.
- **Think before coding — don't assume, don't hide confusion.** State assumptions explicitly; if multiple interpretations exist, present them — don't pick silently. If a simpler approach exists, say so and push back. If a task is impossible under the stated constraints, or info is missing, say so — don't guess. (For trivial tasks, use judgment; this is bias, not ritual.)
- **Surgical changes — touch only what you must.** Every changed line traces to the task. Don't "improve" adjacent code, reformat, or refactor what isn't broken; match existing style even if you'd do it differently. Flag unrelated dead code — don't delete it. Remove only the imports / variables / functions your own change orphaned.
- **Chesterton's Fence — find the problem before undoing the decision.** A config, a flag, a workaround that looks arbitrary is a **fence**: someone put it there, probably to fix something that is invisible to you *because the fence is working*. You arrive with no history, so absence of a visible reason is evidence of your ignorance, not of its uselessness. When your fresh measurement contradicts what the human vaguely remembers ("I changed this once, because of some problem"), **your measurement is the suspect first** — it may be measuring the case that *isn't* failing. Go find the original problem, then decide. *(A CIFS share was benchmarked with a big sequential `dd`, looked fast, and the local-disk download dir was "fixed" away — while the actual failure was random writes: par2, unrar, torrent piece-writes. Two wrong commits.)*
- **Goal-driven execution — define the success check, then loop to it.** Turn the task into something verifiable before coding: "add validation" → write tests for invalid inputs, then pass them; "fix the bug" → write a failing repro test, then pass it; "refactor X" → tests green before and after. For multi-step work, state a brief plan with a verify step each.
- **"Flaky" is not a diagnosis — test in the environment the thing actually runs in.** A component that fails *consistently* under automation is being **mis-invoked**, not being unreliable; "it works when I run it by hand" is not evidence that it works. The shell you test in has a TTY, a `$HOME`, an `ssh-agent`, an interactive stdin — the systemd unit, the CI job and the scripted harness have none of those, so a passing manual run can be testing a different program. Reproduce it *there* (start the unit, `env -u SSH_AUTH_SOCK`, `</dev/null`, `--dry-run` to print the real command line) before accepting "unstable" as a cause. **When a fix doesn't change the symptom, stop fixing and go look at what is actually being executed.** *(An interactive-mode flag with no TTY made one harness fail every review panel for weeks, written off as "flaky"; it was the wrong flag.)*
- **KISS — don't solve a problem you don't have yet.** Simplicity isn't "write less code"; it's not building for a need that doesn't exist. Let structure emerge from the code.
- **YAGNI & flat.** No preventive abstractions, no single-use interfaces. Interfaces for real boundaries only. Architecture is *extracted* once a pattern proves itself in real use — never designed up front for a user who doesn't exist yet. Need pulls architecture.
- **Order: make it work → make it right → make it fast** (Kent Beck), in that order. Most over-engineering is doing "right"/"fast" before a working thing exists to justify it.
- **Flag scope creep — a standing duty, not a suggestion.** When a solo tool starts being framed as a public / multi-user / multi-tenant / plugin-system / configurable-N-backends platform before a real, present need exists, STOP and ask: "Is this needed now?" Justify future-proofing against a need that exists *today*.
- **No silent decisions (comprehension debt).** Never make a silent architectural or design call — state it and record the rationale, so the reasoning is recoverable later.
- **Real decisions are presented in the chat, in isolation — never via popup.** When a design/architecture/scope/trade-off decision arises, surface it on its own: the options, what each means, pros/cons/trade-offs, and a recommendation — then decide together. Don't bury it mid-text or bundle it with other topics, and don't compress it into a quick-pick widget (e.g. AskUserQuestion) — the widget skips the reasoning and overlays the explanation. Widgets are for trivial short-answer picks only.
- **Long answers are written to be scanned, not read twice.** For recaps, status reports, batch reviews, plans, and any comparison of options: lead with the outcome in one line, then break the body into bullets and **bold** the load-bearing terms. Options are always a list — one bullet per option, the recommended one marked — never a paragraph the reader has to parse to find the choices. Reserve unbroken prose for short arguments; a wall of paragraphs costs more in re-reading than the structure would have cost in words.

## Git: branches, commits, PRs, comments

- **Ask the repo for its default branch; never assume one.** Repos differ — `master` and `main` are both common, often in the same person's account — and a wrong guess sends a PR to a branch that does not exist, or, worse, has you "fixing" a URL that was right all along. `git symbolic-ref --short refs/remotes/origin/HEAD | sed 's|^origin/||'`, or `gh repo view --json defaultBranchRef -q .defaultBranchRef.name`. Never commit directly to it: branch, then PR.
- **Branches** — Conventional Branch (conventionalbranch.org): `<type>/<kebab-description>`, types `feature/`, `bugfix/`, `hotfix/`, `chore/`, `release/`, `docs/`.
- **Commits** — Conventional Commits (conventionalcommits.org): `<type>(scope): <description>`, types `feat`, `fix`, `docs`, `refactor`, `test`, `chore`, `ci`, `build`, `perf`, `style`. Breaking change → `!` after the type or a `BREAKING CHANGE:` footer.
- **Atomic commits** — one logical change per commit, each independently green and revertible. Never `git add .` blind; split unrelated changes.
- **Always work in your own worktree — mandatory, not conditional.** Parallel sessions are opened freely and nothing signals their existence to you, so a "check whether another session is here first" step can never be reliable — the honest answer is always "maybe". The only collision-proof arrangement is structural: keep the main working tree on the default branch as a clean reference and **never work in it** — before your first write (commit, branch, rebase, stash; read-only exploration is exempt), create your own worktree and do everything there: `git worktree add ../<repo>-<task> -b <your-branch> <origin>/<default-branch>`. Do this **whether or not** you believe another agent is running — that belief is exactly what you cannot verify. Report which worktree/branch you used; remove it once merged. Only the human can see all the open sessions.
- **Pull requests** — describe **what + why**. *What*: a 1–3 line summary. *Why* (the bulk): decisions, trade-offs, rejected alternatives. The diff shows the what; the PR explains why.
- **Comments** — always **WHY, not WHAT**: explain intent, never restate the obvious mechanics. Keep existing comments; they carry intent.

## Code style (baseline)

- Functions: 4–40 lines, one thing each (SRP). Files: under ~500 lines, split by responsibility.
- Names specific and unique — avoid `data`, `handler`, `Manager`, `util`.
- Explicit types. Early returns over nested ifs; max ~2 levels of indentation.
- Inject dependencies; wrap third-party libs behind a thin interface this project owns.
- No duplication — but don't extract *too early*. Tolerate duplication while the pattern is still forming; extract the abstraction *from* proven, repeated code, never ahead of it.
- **Refactoring is not automatic.** After a large feature, list refactoring candidates (files > ~500 lines, duplicated logic, long functions, hardcoded config) and ask before pruning — the human decides, the tests are the safety net. Consolidate when the thing works and the seams are obvious, not before.
<!-- END universal-principles v3 -->

## Git and secrets

- Before any commit, show `git status` and `git diff --cached`, and confirm no secret is staged. If you spot one, STOP and report it. The gitleaks pre-commit hook is the deterministic backstop; this habit is the probabilistic one.
- Real secrets stay out of git. Only an `.env.example` with fake values is committed.
- The default branch is `main`. Never commit to it directly: branch, then open a pull request.

## Tests (TDD)

- Every feature is born with a test, every bugfix with a regression test.
- Tests run with ONE command (`cargo test`): no manual setup, no credential, nothing that leaves the machine. A test that cannot run headless is wrong.
- Network and filesystem are mocked at the boundary with a named fake, never an inline stub. A test that reaches the live web is not a test, it is a crawl.
- A server the test starts on loopback is the exception, and it is for guards that live inside a dependency and have no reachable entry point. It stays an exception: it costs a socket and a thread, and it is worth that only when asserting the configuration would prove the configuration rather than the behavior.
- Before saying "done", run `bin/ci` and report the result.

## Review

- Before a branch is pushed, one opinion over its whole diff, exactly once. It is a fresh Claude Code instance, started from inside the worktree and given the diff to read.
- The invocation is `claude --safe-mode -p '<prompt>' --allowedTools 'Read' 'Grep' 'Glob' 'Bash(git *)' 'Bash(cargo *)'`. Read-only on purpose: the reviewer reports, and the agent that wrote the code applies the fixes and decides which findings are wrong.
- **`--safe-mode` is the point of the whole gate.** It starts with every customization off: this file, the skills, the hooks, the MCP servers. An opinion formed from the same instructions that shaped the work is not a second opinion, it is the same opinion with a new context window. The project's documents stay readable and the prompt names the ones that matter, because they are evidence about what the code promised rather than instructions about what to conclude.
- The prompt states what changed and why, names the documents that say what the code is supposed to guarantee, and asks for concrete defects ranked by severity, each with the input that triggers it and the wrong result it produces. A finding with no failing case is an opinion about taste and is worth saying so.
- Nothing is pushed until every finding is answered, by a fix or by a written reason it does not hold.
- Escalating or de-escalating for one task is allowed and always announced, never silent.

## Small releases

- Every commit on `main` passes `bin/ci` and is releasable. No "broken commit I fix in the next one".
- Closed work is committed before switching tasks. Flag it when it has not been.

## Security (habit, not a phase)

- This tool fetches attacker-influenced URLs by design. When touching fetching, redirect handling, URL parsing, paths built from remote data, or archive extraction, flag the risk and propose the guard: SSRF and redirects into private ranges, path traversal from a remote filename, decompression bombs, unbounded response bodies, per-host rate limits.
- Archived content stays untrusted forever, not only at capture time. Anything that re-reads the archive treats it as hostile input.
- Dependency CVEs are caught by `cargo audit`, in `bin/ci` and in CI.

## Prose

- No em-dash. Use a comma, a colon, a semicolon or a full stop. `bin/ci` checks this, and it covers Markdown, source comments, config, commit messages and pull request text alike.
- Markdown is soft-wrapped: one paragraph, one line. Rewrapping belongs to whatever renders the text, which is the only thing that knows the reader's width. `bin/ci` checks this too.
- Bold marks structure (a bullet lead-in, a table header), never emphasis in the middle of a sentence. Same for italics: a term being introduced, not a word being stressed.
- No process narration anywhere a stranger can read it: no task ids, no phase names, no review rounds, no mention of a session or a conversation. Commit and pull request text describe the problem and the change, never how the work was organized.
- No audience in the text. A README says what the software does, not who is going to read it.
- Comment density is low by default: the non-obvious only, the why and not the what. Long reasoning belongs in a document under `docs/`, not in a header comment.

## Release

- A release is cut by tagging `vX.Y.Z` on `main`. The release workflow builds the Linux and macOS matrix, publishes one checksum per asset plus `checksums.txt`, and creates the GitHub Release.
- `install.sh` maps `uname` output onto the asset names in that matrix. Adding or renaming a target means updating both, or the installer asks for an asset that does not exist.
- Before tagging: `bin/ci` green, the version in `Cargo.toml` and the tag agree, `Cargo.lock` committed.

## Post-implementation checklist (run before "done")

1. Commits small and well described.
2. Refactoring candidates listed, if the change was large.
3. Security risks flagged, if a sensitive surface was touched.
4. This spec updated if behavior, setup or the release flow changed, and any hurdle it gained is classified rather than merely appended.

## Common hurdles

| hurdle | class | gate |
|---|---|---|
| A fresh clone runs no git hooks until `bin/install-hooks` is run once. Nothing reports this: commits simply pass ungated. | tripwire | none, it is a clone-time step |
| `bin/slop-guard`, `scripts/md-unwrap.py`, `bin/worktree`, `bin/install-hooks` and the git hooks are byte-identical copies of a canonical source outside this repo. Fix the original and re-copy; a local edit is drift that the next sync silently reverts. | prose | none |
| The network path of the crawl adapter (`src/crawl/spider_engine.rs`) is what no test reaches, since a test may not use the network. A change to how the engine is configured compiles and passes `bin/ci` while being broken. `cargo run -- capture <dir> <url> --allow-private-addresses` against a server on localhost is the check. What it still reaches and no test does is a crawl of more than a handful of pages, and the queue between the engine and the archive under real timing. | tripwire | partial: `tests/fetch_hardening.rs` drives the redirect guard, `tests/asset_capture.rs` the single fetch, and `tests/cli_capture.rs` a two page crawl through the binary, each against a server it starts on loopback |
| A crawl at a concurrency of one can lose a link before ever fetching it, inside the vendored engine's own frontier rather than in this project's adapter: it decides the crawl is done by asking whether its own newly-found-links accumulator is empty and nothing is in flight, without asking whether the batch it already knew about still had one left to dequeue, and a concurrency of one is exactly what starves that check into running early. The run still reports `Exhausted`. `hop_depth_guard`'s own bookkeeping in `src/crawl/spider_engine.rs` is compared against what `crawl_seed` actually handed the caller once the crawl claims to be done, and a run that comes up short is refused rather than silently reported as a success. A link a site's `robots.txt` genuinely refuses is excluded from that comparison by asking both matchers, the engine's own `Website::is_allowed_robots` and this project's `RobotRules::allows`, since the two refuse different things and a link either one refuses is a link nothing lost: the engine misses a wildcard anywhere but at the end of a pattern, and this project reads RFC 9309's longest-pattern rule where the engine takes the first rule that matches. A page counted into `pages_dropped` is excluded from the comparison too, by `frontier_claim_is_trustworthy`, since a page the queue lost never reached `on_page` and comparing anyway would report every link the budget legitimately left behind as though this guard had caught it. `depth_key` forces every URL in the comparison onto the seed's own scheme before comparing, matching what the engine's own `push_link` forces onto a link before queueing it, so a hardcoded self link in the other scheme is archived and not reported lost, and a same host link in a scheme the engine will not dial, `ftp://` being the one seen in the wild, is excluded the same way `validate_link` excludes it from the frontier. A page that declares an absolute `<base href>` is left out of the comparison entirely rather than resolved against the wrong base, since this adapter has no way to read the same base back out of `Page` without a second HTML parse of its own. What is not proven is that this project's own href resolution and the engine's always land on the same string for every remaining shape an href can take; measured against an entity-encoded query string and a percent-encoded character, both sides still agree, so no false loss is reported, but a differently-cased host, a relative path with `..` segments and an internationalized domain have not been measured. | tripwire | partial: `a_concurrency_of_one_reports_a_link_it_never_followed_instead_of_a_false_success`, `a_link_disallowed_by_robots_txt_is_not_reported_as_a_lost_link`, `a_link_whose_href_spells_its_query_string_with_an_entity_is_archived_and_not_reported_lost`, `a_page_carrying_an_absolute_self_link_in_the_other_scheme_is_archived_without_a_reported_loss`, `a_page_declaring_an_absolute_base_href_is_archived_without_a_reported_loss` and `a_same_host_link_in_an_unfetchable_scheme_is_not_reported_as_a_lost_link` in `tests/cli_capture.rs` pin the guard against the real engine, `a_dropped_page_makes_the_frontier_claim_untrustworthy` in `src/crawl/spider_engine.rs` pins the `pages_dropped` gate directly, and six cases on `links_discovered_but_never_fetched` in the same file cover the comparison itself |
| An href written with an HTML entity in it, `&amp;` for a literal `&` being the ordinary case, is not decoded before the crawl engine turns it into a URL, so the request that goes out on the wire asks for `x=1&amp;y=2` and the server reads the two parameters `x=1` and `amp;y=2`. Canonicalization now undoes the escape at the address, so the page is filed under the URL it meant rather than a second one, and the tracking rules match the names the page wrote; a run still spends a request per spelling, since a fetch is aimed at the URL as it was found. It costs no link and trips no guard, since every reader of the address, this project's own resolution included, is wrong about it in the same way. Fixed once for metadata extraction in `src/metadata/mod.rs` and once for identity in `src/canonical_url.rs`; what stays unfixed is whatever reads `page.page_links` for its href text before a URL is built from it, which is the layer that would save the request. | tripwire | partial: `an_escaped_ampersand_between_parameters_is_still_a_separator` in `src/canonical_url.rs` covers the address, nothing covers the request |
| A redirect cannot be verified by `cargo run -- capture` against localhost. The engine screens every hop for internal addresses before it checks anything else, so a redirect whose target is on loopback is refused no matter which policy is in force, and the run reports it as a URL that answered nothing. What a local site does exercise is a redirect leaving it, since the target is then not loopback. | tripwire | none, it is a property of the manual run |
| The response byte ceiling reaches the engine through `SPIDER_MAX_SIZE_BYTES`, because both of its configurable byte limits are browser-only and do nothing in this build. It is therefore process-wide, read by the engine on its first fetch, and no test can observe it landing. Renaming the variable, or letting anything run before `SpiderEngine::crawl` sets it, silently removes the ceiling. The command line's `--max-response-bytes` therefore has to be settled at the top of `main`, while the process is still one thread: moved after anything that spawns one, the write stops being sound, and moved after the first fetch it stops being read. | tripwire | none, the reason is in the file |
| A zero on the `Seed` means the opposite thing at each end and neither is what it says. The engine reads a page limit or a depth of zero as no limit at all, so the smallest crawl asked for is an unbounded one; a deadline or request timeout of zero is a budget nothing finishes inside, so every URL is reported as a server that answered nothing and the run leaves with a success. The command line refuses all four, and a caller building a `Seed` by hand gets no such refusal. | tripwire | partial: `a_limit_of_zero_is_refused_rather_than_read_as_no_limit` and `a_budget_no_request_could_finish_inside_is_refused` in `src/cli/capture.rs` cover the command line only |
| The engine silently raises a response byte ceiling under a mebibyte to a mebibyte, so a smaller number is a limit that is reported and never applied. `SMALLEST_MAX_RESPONSE_BYTES` is the floor, and it is a refusal at the caller rather than a correction on the way in: the caller is the only place that can tell somebody. | tripwire | partial: `the_response_ceiling_is_the_run_s_to_choose_and_absent_by_default` in `src/cli/capture.rs` covers the command line only |
| The crawl engine's feature list in `Cargo.toml` is a set of decisions, not a bundle: two features of its `basic` set corrupt an archive silently, one spooling large bodies to disk and reporting them as empty, one attaching a browser fingerprint to every request. Adding features back, or taking `basic` for convenience, reintroduces them. | tripwire | none, the reason is in the file |
| The HTML parser behind metadata extraction matches a token stream, not a tree, so no element is ever implied. A selector written against markup a browser would complete, `head > title` being the one that bit, compiles, passes on every page that spells the tag out, and silently finds nothing on the many that leave it to the parser. | tripwire | partial: `a_page_that_leaves_out_the_tags_a_parser_would_imply_is_still_read` in `src/metadata/mod.rs` |
| An element's namespace is not what separates HTML from what a page embeds. An SVG `<title>` is an HTML integration point, so the parser reports it in the HTML namespace exactly as it reports the page's own title, and a rule written against `namespace_uri` reads as if it works because it does fire for MathML. An ancestor selector is what distinguishes them, and it has to be written once per embedded language: a selector matches a tag name and never a namespace, so `svg title` cannot reach a `<title>` a `<math>` holds and a formula's name was the page's own title until `math title` was written beside it. | tripwire | yes: `a_page_with_only_a_graphic_title_has_no_title`, `a_page_with_only_a_formula_title_has_no_title` and `a_graphic_that_describes_itself_before_naming_itself_still_has_no_page_title` in `src/metadata/mod.rs` |
| Reading hostile markup has two costs that grow faster than the input: the tree parse is quadratic in the number of elements left open, and the readability scoring pass grows with nesting depth far faster than linearly. Each guard in `readability/document.rs` therefore has to run before the step it protects, and a guard moved after it still refuses the page while paying the whole cost. The byte ceiling bounds neither on its own. | tripwire | yes: each ceiling has a pair of tests at its boundary, and `markup_that_only_opens_elements_is_refused_before_a_tree_is_built` pins the ordering |
| The release matrix names the runner labels `ubuntu-24.04-arm` and `macos-15-intel`. They are unverified until the first tag, and a wrong label fails the job at startup rather than at build time. | tripwire | none until the first release |
| A CSS selector reaching `dom_query` from anywhere but a literal is a panic waiting to happen: `Document::select` unwraps the parse, and only `Matcher::new` or `try_select` answer a bad selector with an error. Extraction rules are the first untrusted source of selectors in the tree. | tripwire | yes: every selector in a rule file is compiled when the file is read, and `a_selector_no_parser_will_read_costs_its_host_and_no_other` in `readability/rules.rs` pins it |
| Changing a canonicalization rule silently unfiles every item already stored under the old spelling. The walk re-canonicalizes the address inside each record and refuses one that does not hash to the directory holding it, so those items stop appearing in anything built on the walk while the format version says nothing changed. A rule change is therefore a migration that rewrites the tree, on the same terms `docs/storage-model.md` sets for a field whose meaning changes. | tripwire | none, the reason is on `Misfiled` in `storage/walk.rs` |
| A host's extraction rules never reach a response the host served as Markdown, and cannot: a rule names a subtree of a document that was never parsed. So a `body` or `strip` written for a site applies to its HTML pages and silently does nothing to the Markdown copies beside them, while a repass still counts every one of those captures as worth re-reading for as long as the rule exists. | tripwire | none, the reason is on `extract` in `readability/mod.rs` |
| The readability dependency repairs lazy images by copying an attribute whose value merely contains an image extension over `src` or `srcset`, choosing `srcset` when the value also looks like a candidate list. It fires on any `img[loading="lazy"]` or a class holding `lazy`, which is most of the modern web, and a platform that describes a picture with a JSON descriptor in a data attribute has that descriptor land in `src` and become an address resolved against the page's own path. Nothing in this project's markup reaches it, so a page's `src` is not evidence of what the page wrote. | tripwire | partial: `an_image_whose_source_was_replaced_by_a_descriptor_is_read_from_its_candidates` in `src/readability/markdown.rs` pins the preference that steps around it, and nothing covers the case where the repair overwrote `srcset` instead |
| A run's politeness is whatever the operator asked for and nothing else. `--delay` defaults to zero, so a sitemap phase over hundreds of URLs asks as fast as the host will answer, and the only thing that ever slowed it was how long the subresource pass happened to take. That is not a bound, it is a workload: cutting a page's pictures from eight requests to two took a 250 page run from 22.7 minutes to 1.9 and got 160 of its pages refused with a 429. Any change that makes a capture cheaper makes it ruder, and nothing in the tool notices. | tripwire | partial: `the_sitemap_phase_waits_between_the_pages_it_asks_for` and `a_url_the_run_already_filed_is_skipped_without_waiting_for_it` in `src/capture.rs` pin the flag once it is passed, and `a_capture_the_host_refused_is_counted_apart_from_the_ones_it_served` and `a_refusal_during_the_sitemap_phase_survives_into_the_run_it_is_merged_into` make a refusal visible in the report; nothing makes a run choose a delay for itself |
| A page a site's `robots.txt` refuses is still requested, and only then refused. The engine's frontier takes no predicate from this project, so `crawl_seed` drops the page when it arrives: nothing disallowed is archived, extracted, exported or has its subresources asked for, and the request and the slot it took against the engine's own page limit are both spent. What decides is `RobotRules` in `src/crawl/robots.rs`, which re-implements the matching of one pattern against one path and the precedence between two matching rules, and nothing else about the file: the groups, the comments, the percent-decoding and the empty `Disallow:` are read out of the parse the engine already made. Both of the engine's own matchers are wrong, in different directions, so neither is a fallback: the default one reads a wildcard only at the end of a pattern, and the `regex` feature compiles a robots pattern as a regular expression, which matches different paths, matches unanchored so `Disallow: /subscribe` starts refusing `/x/subscribe`, and ignores `Allow` entirely. | tripwire | yes: `a_disallow_with_an_interior_wildcard_keeps_its_paths_out_of_the_archive` in `tests/cli_capture.rs` drives the real engine against a loopback site serving its own `robots.txt`, and the cases in `src/crawl/robots.rs` cover the matching |
| Nothing outside a crawl consults `robots.txt` at all, not even for a plain prefix. `engine.fetch` builds no parser, so a sitemap phase run without `--max-depth` fetches every listed URL unasked, and so does every subresource, which is deliberate for a subresource and not for a listed page. A rule this project honours during a crawl is therefore honoured or not depending on which phase reached the address. | tripwire | none, it is the phase's own gap |
| A capture record and the metadata record beside it stop naming the same set of references the moment the metadata extractor version moves. The capture is an immutable observation of what one run asked for and missed; the metadata is re-derived under whichever extractor ran last. So a rule change about which references are recorded, keeping one candidate of a `srcset` rather than every one being the case in hand, leaves a capture missing addresses the current metadata does not name, and the invitation in `docs/asset-capture.md` to compare the two counts stops holding across such a bump. A repass reads the role of each retry out of that metadata and falls back to an image for an address it no longer names, which is what every retry was handed over as before roles were read at all. | tripwire | yes: `a_missed_address_the_metadata_no_longer_names_is_still_retried` and `a_missed_address_with_no_role_in_the_record_is_still_asked_for_before_a_script` in `tests/repass.rs` |
| A session cookie sent to the seed's origin travels a redirect hop on the engine's own client, and what keeps it off a third party's host there is `remove_sensitive_headers` in the HTTP client, which this project cannot reach. **The crate that runs is `reqwest` 0.13.4**, the one `spider` depends on and the one `spider::reqwest` re-exports; the 0.12.28 also in the lock file is a build dependency of `spider_fingerprint` and makes no request here, so reading the rule out of it describes source that never executes. In 0.13.4 the comparison is host, port and scheme, each written out separately, so a change in any one of the three drops `Cookie` and no shape survives it. The reusable half belongs to any re-exported dependency: the version that matters is the one the re-exporter depends on, and a lock file holding two copies makes reading the wrong one easy. What is still unguarded is the chain that leaves the origin and comes back, since the header is gone for the rest of the chain while the final address matches the binding again, and `policy_departures` is decided from that final address. No test here can drive a followed redirect at all, because the engine refuses a redirect to a loopback target before any policy is consulted and a test may not leave the machine, so a dependency bump that changed the rule would leak the credential with every gate still green. | tripwire | partial: `the_binding_covers_neither_another_host_nor_the_same_host_on_another_scheme` in `src/crawl/boundary.rs` and `a_capture_a_redirect_took_off_the_credential_s_origin_claims_no_session` in `src/capture.rs` cover the requests this project aims itself, which is every request but a hop inside a chain |
| Assets retried by a repass are not written into the original capture record, whether the retry stores bytes or records that the asset is still missing. The capture id includes the assets present when the capture was filed, so mutating that JSON would make the filename stop describing the record. Use `Archive::read_capture`, which folds in `<capture-id>.assets-recovered.json`; direct JSON reads see only the original observation. | tripwire | yes: `an_asset_missed_by_archive_policy_is_recovered_without_rewriting_the_capture_record` and `a_failed_asset_recovery_is_not_retried_on_the_next_pass` in `tests/repass.rs` |

| The subresource pass of the sitemap phase is handed the instant the run began, and no test can see that it is. A run's deadline bounds the run rather than a phase of it, so `AssetCapture` reads the same clock the phase's own loop guard reads; handed `Instant::now()` instead it would compile, pass every gate, and give each phase's subresources the whole deadline over again. What hides it is that shared instant: a clock that would refuse a file has already refused the page that references it, so the two readings only come apart while a deadline expires inside one page's own subresource pass, which nothing can arrange without sleeping through the budget. The page ceiling and the deadline handed to a sub-crawl are the two places that are pinned, and this is the third. | tripwire | none, the reason is beside the field in `src/capture.rs` |

**A hurdle promoted to a gate is deleted from this table, not duplicated.** The gate is the instruction; a line here restating it only dilutes the ones still unguarded.

<!-- br-agent-instructions-v1 -->

---

## Beads Workflow Integration

This project uses [beads_rust](https://github.com/Dicklesworthstone/beads_rust) (`br`/`bd`) for issue tracking. Issues are stored in `.beads/` and tracked in git.

### Essential Commands

```bash
# View ready issues (open, unblocked, not deferred)
br ready              # or: bd ready

# List and search
br list --status=open # All open issues
br show <id>          # Full issue details with dependencies
br search "keyword"   # Full-text search

# Create and update
br create --title="..." --description="..." --type=task --priority=2
br update <id> --status=in_progress
br close <id> --reason="Completed"
br close <id1> <id2>  # Close multiple issues at once

# Sync with git
br sync --flush-only  # Export DB to JSONL
br sync --status      # Check sync status
```

### Workflow Pattern

1. **Start**: Run `br ready` to find actionable work
2. **Claim**: Use `br update <id> --status=in_progress`
3. **Work**: Implement the task
4. **Complete**: Use `br close <id>`
5. **Sync**: Always run `br sync --flush-only` at session end

### Key Concepts

- **Dependencies**: Issues can block other issues. `br ready` shows only open, unblocked work.
- **Priority**: P0=critical, P1=high, P2=medium, P3=low, P4=backlog (use numbers 0-4, not words)
- **Types**: task, bug, feature, epic, chore, docs, question
- **Blocking**: `br dep add <issue> <depends-on>` to add dependencies

### Session Protocol

**Before ending any session, run this checklist:**

```bash
git status              # Check what changed
git add <files>         # Stage code changes
br sync --flush-only    # Export beads changes to JSONL
git commit -m "..."     # Commit everything
git push                # Push to remote
```

### Best Practices

- Check `br ready` at session start to find available work
- Update status as you work (in_progress → closed)
- Create new issues with `br create` when you discover tasks
- Use descriptive titles and set appropriate priority/type
- Always sync before ending session

<!-- end-br-agent-instructions -->
