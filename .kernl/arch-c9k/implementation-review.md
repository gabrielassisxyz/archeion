# Implementation Review

Plan file `.kernl/arch-c9k/plan.md` was absent in this worktree, so the review used the implementation commit and the bead acceptance criteria.

The implementation satisfies the required behavior. `src/canonical_url.rs` removes the escaped ampersand tail before reading parameter names, so `utm_*` tracking parameters are still recognized after either `&amp;` or `amp%3B` has reached canonicalization. The regression coverage compares the bare `&` spelling with the `&amp;` spelling, the percent-encoded `amp%3B` spelling, a mixed-case spelling, and the observed first-parameter `amp%3Butm_medium` case.

The chosen correction point matches the accepted contract for this defect. Canonicalization now fixes item identity for addresses from crawl links, sitemaps, and direct inputs, while the documentation and `AGENTS.md` clearly record that this does not change the original request sent on the wire.

No source defects were found in the reviewed diff. The additional guard for real `amp` parameters is covered by `a_parameter_actually_named_amp_is_left_alone`, and the canonical fixed-point test includes escaped forms.

Verification passed:

- `cargo test canonical_url -- --nocapture`
- `bin/ci`

VERDICT: PASS
