# matcher fixture set

Version-difference matrix for the M6 explainable matcher
(docs/integration/RESOURCE_DESIGN.md §3.5).

Test type: **simulated fixture data**, not verbatim live captures
(tasks.md §15.4 separation). Every case is hand-written metadata; passing
these tests proves the scoring/penalty/confidence contract only, never any
provider endpoint. The per-provider directories of this fixture tree hold
the live-capture sets and say so in their own READMEs.

Schema (`version_matrix.json`):

- `target` / `candidate`: provider, id, title, artists, album,
  duration_secs, isrc. The provider/id pairs are synthetic; no title is
  ever used as a cache key by the matcher.
- `expect_score_min` / `expect_score_max`: inclusive bounds on
  `match_breakdown(..).total()`.
- `expect_confidence`: `Low` / `Medium` / `High` (RESOURCE_DESIGN.md §3.4).
- `expect_flags`: subset of `version_token_mismatch`,
  `multi_artist_mismatch`, `duration_unknown`.

Run: `cargo test --locked --no-default-features --lib matcher::` (core, no
feature gate).
