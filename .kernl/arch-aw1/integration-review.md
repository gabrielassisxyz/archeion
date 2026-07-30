# Integration Review

Reviewed integration of `kernl/arch-c9k` into `feat/arch-aw1` through merge commit `7815666` and marker commit `22c4bb2`.

No unresolved merge state or conflict markers were found. `git diff --check HEAD~2..HEAD` passed, and the integrated diff is limited to `.kernl/arch-c9k/implementation-review.md`, `AGENTS.md`, `docs/canonicalization.md`, and `src/canonical_url.rs`.

No regressions were found in the reviewed source diff. Canonicalization now strips the escaped ampersand tail before parameter names are compared against tracking rules, which makes `&amp;utm_source` and `amp%3Butm_source` reduce like a literal query separator while preserving real `amp` parameters. The implementation keeps the documented boundary intact: item identity is corrected, while the originally requested URL remains the request that went on the wire.

Coverage is present for the accepted behavior. `src/canonical_url.rs` includes regression coverage for ordinary escaped separators, percent-encoded semicolon spellings, mixed-case spellings, a first-parameter escaped tail, fixed-point canonicalization, and real `amp` parameters.

Verification passed:

- `rg --hidden -n "<<<<<<<|=======|>>>>>>>"`
- `git diff --check HEAD~2..HEAD`
- `bin/ci`

The Go gate from the orchestration template is not applicable in this Rust worktree: `orchestrator/go.mod` is absent, while the project gate defined by `AGENTS.md` is `bin/ci`.

VERDICT: PASS
