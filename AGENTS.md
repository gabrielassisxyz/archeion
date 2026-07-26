# Archeion, agent briefing

> Read before every interaction. Living spec: short, imperative. On every gotcha or decision, append one line here.

> **What it is:** a local-first archival tool that captures web content and its metadata into a durable personal collection. **Calibration:** Tier 2 · Phase: work. External stakes are contained (single operator, no hosted service, no third-party data), personal stakes are high: this is a public developer tool meant to grow. Update the phase as the project moves work, then right, then fast; an agent reads this line to decide how much rigor a change deserves. **Review gate:** standard. One independent external opinion over the whole branch diff, exactly once, pre-push. No per-commit reviews. Escalating or de-escalating for one task is allowed and always announced, never silent.

## Stack and commands

- **Stack:** Rust, 2024 edition, toolchain 1.96 or newer. Single binary crate.
- **Build:** `cargo build`
- **Run:** `cargo run`
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
- Tests run with ONE command (`cargo test`): no manual setup, no network, no credential. A test that cannot run headless is wrong.
- Network and filesystem are mocked at the boundary with a named fake, never an inline stub. A test that reaches the live web is not a test, it is a crawl.
- Before saying "done", run `bin/ci` and report the result.

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
| The network path of the crawl adapter (`src/crawl/spider_engine.rs`) is what no test reaches, since a test may not use the network. A change to how the engine is configured compiles and passes `bin/ci` while being broken. `cargo run --example capture_seed -- <url> <dir>` against a server on localhost is the check. | tripwire | none, it is a manual run |
| The crawl engine's feature list in `Cargo.toml` is a set of decisions, not a bundle: two features of its `basic` set corrupt an archive silently, one spooling large bodies to disk and reporting them as empty, one attaching a browser fingerprint to every request. Adding features back, or taking `basic` for convenience, reintroduces them. | tripwire | none, the reason is in the file |
| The release matrix names the runner labels `ubuntu-24.04-arm` and `macos-15-intel`. They are unverified until the first tag, and a wrong label fails the job at startup rather than at build time. | tripwire | none until the first release |

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
