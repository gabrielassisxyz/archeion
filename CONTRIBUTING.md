# Contributing

Issues and pull requests are welcome. The project is early, so an issue that describes a problem or a use case is usually worth more than a patch to code that is about to be redesigned. [`ROADMAP.md`](ROADMAP.md) says what is being built next and what will not be built at all.

## Setup

```sh
git clone https://github.com/gabrielassisxyz/archeion
cd archeion
bin/install-hooks   # once: points git at the versioned hooks in .githooks/
bin/ci              # every gate, exactly what CI runs
```

`bin/ci` needs `cargo`, `gitleaks`, `cargo-audit` and `python3`. It names the install command for whichever is missing, and it fails rather than skipping a check it cannot run.

## Conventions

- **Branches** follow [Conventional Branch](https://conventionalbranch.org): `<type>/<kebab-description>`, with `feature/`, `bugfix/`, `hotfix/`, `chore/`, `release/` or `docs/`.
- **Commits** follow [Conventional Commits](https://conventionalcommits.org): `<type>(scope): <description>`. One logical change per commit, each one independently green and revertible.
- **The default branch is `main`.** It is never committed to directly.
- **Every feature is born with a test, every bugfix with a regression test.** Tests run with `cargo test` alone: no network, no credential, no manual setup.
- **Pull requests describe what and why.** The diff already shows what changed. The body is for the decisions, the trade-offs and the alternatives that were rejected.

## Prose gates

Two checks in `bin/ci` cover text rather than code, and both apply to Markdown, source comments, config, commit messages and pull request text:

- **No em-dash.** A comma, a colon, a semicolon or a full stop says the same thing. A line that genuinely needs one carries the marker `allow-emdash`.
- **Markdown is soft-wrapped:** one paragraph on one line. Rewrapping belongs to whatever renders the text, which is the only thing that knows the reader's width. A hard-wrapped paragraph also breaks in ways that change meaning, when a line break lands in front of a character Markdown reads as a block marker.

Run `python3 scripts/md-unwrap.py --write .` to fix the wrapping of a file automatically.

## Scope

Archeion owns the archival layer: canonicalization, dedupe, execution policy, storage layout, extraction, retention, indexing and export. The crawl engine stays behind an interface this project owns. A change that pulls crawler specifics into the archival layer, or that adds a service, a UI or a summarizer, is out of scope as it stands; open an issue before writing it.
