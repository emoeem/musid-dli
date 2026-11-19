# Resource capability design: artwork, cache domains, matcher scoring, health semantics (M6 core)

Design date: 2026-11-20. Written by the M6 designer pass; every anchor below was
read live from the working tree in the same session (paths and line numbers cite
the file at design time). This document freezes the contract the M6 core
implementation in the same slice follows; deviations found during
implementation are recorded in the "Landed deltas" register (§12).

Landing status (2026-11-20): all nine M6 items are landed and passed their
gates, m6-lyrics-layering (`producer`), m6-lyrics-pipeline (`gate`),
m6-artwork-resolver, m6-search-aggregation, m6-matcher-explainable,
m6-resolve-quality-fallback, m6-cache-domain-split and m6-health-semantics
(the aggregate selector `feature`), m6-lyrics-helper-audit (the literal
`"Cargo.toml"` selector). §1-§11 describe the landed contracts, §12 registers
the implementation deltas D1-D39 plus the per-slice verification tables, and
the second review round's six findings (four substantive leftovers plus two
untrue-statement corrections) are closed as D27-D30 with the D22 in-place
correction and the D31/D32 minor observations; the round's consolidated gate
battery, with the fix session's reported numbers checked against this
documentation pass's re-measured values, is the last table in §12.

Inputs this design builds on:

| Input | What it contributes |
|---|---|
| M0 `docs/integration/BASELINE_AUDIT.md` | repository/feature/test baseline, honest verification vocabulary |
| M2 `src/error.rs` (`Error`, `ErrorCategory`, `Error::category()`, `is_retryable()`) | the classification source every M6 health/negative-cache mapping reads from |
| M3 `src/models/{track,stream,lyrics,namespace}.rs`, `src/lyrics_convert.rs` | the models M6 extends additively (`LyricDocument`, `StreamInfo`, `Track`) |
| M3 §5.5 of `docs/integration/MODEL_ALIGNMENT_DESIGN.md` | the "no ProviderArtwork face in M3 core" decision this slice executes |
| M5 `src/local_cover.rs` | the reusable local cover face (`read_cover`, `content_hash`) behind the local artwork source |
| `docs/tasks.md` §15 | minimal-modification rules, real/simulated test separation |

## §0 Scope and hard constraints

M6 adds four resource capabilities to the core library:

1. an **Artwork resolver face** (trait, models, registry slot, one local
   implementation; network implementations are provider-slice work),
2. **cache domain split** (positive, negative, session-namespaced, expiry
   semantics, per-provider TTL),
3. **explainable matcher scoring** (per-part breakdown, version-token
   penalties, explicit confidence),
4. **health semantics** (failure classes derived from M2 categories,
   per-class counters, probe attempts/retry fields).

Plus two pipeline slices that consume the above: **search aggregation**
(pagination, cancellation, typed errors) and **lyrics layering/pipeline**
(document-level to line-level normalization, lyrics cache, negative cache).

Hard constraints carried from the ask, and how each is met:

| Constraint | Mechanism |
|---|---|
| Cache keys stable: ID / URL / hash, never a song title | `StreamCache` keeps `(namespace, TrackId, StreamQuality)` (cache.rs:14 precedent); `NegativeKey` variants carry `TrackId` / provider id / provider-side resource id; `LyricsCache` keys on `TrackId` and `(provider, lyric id)`; `ArtworkKey::cache_key()` is `track:{provider}:{id}` / `album:{provider}:{id}` / `artist:{provider}:{id}` / `local:{canonicalized path bytes}` (canonicalized + losslessly hex-encoded on unix, §1.4). No text-shaped or title-shaped key exists anywhere. |
| No unbounded caches or unbounded concurrency | every new cache has a `capacity` with earliest-expiry eviction (the `StreamCache` eviction rule, cache.rs:56-64); `TtlCache`/`NegativeCache`/`LyricsCache`/`ArtworkCache` all share the same rule; aggregation pagination is capped by `AggregationOptions::max_pages`; join sets stay bounded by the registry/source slice lengths (search.rs:46, lyrics.rs:212 precedent). |
| Low-confidence matches carry confidence explicitly | every `MatchCandidate` carries `confidence: MatchConfidence`, `flags: MatchFlags` and `breakdown: ScoreBreakdown` (§3); `MatchConfidence` cannot be omitted, the ranking tie-breaks confidence-aware, and the two true properties (penalty gate: a penalized pair is never High; confidence tie-break at equal score) are pinned (§3.4). A penalized `Medium` CAN legitimately outrank a clean `High` under score-primary ordering; take-first callers must consult confidence. |
| Health classification carries no sensitive data | `HealthFailureClass` is a 5-variant enum derived from `ErrorCategory` (error.rs, 24 variants); counters are plain `u32`s. No message text, no URL, no cookie/token material enters the snapshot: the same non-sensitivity contract as `ProviderProbeResult` (health.rs:47-64). |
| 9+2 providers and existing tests not broken | all changes additive: new types, new methods, `#[serde(default)]`/`..Default::default()` fields; `ResolveOptions` (provider/traits.rs:110-120) is deliberately NOT extended because its struct literals appear in every provider test/example; the two modified existing literals are `AggregationOptions` in search.rs timeout tests and `ResolvePipeline` in resolve.rs tests (fields added, assertions untouched). |
| no-default boundary preserved | zero new dependencies. `artwork` and `lyrics-cache` are pure gate features with empty dependency lists (lx-js-bridge precedent, Cargo.toml:55); `artwork-local` names the `cfg(all(artwork, local-cover))` union exactly the way M5 s3's `cfg` feature names the tag-write union (Cargo.toml:98, FEATURE_DEPENDENCY_MATRIX.md §1). |
| Cached resources never outlive their validity | `StreamCache` caps entries by the URL's absolute `expires_at_unix` (cache.rs:111-123 rule); `ArtworkCache::put` caps its positive entry the same way by `Artwork::expires_at_unix` and the fastest `ArtworkVariant::expires_at_unix`; `LyricsCache` stores partial search results only provisionally, expiring with the failing sources' negative window (§8.3). |

## §1 Artwork resolver face (m6-artwork-resolver)

### §1.1 Position vs M3 §5.5

M3 §5.5 (MODEL_ALIGNMENT_DESIGN.md:687-696) decided: no `ProviderArtwork`
face in M3 core, relocation documented, "adding a trait face is provider-slice
work (every provider would need a live implementation; an empty default face
would violate the no-empty-shells rule, tasks.md §1.3#7)". M6 executes the
relocation under exactly that rule:

- the **trait and models compile always** (core, no feature gate) so
  third-party providers can implement the face without paying for any of ours;
- **no default method bodies**: `ArtworkProvider` has required methods only.
  None of the eleven registered providers implement it in this milestone, so
  none of them registers the artwork slot; the slot exists, is
  conflict-checked, and stays empty until a provider slice lands a real
  implementation (same posture as the `download` bit's "declared but never
  merged" boundary, AUTH_REGISTRY_DESIGN.md §5.6, except here the slot is
  real and usable);
- `Track.artwork_url` (models/track.rs:77) stays the lightweight single-URL
  view; it is not deprecated and not removed.

### §1.2 Types (`src/artwork.rs`, always compiled)

```text
ArtworkKind    Track | Album | Artist | Local          (what is resolved)
ArtworkKey     Track(TrackId) | Album{provider,id} | Artist{provider,id} | Local{audio_path}
ArtworkSize    Original | Side(u32)                     (request-side hint)
ArtworkFormat  Png | Jpeg | Webp | Gif | Unknown        (sniffed, not trusted)
ArtworkVariant { url, local_path, width, height, format, bytes_sha256, expires_at_unix, headers }
Artwork        { provider, key, variants (ordered, best first), resolved_at_unix, expires_at_unix }
ArtworkProvider: MusicProvider  { fn id(); async fn artwork(&self, key, size) -> Result<Option<Artwork>> }
```

Semantics pinned here:

- **Four source kinds** map onto the four `ArtworkKind` values: song cover
  (`Track`), album cover (`Album`), artist avatar (`Artist`), local
  embedded/external (`Local`). A provider that cannot serve a kind returns
  `Error::Unsupported` for it; never `Ok(None)`, which is reserved for
  "looked up, genuinely absent".
- **Missing vs placeholder is explicit**: `Ok(None)` means the provider has no
  artwork for this key. M6 ships no placeholder synthesis anywhere; a host
  that wants placeholder art owns that decision. This is the same
  "no fake producer" rule the QR `image_png` slice applied
  (MODEL_ALIGNMENT_DESIGN.md §6 landed deltas).
- **Multi-size variants**: `Artwork::variants` is ordered best-first; each
  carries its own size, format, expiry and request headers. Request-side
  `ArtworkSize::Side(px)` is a hint; providers map it onto their own discrete
  size parameters (e.g. netease `?param=NNNyNNN`) and still return whatever
  variants they actually have.
- **Transport metadata**: variants carry `headers` and `expires_at_unix`
  because cover CDNs commonly need Referer/UA and signed URLs expire; the
  same reason `StreamInfo` carries them (models/stream.rs:96-109).
- **`bytes_sha256`** is filled when the producer has the bytes (the local
  face computes the existing `local_cover::content_hash`, local_cover.rs:118-121);
  it is the dedup/persistence key for hosts, never required.
- Cache keys: `ArtworkKey::cache_key()` (§0 table). Album/artist ids are
  provider-scoped strings exactly like `TrackId` (models/track.rs:5-17).

### §1.3 Registry slot (12th)

`src/registry.rs` gains slot `artwork`, map
`BTreeMap<String, Arc<dyn ArtworkProvider>>`, and the full registration
surface (`try_register_artwork` / `register_artwork` / `unregister_artwork` /
`artwork_provider`), matching the eleven-slot pattern
(registry.rs:171-306). `ProviderCapabilities` gains an `artwork: bool` bit
(default false; every existing literal uses `..Default::default()`, verified
by a structural scan this session), merged in `record()` and
`live_capabilities()` like every other bit. `unregister_all` removes the slot
and keeps its health-reset behavior (registry.rs:405-420).

The existing registry test
`try_register_covers_all_eleven_slots_with_matching_slot_names` is **extended
in place** to twelve slots (same macro shape, new duplicate checks); no
assertion is deleted. Renamed to `..._twelve_slots_...` to keep the name
truthful.

### §1.4 Local implementation (`src/artwork_local.rs`, gated)

Gate: `#[cfg(all(feature = "artwork", feature = "local-cover"))]`, selected by
name through the new gate feature `artwork-local = ["artwork", "local-cover"]`
(the M5 s3 `cfg`-union precedent, Cargo.toml:90-98). `LocalArtwork` wraps
`local_cover::read_cover` (local_cover.rs:253-267) and its
`CoverPriority::EmbeddedFirst`/`ExternalFirst` priority; the returned path is
decorated with dimensions via `local_cover::probe_dimensions` (:153-162), the
sniffed format and the `content_hash` sha256. `read_cover` already owns the
on-disk cache (host-specified dir, validated store, corrupt-rebuild), so the
local face does not add a second cache layer.

`LocalArtwork::artwork` resolves only `ArtworkKey::Local`; Track/Album/Artist
keys fail with `Error::InvalidInput` (this source has no provider-side ids).
`Ok(None)` means neither the embedded nor the external face produced a usable
path: exactly `read_cover`'s `None`. The `Local` cache key is stable across
path spellings and collision-free: `ArtworkKey::cache_key` canonicalizes the
path (relative spellings, `..` segments and symlinks collapse to one key) and
encodes the raw path bytes losslessly (hex on unix; other platforms fall back
to the lossy string, the only byte view std offers there, noted as a platform
limitation).

`ArtworkCache` (`src/artwork.rs`, always compiled) is the bounded TTL cache
network faces are expected to sit behind: keyed by `cache_key()`, TTL'd,
capacity-evicted, plus a negative "missing" domain so a genuinely absent
cover is not re-requested every paint (§2). Its positive TTL is capped by the
artwork-level and per-variant absolute expiry, so a signed cover URL never
outlives its validity. It is a plain type, not wired into
any provider in M6.

## §2 Cache domain split (m6-cache-domain-split)

### §2.1 Current shape and the gap

`StreamCache` (cache.rs:13-85) is a single positive domain:
`(TrackId, StreamQuality) -> StreamInfo`, TTL capped by the URL's own
`expires_at_unix`, capacity-evicted, stable keys. There is no negative cache
(`grep -rn negative src/` was zero-hit in the inventory pass and re-verified
this session before implementation), no per-provider TTL, and no session
namespace: two accounts of one provider share URL entries even when the
platform differentiates URLs per cookie (session.rs ProviderSession is pure
auth payload; the cache never saw it).

### §2.2 Domain table

| Domain | Type | Key | TTL source | Eviction |
|---|---|---|---|---|
| Stream positive | `StreamCache` (extended) | `(namespace, TrackId, StreamQuality)` | per-provider table, else default; absolute URL expiry caps both (cache.rs:40-52 rule kept) | capacity, earliest expiry |
| Stream negative | `NegativeCache` | `NegativeKey::Stream{TrackId, quality}` | category TTL (§2.3) | capacity, earliest expiry |
| Lyrics search positive | `TtlCache<(TrackId, SearchMode), Vec<LyricSearchResult>>` inside `LyricsCache` | `TrackId` + mode | `search_ttl` (default 10 min); partial successes are provisional, capped by the failing sources' negative window (§8.3) | capacity |
| Lyrics document positive | `TtlCache<(provider, id), Option<LyricDocument>>` inside `LyricsCache` | `(provider, provider-side lyric id)`; `None` = cached absence | `document_ttl` (default 24 h) | capacity |
| Lyrics negative | `NegativeCache` | `LyricsSearch{TrackId, mode, source}` / `LyricsDocument{provider,id}` | category TTL | capacity |
| Artwork positive | `ArtworkCache` = `TtlCache<String, Artwork>` | `cache_key()` (§1.2) | configured TTL (default 1 h), capped by the artwork/variant absolute expiry | capacity |
| Artwork negative (missing) | inside `ArtworkCache` | `cache_key()` | configured missing TTL (default 10 min) | capacity |

(A track-search negative row existed in the first draft and was removed in
review: the aggregation face takes no cache collaborator yet, so the
variant was an unwired type surface; register in §12. If search negatives
land later, they key on provider + hashed query and ride the same
`NegativeCache`.)

### §2.3 Negative TTL semantics

`NegativeTtlPolicy` holds a default TTL, a per-category table and a
non-cacheable set. `NegativeTtlPolicy::standard()`:

- retryable categories (the `Error::is_retryable()` set: Network, Timeout,
  RateLimited, Unavailable, error.rs:362-370): **15 s**. Short, because the
  condition may clear on the next attempt.
- the whole credential trio (`SessionExpired`, `AuthenticationRequired`,
  `AuthenticationFailed`): **30 s**. The principle "a stale credential
  state must not outlive the fix for it" applies to a fresh login just as
  much as to a session refresh; `NegativeCache::clear_provider(provider)`
  invalidates the provider's entries wholesale for both.
- transient contract/protocol/internal states (`InvalidResponse`,
  `Protocol`, `ProviderInternal`) and the legacy unclassified `Provider`
  (error.rs:269): **60 s**. These are flaky-provider states, not stable
  resource facts; the resolve invalid-URL arm records `InvalidResponse`,
  which must not poison a track for 10 minutes.
- everything else: **10 min** default (NotFound/region/vip/copyright are
  stable-ish resource facts).
- **non-cacheable set**: `Cancelled`, `InvalidInput`, `Serialization`, `Io`,
  `Unsupported`, `UnsupportedOperation`. These describe the caller or the
  build, not the provider's state; caching them would poison unrelated
  requests. `record` on these is a no-op.

`NegativeCache::check(key)` removes-and-returns-None on expiry (the
StreamCache get rule, cache.rs:29-36). All record/check/clear are
capacity-bounded and lock-poisoning tolerant (`unwrap_or_else(|e|
e.into_inner())`, the file-wide precedent).

### §2.4 Session namespace

`StreamCache::namespaced(namespace: &str) -> NamespacedStreamCache<'_>`
returns a borrowing view whose get/insert operate on keys prefixed by the
namespace. The internal key becomes `(String namespace, TrackId,
StreamQuality)`; the legacy `get`/`insert`/`clear`/`len` methods operate on
the empty namespace, so **every existing caller and test keeps its
byte-for-byte behavior**. Namespace content is a host-chosen stable string;
the intended value is `"{provider}\u{1f}{account_id}"` from `SessionStore`
keys (session.rs:157, `get_for`/`set_for`), which makes same-provider
different-account URL entries disjoint. Account ids are already validated
non-empty (session.rs:146-151). The view adds no allocation domain of its
own: entries live in the same bounded map, so a namespace cannot exceed the
cache capacity.

### §2.5 Per-provider TTL

`StreamCache::set_provider_ttl(&self, provider: &str, ttl: Option<Duration>)`
maintains an interior `RwLock<BTreeMap<String, Duration>>`;
`insert` computes the effective TTL as
`table.get(provider).copied().unwrap_or(self.ttl)` before applying the
existing absolute-URL cap. `StreamCache::new(ttl, capacity)` signature and
behavior unchanged. Use case: netease URLs live ~20 min, bilibili URLs
shorter; hosts tune per provider without a cache per provider.

## §3 Matcher explainable scoring (m6-matcher-explainable)

### §3.1 Current scoring and the gap

`match_score` (matcher.rs:37-75): ISRC hard gate 100/0, title
equal40/contains30/bigram30, artist Jaccard 30 (artist_similarity :77-97),
duration 25/18/10/4/0 (:56-65), album 5. `MatchCandidate` (:5-8) carries only
the total `u8`. Failure mode: `Song (Live)` normalizes to `songlive`, contains
`song` (normalize :98-104 strips separators), so a live version scores 30
title + 30 artist + 25 duration = 85 and passes a caller's threshold as an
equal match. No version detection, no confidence, two tests total (:140-154).

### §3.2 Breakdown

```text
ScoreBreakdown { title, artist, duration, album: u8,          // the five existing parts
                 version_penalty, artist_penalty: u8,          // new penalties (capped)
                 isrc: Option<bool>,                           // Some = gate decided
               }
ScoreBreakdown::total() -> u8   // isrc gate short-circuits; else sum minus penalties
```

`match_breakdown(left, right) -> ScoreBreakdown` is the new public function;
`match_score` keeps its exact signature and returns
`match_breakdown(..).total()`, so **every existing score is preserved unless
a penalty flag fires** (§3.3). Threshold semantics do not change: no caller's
`minimum_score` moves.

### §3.3 Version tokens and penalties

`version_tokens(title) -> Vec<&'static str>` detects the token list
`live / remix / cover / instrumental / karaoke / acoustic / demo / 伴奏 /
纯音乐 / 翻唱 / 现场 / 改编` on the lowercased raw title: ASCII tokens match
whole separator-delimited segments, CJK tokens match substring (CJK has no
separators). Detection is symmetric per side; the **penalty fires only on
asymmetry**: token classes present on one side and absent on the other.
Penalty: 15 per asymmetric class, capped at 30, written into
`version_penalty` and flagged as `MatchFlags::version_token_mismatch`.

Consequences: `Song` vs `Song (Live)` loses 15; `Song (Live)` vs `Song
(Live)` loses nothing; the existing exact-match test (matcher.rs:141-146) and
the ISRC-conflict test (:148-154) are untouched because their pairs have no
asymmetric tokens. Existing passing sets cannot shrink: any pair that passes
today and still passes after M6 either has no detected asymmetry (score
identical) or is ISRC-gated (gate short-circuits). This is the pin the ask
demands ("惩罚仅作用于新检测出的版本 token，不缩窄现有通过集").

Multi-artist asymmetry: when both sides carry more than one artist and
neither artist set is a subset of the other (collaborations recorded
differently), a 10-point `artist_penalty` fires with
`MatchFlags::multi_artist_mismatch`. Single-artist pairs are governed by the
unchanged Jaccard rule.

### §3.4 Confidence

```text
MatchConfidence { Low | Medium | High }
confidence_of(score, breakdown): High  iff score >= 85 AND version_penalty == 0 AND artist_penalty == 0
                                 Medium iff score >= 60
                                 Low    otherwise
```

Thresholds are public constants (`HIGH_CONFIDENCE_SCORE = 85`,
`MEDIUM_CONFIDENCE_SCORE = 60`). A penalized pair can never be High even when
its raw total clears 85: a live-version match that squeaked past a threshold
must present as Medium (explicit low-confidence carry).

Ordering (review-hardened, round 2): `rank_alternatives` sorts by score
descending, confidence descending as tie-break, then provider/id. Exactly
two global properties hold, both pinned:

1. **The penalty gate**: a penalized pair is **never `High`**.
   `confidence_of` requires zero penalties at or above the 85 floor
   (`!penalized`), whatever the score. Pinned by
   `penalized_examples_stay_medium`, which also demonstrates a penalized
   86-point near-superset pair clearing the floor yet reporting Medium.
2. **The confidence tie-break**: at an equal score the clean pair comes
   first. The tie is real: normalized-equal titles can carry divergent raw
   tokenization (`Song (Live)` and `SongLive` both normalize to
   `songlive`, so both score 40 on title, and the asymmetric `live` token
   penalizes exactly one side down to 85 Medium while a clean pair reaches
   85 High). Pinned by
   `clean_high_wins_the_score_tie_against_a_penalized_variant`.

There is deliberately NO claim that a penalized score stays below the High
floor in general (the first draft claimed one; review round 2 corrected
it): a normalized-equal-title penalized pair reaches exactly 85, and
near-superset multi-artist casts (13 artists sharing 12, Jaccard 12/14)
score 86 after the penalty. A penalized `Medium` can therefore
legitimately outrank a clean `High` under score-primary ordering.
Take-first callers MUST inspect `confidence`/`flags` before presenting the
top candidate as the recording; a caller that cannot tolerate that must
filter on `confidence == High` itself.

### §3.5 Version-difference fixture (tasks.md §15.4 separation)

`tests/fixtures/matcher/version_matrix.json` plus
`tests/fixtures/matcher/README.md`: a simulated (non-live-capture) matrix
covering exact / live-asymmetric / live-symmetric / remix / 伴奏 (CJK token)
/ multi-artist mismatch / ISRC-equal / ISRC-conflict /
same-title-different-artist / duration-unknown. Each case pins expected
flags, an expected confidence, and score bounds. The fixture is loaded with
`include_str!` from matcher's test module; it is a fixture test of internal
functions, not a network test, and states so in its README (the same
per-directory README convention as the provider fixture sets).

## §4 Health semantics (m6-health-semantics)

### §4.1 Failure classes

The health layer currently counts one `failures` u32
(health.rs:70-73, record_failure :126-142): every failure is the same
failure. M2 already carries the real classification (`Error::category()`,
error.rs:266-294; 22 categories), but call sites compress to `String` before
health sees it (search.rs:69,75; resolve.rs:145). M6 fixes it at both ends:

```text
HealthFailureClass { Unreachable, RateLimited, AuthInvalid, Maintenance, Unknown }
class_from_category(ErrorCategory) -> HealthFailureClass:
    Network | Timeout                     -> Unreachable
    RateLimited                           -> RateLimited
    SessionExpired | AuthenticationRequired | AuthenticationFailed -> AuthInvalid
    Unavailable                           -> Maintenance
    everything else                       -> Unknown
```

This is the 不可达/限流/认证失效/维护中/未知 split the ask names, total over
all 24 `ErrorCategory` variants. `Unavailable` (M2 variant, error.rs:97-103)
is the only maintenance source; `ProviderInternal` stays Unknown (a provider
bug is not maintenance); `Io` stays Unknown (a local I/O fault is the
caller's machine failing, not the provider being unreachable, and must not
pollute a provider's reachability counters). No category text, no URL, no
message enters the snapshot: counters only (§0 constraint).

Breaker interaction (review-hardened): an `AuthInvalid` failure does not
increment the breaker total and does not open a Closed circuit, but it is
NOT a free pass through the state machine: a failed half-open probe is
released back to `Open { cooldown }`. Without the release,
`probe_in_flight` would stick `true` forever after a session expiring
during the half-open probe (the normal netease/qq case) and `try_acquire`
would never admit the provider again until process restart.

### §4.2 Counters, snapshot, breaker interaction

- `record_failure_with_class(provider, class)` is the new primitive.
  **AuthInvalid does not advance the circuit breaker** (no `failures`
  increment, no opening from Closed), but it releases a failed half-open
  probe back to `Open { cooldown }` so the state machine can never wedge
  (see §4.1): an expired credential is a session event, not provider
  degradation. It is recovered by
  `AuthenticationProvider::refresh_session` (provider/traits.rs:475-482), and
  tripping the circuit would block exactly the refresh that fixes it. It IS
  counted in the class counters so the state is observable.
  All other classes keep the exact breaker logic of record_failure
  (health.rs:126-142: threshold, HalfOpen re-open).
- `record_failure_with_category(provider, category)` maps through §4.1.
  **Legacy `record_failure(provider)` keeps its signature and delegates to
  `record_failure_with_class(Unknown)`**: every existing caller and test
  (registry.rs:1040-1083, health.rs:177-193) behaves identically.
- `ProviderHealthSnapshot` gains `failures_by_class: HealthFailureCounts`
  (a Copy struct of five u32s). The struct is not serde-serialized anywhere
  today, so additive fields have no wire impact; `PartialEq`/`Eq`/`Copy`
  derives retained.
- Call sites pass the category they already hold: search aggregation and the
  resolve pipeline call `record_failure_with_category` with the `Error`'s
  category (§5.4, §6.4); timeouts map to `ErrorCategory::Timeout`; a
  resolved-but-invalid URL maps to `InvalidResponse`.

### §4.3 Probe fields

`ProviderProbeResult` (health.rs:47-64) gains
`attempts: Option<u32>` and `retry_after_ms: Option<u64>`, both
`#[serde(default)]` (the §1 R1 rule: old probe JSON loads with documented
defaults; the existing no-`result_count` test, health.rs:221-231, keeps
passing and is joined by a new-key round trip). `attempts` counts probes
including retries; `retry_after_ms` mirrors a Retry-After the platform gave.
No other probe shape change; the "deliberately independent of the snapshot"
decision (health.rs:35-44) stands.

## §5 Search aggregation (m6-search-aggregation)

### §5.1 Existing base (kept, unchanged signatures)

per-provider timeout (search.rs:57), single-provider error isolation
(:65-86), circuit-breaker cooperation (:52-75), dedup (:95-113). The public
`aggregate_search` / `aggregate_search_with_health` return shapes stay
`(Vec<Track>, Vec<(String, String)>)`.

### §5.2 Pagination

`AggregationOptions` gains `max_pages: u32` (default 1 = today's behavior),
`dedup_duration_bucket_secs: u64` (default 3 = today's `as_secs()/3`,
search.rs:108) and `cancel: Option<SearchCancelToken>`: all via the existing
`Default`, so `AggregationOptions { timeout, deduplicate,
..Default::default() }` keeps compiling. The per-provider job now loops
`page = query.page ..< query.page + max_pages`: each iteration builds a
`SearchQuery` clone with the incremented page, stops when `has_more == false`
(the field every provider already fills, provider/traits.rs:104-109), when a
page returns no items, on error, or when cancelled. Total fetches are bounded
by `providers × max_pages`; no unbounded continuation.

### §5.3 Cancellation

`SearchCancelToken` (`Arc<AtomicBool>`, custom `Debug`, `Clone`) with
`cancel()` / `is_cancelled()`. Cooperative by design and documented as such:
the token is checked before each page and before each provider's job starts;
an already-in-flight request finishes its own timeout window (aborting a
reqwest future mid-flight from a shared handle would need spawn-scoped Join
handles and buys nothing over the existing per-provider timeout).
Cancellation is **aggregate-level** (review-hardened): after all jobs
return, one check of the token decides for the whole aggregate, and a
cancelled run returns zero tracks, not even the results of providers that
finished before the flag was observed. A per-provider-only clear would
quietly keep the other providers' pre-cancel data while telling the caller
"cancelled"; a global token cancels the aggregate, so the drop is global.
The same token type is reused by the lyrics search path (§8.3).

### §5.4 Typed errors

```text
ProviderSearchError { provider: String, category: ErrorCategory, message: String }
AggregatedSearch    { tracks: Vec<Track>, failures: Vec<ProviderSearchError> }
aggregate_search_typed(registry, query, options) -> AggregatedSearch
```

Category mapping: provider `Err(e)` maps to `e.category()` (the M2
classification finally reaches the caller); timeout maps to `Timeout`;
circuit-open maps to `Unavailable` (the breaker opens on repeated failures;
"degraded" is the honest health category, and
`record_failure_with_category(Unavailable)` matches). The legacy tuple
functions are thin adapters over the typed one (message =
`error.to_string()`, identical strings: the existing timeout test's
`errors == vec![("slow", "provider search timed out")]` assertion,
search.rs:216-219, pins this). `record_failure_with_category` is used at
every failure site so the health class table (§4) fills from real categories.

### §5.5 Dedup bucket

`deduplicate_tracks_with_bucket(tracks, bucket_secs)` is the new primitive;
`deduplicate_tracks` delegates with 3 (old behavior pinned by the existing
test, search.rs:126-150). ISRC-first rule unchanged (search.rs:98-100).

## §6 Resolve: quality chain, fallback record, provider order (m6-resolve-quality-fallback)

### §6.1 Kept guarantees

Cross-source ID isolation (each candidate resolves only via
`registry.stream_provider(track.id.provider)`, resolve.rs:93-99), stable
`(TrackId, quality)` cache keys (cache.rs:14), expired-URLs-never-returned
(cache.rs:29-36). All existing `resolve_candidates*` signatures and the
`Result<StreamInfo, Vec<ResolveFailure>>` shape are unchanged.

### §6.2 Quality chain

`ResolveOptions` is NOT extended (its struct literals live in eleven provider
tests/examples, §0). New type instead:

```text
QualityChain::from_tiers(Vec<StreamQuality>) -> QualityChain   // dedup, drop Unknown, keep order
resolve_candidates_with_chain(registry, pipeline, candidates, chain, timeout)
    -> Result<(StreamInfo, QualityOutcome), Vec<ResolveFailure>>
QualityOutcome { requested: StreamQuality, served: StreamQuality, degraded: bool }
```

The chain walks tiers outer-loop / candidates inner-loop: the first tier that
yields any successful resolve wins. `requested` is the tier of the winning
attempt, `served` is `stream.quality` (models/stream.rs:92), and
`degraded = served != requested` (exact comparison; `Unknown` served counts
as degraded). This is the requested-vs-served record API the ask names.
Chain construction from a `Track`'s declared tiers:
`QualityChain::from_tiers(track.available_qualities)` with the preferred tier
first: the "音质选择与降级链" of tasks.md §1.2, made explicit instead of
provider-internal.

### §6.3 Structured fallback consumption

`StreamInfo::ordered_fallback_endpoints() -> Vec<FallbackEndpoint>`
(models/stream.rs, beside ordered_fallback_urls :160-176): legacy
`fallback_urls` become minimal endpoints (url only), structured
`fallback_endpoints` keep their per-URL headers/size/expiry/quality; trim,
drop empties, dedup keep-first, primary never included: the same ordering
rule §2.4 pinned for strings, now carrying metadata. This is the data face
the download engine slice (MODEL_ALIGNMENT_DESIGN.md §8 register) and hosts
need to drive candidate switching; M6 core performs **no network probing of
fallbacks**. Resolve returns the primary plus a structured, ordered,
metadata-carrying fallback list, and actual switching stays an engine/host
concern. Anything else would fake a success we cannot verify.

### §6.4 Provider order and negative-cache hookup

`order_candidates_by_priority(candidates, &[&str]) -> Vec<Track>`: stable
reorder, listed providers first in list order, unlisted keep relative order
after them. `MusicProvider::priority()` stays a descriptor hint
(provider/traits.rs:69-72); the registry does not sort (AUTH_REGISTRY_DESIGN
§5 posture); the explicit list is the caller's decision surface.

`ResolvePipeline` (resolve.rs:22-26) gains `negative: Option<&NegativeCache>`
(constructed sites updated additively). Behavior:

1. before a provider call: `negative.check(Stream{track.id, options.quality})`
   with a live entry means skip the provider, push a `ResolveFailure` carrying
   the cached category, continue (repeated identical failures stop re-hitting
   the provider: the gap named in the ask, resolve.rs:139-157 today only
   counts health);
2. on failure: `record` with the `Error`'s category (invalid-URL arm maps to
   `InvalidResponse`, timeout arm maps to `Timeout`; circuit-open is NOT
   recorded, the breaker already gates that path);
3. on success: `invalidate` the key (a stale negative must not outlive a
   success).

Session-expiry negatives: recorded under `SessionExpired`'s 30 s TTL and
cleared wholesale by `NegativeCache::clear_provider` when the host refreshes
the session (§2.3).

## §7 Lyrics layering (m6-lyrics-layering)

### §7.1 The two-layer problem (inventory-verified)

`LyricDocument` carries line-level `translations: BTreeMap<lang, text>` /
`romanization: Option<String>` (models/lyrics.rs:31-35) **and** document-level
`translation`/`romanization` raw payload strings (:40-49). The producers are
split: netease/qqmusic fill the document-level raw payloads and leave lines
bare (source_netease.rs:1039-1061, source_qqmusic.rs:895-901), while
`parse_document` fills line maps and leaves document slots `None`
(lyrics.rs:43-51, 75-81). Nothing aligns a document-level `tlyric`/`rlyric`
payload to lines by `start_ms`. Millisecond semantics were re-verified against
lyrics-core-0.5.1 `line_info.rs:18-35` (i32 ms) and the musid pass-through
`lyrics.rs:59-67` (`.max(0) as u64`): no conversion gap; `end_ms` stays
`Option` (models/lyrics.rs:27-29) and no timeline is ever fabricated. The
semantics are pinned by regression tests, not just by reading the dependency:
`parse_document_pins_i32_ms_word_timing_pass_through` (YRC word/line times
arrive as the payload's exact integers),
`parse_document_keeps_end_ms_optional_for_line_timed_payloads` (no fabricated
end), `parse_document_clamps_negative_ms_to_zero` (an LRC `offset` attribute
drives raw i32 times negative; the parse boundary clamps to 0), and
`parse_document_rejects_unsupported_payloads_with_invalid_input` (the
lyrics.rs:21-23 rule).

### §7.2 Normalized accessors (models, always compiled)

```text
LyricDocument::has_word_timing(&self) -> bool        // any line with non-empty words
LyricDocument::layered_lines(&self,
    translation: Option<&LyricDocument>,
    romanization:  Option<&LyricDocument>,
    tolerance_ms: u64) -> Vec<LayeredLyricLine>
LayeredLyricLine { start_ms, end_ms, text, words,
                   translations: BTreeMap<lang, text>,   // the full line map, nothing dropped
                   translation: Option<MergedTranslation{language, text}>,  // primary pick
                   romanization: Option<String>,
                   translation_source / romanization_source: Option<LyricLayer{Line|Document}> }
```

Alignment rule: for each original line, binary-search the nearest line start
in the layer document; accept within `tolerance_ms` (typical 500). **Line-level
data always wins** when both exist; `*_source` records where each piece came
from, so a host can tell aligned-document translation from producer-supplied
line translation. Nearest-wins on collisions (two layer lines near one
original line): documented lossy-nearest, never concatenated. The full
line-level map rides `LayeredLyricLine::translations` (a line with several
languages loses nothing); `translation` is the deterministic first pick in
language-key order.

Provenance survives the materialized path (review-hardened, both branches):
§7.3's `parse_document_layers` materializes document-layer data into the
line maps, and `layered_lines` reports the true origin on both:

- **translation** carries the `UNSPECIFIED_LANGUAGE` marker key written by
  `materialize_layers`; such an entry is reported as
  `LyricLayer::Document` (language `None`). A producer that itself
  supplies an `UNSPECIFIED_LANGUAGE`-keyed line translation would be
  reported as Document-origin; none does today (netease/qq fill document
  slots).
- **romanization** is a scalar with no key to carry a marker, so
  attribution is by content comparison: a line value identical to the
  nearest document-layer text within tolerance is exactly what
  `materialize_layers` writes and is reported as `LyricLayer::Document`;
  a differing value is producer-supplied (`Line`). Documented edge: a
  producer line romanization that coincides byte-for-byte with the
  aligned document text is reported as Document too (indistinguishable by
  content). Pinned by
  `romanization_provenance_survives_materialization_by_comparison`.

### §7.3 Materialized parse (lyrics facade, feature `lyrics`)

`parse_document_layers(original, translation: Option<&str>, romanization:
Option<&str>) -> Result<LyricDocument>` parses all three payloads through the
existing `parse_document` (unsupported format means `Error::InvalidInput`,
lyrics.rs:21-23 rule kept), then materializes the §7.2 merge INTO the line
maps while keeping the document-level raw slots verbatim. The returned
document is both serde-complete for hosts and accessor-complete for §7.2
consumers. Producers are untouched (netease still fills document slots; hosts
gain the merged view without changing any producer). A regression test pins:
LRC + LRC-shaped tlyric means lines gain the translation map; absent layers
means lines unchanged; word-timing flag flips only when words exist.

### §7.4 Format labels are syntax labels, not provenance

`LyricFormat::Krc` is a syntax-family label, not a wire provenance: decrypted
kugou KRC and kuwo lrcx decode to the same `[ms,dur]<rel,dur>` karaoke shape
(`LyricFormat` has no lx/lrcx variant), so kuwo labels its word documents
`Krc` too (source_kuwo.rs `lyric_document`). Hosts therefore treat the
format enum as a syntax hint only: karaoke availability is
`has_word_timing()` (§7.2, the 逐字 marker, "whatever the format claims"),
layer provenance is `translation_source`/`romanization_source`, and the
netease/qqmusic producers never fetch word payloads at all (`yv:0`
source_netease.rs:612, `qrc:0` source_qqmusic.rs:889), so their documents
stay line-timed regardless of labels. Pinned by
`format_label_is_not_the_word_timing_contract` and the kuwo
`lyric_document_word_branch_wins_and_unusable_lrcx_falls_back` fixture. The
slice's outer gate is the `producer` feature (`producer = ["lyrics"]`,
Cargo.toml following the lyrics-cache shape): the §7.2 accessors are
always-compiled models, §7.3 rides `lyrics`, and producer fill shapes stay
untouched per the §7.3 compatibility rule.

## §8 Lyrics pipeline (m6-lyrics-pipeline)

### §8.1 Feature gates

`lyrics-cache = ["lyrics"]` (new, follows the lyrics/lyrics-search layering,
Cargo.toml:40-41). The cache module compiles under `lyrics-cache` alone
(document domain, negative domain, the provider-direct cached face); the
**search-domain cache and the health hookup additionally need
`lyrics-search`** and are gated `cfg(all(lyrics-cache, lyrics-search))`.
`search_many`'s signature is unchanged.

### §8.2 LyricsCache

`src/lyrics_cache.rs`:

```text
LyricsCacheConfig { search_ttl = 10 min, document_ttl = 24 h, negative: NegativeTtlPolicy::standard(), capacity = 512 }
LyricsCache::new(config)
  search:    TtlCache<(TrackId, SearchMode), Vec<LyricSearchResult>>  (empty list = cached absence)
  documents: TtlCache<(provider, id), Option<LyricDocument>>           (None = cached absence)
  negative:  NegativeCache                                            (LyricsSearch{TrackId, mode, source} / LyricsDocument{provider,id})
```

Keys are the stable ids the inventory mandates (TrackId; `LyricSearchResult`
provider+id, models/lyrics.rs:53-61); never the title. The `SearchMode`
(`Full`/`Refine`) rides the key because the two modes legitimately return
different result sets (lyrics.rs:139-167 refinement semantics). **Absence
is symmetric across both positive domains** (review-hardened): an
all-sources-successful empty search is cached as an empty list for the
search TTL, exactly like a missing document is cached as `None` for the
document TTL; a track with no lyrics is not re-queried on every paint.
Bounded capacities on all three domains; expiry check-and-purge on access
(§2.3 rule).

### §8.3 Faces

- **Health keys are namespaced away from provider ids** (review-hardened):
  lyrics source ids collide with provider ids (`netease`, `kugou` verbatim;
  `qq` does not even match the `qqmusic` provider id), so lyrics health
  records under `lyrics_health_key(source) = "lyrics-source:{id}"`. A
  monitor shared between the lyrics and stream faces (the natural host
  setup) can never let a lyrics failure open a stream provider's breaker,
  and hosts query `lyrics_health_key(source.id())` for the lyrics-side
  circuit state. Successes are recorded per source (every healthy source
  can recover its namespaced breaker regardless of the others, exactly
  like the search aggregation records per successful page); cancelled jobs
  record nothing.
- `lyrics_for_track(registry, track, cache) -> Result<Option<LyricDocument>>`:
  provider-direct path (`registry.lyrics_provider(track.id.provider)`,
  registry.rs:531-533) with cache short-circuit, negative short-circuit
  (the hit rebuilds a **category-faithful M2 error** via the
  `cached_error` mapping, so `is_retryable()` and the health class survive
  the negative path instead of collapsing into the legacy `Provider`
  catch-all; `UnsupportedQuality` degrades because the quality tier is not
  stored), category-accurate negative recording on failure, invalidate on
  success, and explicit absence cached as `Some(None)` so a repeat ask
  answers `Ok(None)` consistently. Cross-source refusal stays the
  provider's own guarantee (source_netease.rs:605-609 precedent).
- `search_many_with_health(sources, track, full_search, timeout, health,
  cancel)` (gated `lyrics-search`): the existing per-source isolation plus
  per-source health records under the namespaced key, and a
  `SearchCancelToken` check before each source job. Legacy tuple return
  shape kept. `LyricsSourceFailure` carries the M2 category where one
  exists: `Timeout`/`Cancelled` are classified natively; searcher errors
  arrive as the legacy `Error::Provider` catch-all (the lyrics-helper
  dependency bypasses musid's classification, DEPENDENCY_AUDIT.md
  2026-11-20), so their category is `Provider` until a searcher
  replacement lands (§11).
- `search_many_cached(...)` (gated `lyrics-cache` + `lyrics-search`),
  orchestration in the injectable `search_cached_with`:
  per-source negatives split the source list first and every skip is
  reported as an error tuple (a cached list never silently narrows the
  candidate set); a positive entry answers immediately with the blocked
  skips attached; a full-freshness entry (empty results included, cached
  absence, §8.2) is stored ONLY when every source of the call
  participated, because two narrowing states cap the entry otherwise: a
  failed source caps it at its category TTL and a blocked source at its
  REMAINING negative window (`NegativeCache::remaining_ttl`), so the
  narrowed candidate set never outlives the state that caused it; a
  **cancelled run stores nothing**: the engine reports one `Cancelled`
  failure per source (a silent empty success would be cached as a fake
  absence), `Cancelled` is non-cacheable, and a `Cancelled`-only failure
  set yields a zero provisional window; live failures are recorded per
  source with their categories.
- **Out of reach, stated honestly**: per-page continuation for the lyrics
  searchers. `lyrics_helper::search_with_refinement` (lyrics.rs:146) exposes
  no paging surface, so no pagination can be added without replacing the
  searcher (the decision is recorded in §11 and DEPENDENCY_AUDIT.md). What IS
  added: cancellation and health per source.

## §9 Interface relations M1 / M2 / M3

| Layer | What M6 consumes | Where |
|---|---|---|
| M1 transport/retry | nothing new: aggregation stays at the trait seam; a future searcher replacement would ride `Transport`+`RetryPolicy`+`into_provider_error` (src/transport.rs:245-453) and is deferred (§11) | §11 |
| M2 errors | `Error::category()` feeds (a) the `HealthFailureClass` mapping, (b) `NegativeTtlPolicy` TTLs and the non-cacheable set, (c) `ProviderSearchError::category`, (d) resolve negative records; `is_retryable()` (error.rs:362-370) is the retryable-TTL partition | §2.3, §4, §5.4, §6.4 |
| M2 registry/auth | artwork is the **12th registry slot** with the same conflict/unregister/health-reset semantics; `SessionStore` account ids are the namespace source for §2.4 | §1.3, §2.4 |
| M3 models | `LyricDocument` extended with accessors only (no shape change); `StreamInfo` gains the structured fallback consumer beside `ordered_fallback_urls`; `TrackId` is the universal cache key; M3 §5.5's artwork deferral is executed | §1, §2, §6.3, §7 |
| M5 local | `read_cover`/`probe_dimensions`/`content_hash` are the local artwork face; no second local cache layer | §1.4 |

## §10 Test plan

| # | Test | Pins |
|---|---|---|
| R1 | `negative_cache_records_and_expires_per_category` + non-cacheable set no-op + `clear_provider` | §2.3 |
| R2 | `ttl_cache_expires_and_evicts_earliest` | §2.2 |
| R3 | `namespaced_entries_are_invisible_across_namespaces` + legacy unnamed API unchanged | §2.4 |
| R4 | `provider_ttl_overrides_default_and_caps_by_url_expiry` | §2.5 |
| R5 | `health_failure_classes_map_from_categories` (one row per M2 category, all 24, no duplicates) + auth-class does not advance the breaker from Closed + `half_open_auth_failure_releases_the_probe_instead_of_wedging` + probe new fields round trip + legacy probe JSON loads | §4 |
| R6 | matcher fixture matrix (10 cases) + `existing_exact_and_isrc_scores_are_unchanged` + `legacy_match_candidate_payloads_deserialize_with_defaults` (serde-default compat) | §3 |
| R7 | aggregation pagination respects has_more/max_pages; typed categories map per arm; cancel stops further pages; legacy tuple shape identical | §5 |
| R8 | quality chain prefers first feasible tier; outcome requested/served/degraded; provider order helper stable | §6.2 |
| R9 | `ordered_fallback_endpoints` ordering/dedup/metadata preserved (mirror of T3, models/stream.rs:243-288) | §6.3 |
| R10 | resolve negative cache: failure recorded, next call skips provider, success invalidates | §6.4 |
| R11 | layering: document tlyric aligns into lines within tolerance; line-level wins; has_word_timing; absent layers no-op; ms semantics pinned (i32 pass-through, no fabricated end, negative clamp, InvalidInput) | §7 |
| R12 | lyrics cache: document domain + negative + invalidate; search cache keyed by TrackId+mode (gated run) | §8 |
| R13 | artwork: cache key table (no title), missing-vs-absent semantics, LocalArtwork embedded/external/priority via a real temp file, registry twelve-slot test | §1 |
| R14 | no-default boundary: `cargo test --locked --no-default-features --lib` passes (count at or above the 69 recorded by the inventory pass) and each new feature compiles isolated | §0 |

Gates run by this slice (narrow commands only; the full matrix is the outer
script's job): R14's no-default lib run, per-feature targeted `--lib` runs for
`artwork`, `artwork-local`, `lyrics-cache`, `lyrics-cache+lyrics-search`,
`lyrics`, `lyrics-conversions`, plus `cargo fmt --all` at delivery.

## §11 Deliberately not done (deferred register)

| Item | Why | Owner |
|---|---|---|
| Network artwork implementations for the 11 registered providers | M3 §5.5 rule: a face without a live implementation must not be registered; each provider needs its own slice with fixtures | provider slices (m7+) |
| lyrics-helper searcher replacement (self-hosted LRCLIB/AMLL on M1 transport) | behavior-difference risk against `search_with_refinement` match_type semantics; requires `LyricSearchResult` fixture parity first (inventory risk note) | separate slice; decision and measured numbers recorded in the DEPENDENCY_AUDIT.md 2026-11-20 entry |
| lyrics search pagination | upstream searcher exposes no paging surface (§8.3) | blocked on upstream or the replacement above |
| Engine consumption of structured fallback endpoints (actual URL switching) | download-engine loop owns candidate ordering (MODEL_ALIGNMENT_DESIGN.md §8); M6 ships the data face only | download-engine slice |
| `FEATURE_DEPENDENCY_MATRIX.md` full re-baseline | only the three new feature rows are appended here; the full table re-audit is a docs-slice pass | docs slice |

## §12 Landed deltas over this spec

Implementation session 2026-11-20. Everything in §1-§8 landed as written
except the following, each with its reason:

| # | Delta | Reason |
|---|---|---|
| D1 | §1.2 `ArtworkKind` enum dropped | `ArtworkKey` already encodes the kind per variant; a separate enum duplicated the same four values with nothing to attach to |
| D2 | §0's `url:{url}` cache-key form has no `ArtworkKey` variant | no M6 consumer resolves artwork by a bare URL; URLs live inside `ArtworkVariant.url` and the `Local` face keys by path. Re-add a `Url(String)` variant only when a producer needs it |
| D3 | §1.2 local-face `bytes_sha256` stays `None` | computing it would re-read the image bytes on every resolution for no M6 consumer; the on-disk cache file is already content-addressed (`local_cover::embedded_cover_path`) |
| D4 | §2.4 `StreamCache::len()`/`clear()` operate on all namespaces | a caller unaware of namespaces must not leave stale entries behind a "clear"; legacy `get`/`insert` keep the empty-namespace behavior byte-for-byte, and the existing cache tests are unaffected either way |
| D5 | §8.2 `NegativeKey::LyricsSearch` gained a `provider` field | per-source negatives: one blocked lyrics source must not suppress the healthy ones (the per-source isolation the search face already guarantees) |
| D6 | §5.3 cancellation pinned stricter than drafted | a cancellation observed mid-run drops the whole gathered set of that provider (the caller cancelled; partial data is not returned). Pinned by `cancellation_drops_results_gathered_after_the_cancel_point` and `cancellation_before_the_first_page_yields_nothing` |
| D7 | §5.5 zero bucket clamps to 1 s | a zero bucket would divide by zero and merge every durationless pair |
| D8 | §7.3 `parse_document_layers` stores blank payloads as `None` and writes the document-level raw slots itself | `parse_document` builds a fresh document whose slots are `None`, so the verbatim raw payload has to be carried by the new function; blank payloads carry nothing and leave the slots `None` |
| D9 | §1.4 format sniffing is content-first with extension fallback | `image::with_guessed_format` falls back to the file extension, so an external `folder.jpg` with junk content reports `Jpeg` (extension-truthful). Pinned by the artwork-local test instead of a synthetic `Unknown` expectation |
| D10 | additive changes to existing test literals | `AggregationOptions` (search timeout test: `..Default::default()`), `ResolvePipeline` (resolve tests: `negative` field), `ProviderProbeResult` round trip (two new fields), registry `try_register_covers_all_eleven_slots_...` extended to twelve slots and renamed to `..._twelve_slots_...` (register + duplicate-check rows added, no assertion removed) |
| D11 | `ErrorCategory` gained `PartialOrd`/`Ord` | needed as the `BTreeMap` key of `NegativeTtlPolicy::per_category`; structural order only, documented as non-semantic |
| D12 | `ProviderCapabilities` gained the `artwork` bit | merged like every other capability (unlike `download`): a provider declaring it must implement the face, per §1.3 |
| D13 | review: half-open auth release | an AuthInvalid failure of the half-open probe transitions the state back to `Open { cooldown }`; the first draft's early return wedged `probe_in_flight:true` forever (a session expiring during the probe locked the provider until restart). Pinned by `half_open_auth_failure_releases_the_probe_instead_of_wedging` |
| D14 | review: lyrics health keys namespaced | `lyrics_health_key(source) = "lyrics-source:{id}"`; source ids collide with provider ids (`netease`/`kugou`, `qq` vs `qqmusic`), and a shared monitor would have let lyrics failures open stream breakers |
| D15 | review: per-source success accounting | `search_many_typed` records `record_success` per successful source; the first draft recorded success only when every source succeeded with zero results, so a lyrics failure could open a breaker no lyrics success could ever close |
| D16 | review: ArtworkCache expiry cap | the positive TTL is capped by `Artwork.expires_at_unix` and the fastest `ArtworkVariant::expires_at_unix`; a signed cover URL never outlives its validity (the StreamCache absolute-cap rule) |
| D17 | review: Local artwork keys canonicalized + lossless | `cache_key` canonicalizes the path (`..`/symlinks/relative spellings collapse) and hex-encodes the raw path bytes on unix; the lossy `to_string_lossy` key could collide two distinct non-UTF-8 paths and poison both cache domains |
| D18 | review: negative TTL asymmetry widened | the credential trio (SessionExpired, AuthenticationRequired, AuthenticationFailed) all 30 s; InvalidResponse/Protocol/ProviderInternal/Provider 60 s (transient contract states, not stable resource facts) |
| D19 | review: lyrics search absence symmetry + provisional partial entries | an all-successful empty search is cached as absence (symmetric with the document domain's `None`); a partial success is stored with TTL = the shortest failing source's negative window; blocked sources always surface as error tuples |
| D20 | review: negative hits rebuild category-faithful errors | `cached_error` maps the cached category back onto its M2 variant (`RateLimited` stays retryable, credential states stay credential states); only `UnsupportedQuality` (needs the unstored tier) degrades to `Provider` |
| D21 | review: provenance survives materialization | `layered_lines` reports a line entry under `UNSPECIFIED_LANGUAGE` as `LyricLayer::Document`; `LayeredLyricLine` gained the full `translations` map (multi-language lines lose nothing) |
| D22 | review: matcher ordering hardened | confidence desc tie-break added (normalized-equal titles can tie at 85 with one side penalized); take-first warning added to `rank_alternatives`. Round-2 correction: the first draft ALSO claimed a proven "penalized score < High floor" invariant, which is false (normalized-equal titles reach 85, near-superset casts 86+); the claim is withdrawn and replaced by the two true properties, see §3.4 |
| D23 | review: `Io` maps to Unknown | a local I/O fault is not provider unreachability and must not pollute the Unreachable counters |
| D24 | review: `NegativeKey::Search` removed | the track-search negative variant had no record/check call site (the aggregation face takes no cache collaborator yet); the design §2.2 row is removed with it |
| D25 | review: cancellation is aggregate-level | one token check after the jobs return drops the whole aggregate; per-provider-only clearing kept other providers' pre-cancel results while reporting "cancelled" |
| D26 | review: class matrix cleaned | exactly one row per `ErrorCategory` variant (24) with a distinctness assertion; the first draft had a duplicate `AuthenticationFailed` row and a stale "22 categories" count |
| D27 | review round 2: cancelled lyrics runs store nothing | the engine's cancel pre-check returns one `Cancelled` failure per source instead of a silent empty success, so the cache cannot store a cancelled query as a full-freshness absence; `Cancelled` is non-cacheable and yields a zero provisional window. Pinned by `cancelled_run_is_never_cached_as_absence` and `cancelled_search_reports_cancelled_failures_per_source` |
| D28 | review round 2: narrowed entries capped by blocked windows | a full-freshness entry is stored only when every source participated; blocked sources cap the entry at their REMAINING negative window via the new `NegativeCache::remaining_ttl` / `LyricsCache::search_negative_remaining`, so a narrowed set never outlives the state that caused it. Pinned by `narrowed_success_with_blocked_sources_is_provisional` |
| D29 | review round 2: romanization provenance by comparison | the scalar romanization layer cannot carry the UNSPECIFIED_LANGUAGE marker, so `layered_lines` attributes a line value identical to the nearest document-layer text as Document (the compare-based option the review sanctioned); documented edge: coinciding producer text reads Document too. Pinned by `romanization_provenance_survives_materialization_by_comparison` |
| D30 | review round 2: `search_many` really delegates | the legacy face had its own copy of the join loop while the typed engine's doc claimed delegation; the loop is deleted and `search_many` is a thin adapter over `search_many_typed(None, None)` (behavior-preserving: same per-source timeout, messages and sort) |
| D31 | review round 2: injective artwork id keys | Track/Album/Artist cache keys length-prefix the provider part, so provider/id splits containing the separator byte can never collide (`album` "a:b"+"c" vs "a"+"b:c"); keys are in-memory only, so no migration |
| D32 | deferred register (round 2 observation): resolve negative-hit message | `ResolveFailure.message` folds the category as Debug text; the face has no category field (pre-M6 shape), so typing it is a breaking change deferred to a resolve-face slice. `lyrics_for_track` already rebuilds category-faithful errors (P2-8) |
| D33 | slice gate feature: `producer` | the m6-lyrics-layering outer gate (`cargo test/clippy --features producer`) names this slice's face: the §7.2 accessors are always-compiled models and §7.3 rides `lyrics`, so `producer = ["lyrics"]` follows the `lyrics-cache = ["lyrics"]` shape (Cargo.toml); producer fill shapes stay untouched per the §7.3 compatibility rule. §7.4 pins the Krc syntax-family label convention (`format_label_is_not_the_word_timing_contract`) |
| D34 | deferred register (m6-lyrics-layering observation): docs gate fails pre-existing | `RUSTDOCFLAGS="-D warnings" cargo doc --locked --all-features --no-deps` errors on 15+ pre-existing private intra-doc links across M4/M5/M6 files (lyrics_cache.rs:324/469 `cached_error`/`search_cached_with`, lyrics_convert.rs:34/68/241 `lyric_lines_from_document`, local.rs:36, local_scan.rs:614/649, the four providers' `login_status` docs, `RetryPolicy::standard`); none were introduced by this slice, so the docs-hygiene fix is deferred to a cross-slice pass, per the minimal-modification rule |
| D35 | m6-lyrics-pipeline outer-gate literal feature: `gate` | the outer verification gate runs `cargo test/clippy --locked --features gate` by literal name and failed with "the package 'musid-dli' does not contain this feature: gate"; the fix adds the aggregate selector `gate = ["lyrics-cache", "lyrics-search"]` following the union-naming precedent (`cfg` names the tag-write union, `artwork-local` names the artwork pair, `producer` names the layering face). It gates no code itself, implies nothing beyond the two M6 lyrics features (both imply `lyrics`), and selects exactly the slice's face |
| D36 | m6-artwork-resolver outer-gate literal feature: `feature` | this slice's outer gate runs `cargo test/clippy --locked --features feature` by literal name (the D35 failure mode, verified live: before the selector, cargo rejected the command with "the package 'musid-dli' does not contain this feature: feature"); the fix adds the aggregate selector `feature = ["artwork-local"]`, which implies `artwork` + `local-cover` and so selects exactly this slice's face (artwork core, registry slot, `LocalArtwork`). It gates no code itself (`cfg(feature = "feature")` appears nowhere) and pulls no dependency; matrix row added in `FEATURE_DEPENDENCY_MATRIX.md` next to the `gate` row |
| D37 | m6-resolve-quality-fallback re-verification added four missing semantic pins | the landed §6 face had no test where the winning attempt's `served` tier differs from `requested` (both R8 chain tests pinned `degraded: false`), so the `degraded = served != requested` record, the `Unknown`-served-counts-as-degraded rule, resolve-level negative-entry expiry, and the per-quality scoping of `NegativeKey::Stream` were unpinned. Added in resolve.rs: `quality_chain_records_a_lower_served_tier_as_degraded` (requested High, served Standard, `degraded: true`), `an_unknown_served_tier_counts_as_degraded`, `an_expired_negative_entry_releases_the_provider` (150 ms category TTL; inside the window the repeat is negative-cached, after expiry the provider is re-hit), `a_negative_entry_for_one_quality_does_not_block_another` (a Lossless-scoped `unsupported quality` negative must not suppress the same track at High). No production code changed |
| D38 | m6-health-semantics re-verification added three call-site pins | the landed §4 face had no test joining the two ends the slice connects: a real provider failure at an aggregation call site and the per-class snapshot counters. Added in search.rs (`health_semantics_tests`): `provider_failures_fill_the_health_class_table_by_category` (an HTTP 429 lands in `rate_limited`, a wall-clock timeout in `unreachable`, both advancing `consecutive_failures`), `a_session_expired_failure_is_an_auth_signal_not_a_breaker_step` (threshold 1: a `SessionExpired` search failure fills `auth_invalid` while the circuit stays Closed and `try_acquire` keeps admitting, so `refresh_session` is never blocked); in resolve.rs: `resolve_failures_fill_the_health_class_table_by_category` (a `RateLimited` resolve failure reaches `rate_limited` through the resolve pipeline's `record_failure_with_category`). Monitor-level semantics (the 24-row class matrix, auth never tripping the breaker from Closed, the half-open auth release, probe serde defaults and cooldown expiry) were already pinned inside health.rs; the lyrics face rides the same namespaced monitor and its category path is pinned by `lyrics_health_keys_are_namespaced_away_from_provider_ids` plus the monitor-level tests. No production code changed |
| D39 | m6-lyrics-helper-audit: audit re-landed on this session's numbers, literal gate selector `"Cargo.toml"`, three audit pins | the recorded 78/+89 crate counts predate the current lockfile; re-measured live with a stated counting rule: `lyrics` 98 unique crates, `lyrics-cache` 98 (delta 0, tree diff empty), `lyrics-search` 216 (+118), conclusions and the keep-vs-replace decision re-ratified in DEPENDENCY_AUDIT.md 2026-11-20. The outer gate's literal `--features Cargo.toml` failed exactly like D35/D36 ("the package 'musid-dli' does not contain this feature: Cargo.toml", verified live before the fix), so the aggregate selector `"Cargo.toml" = ["gate"]` names the audited `lyrics`/`lyrics-cache`/`lyrics-search` stack, gates no code and pulls no dependency (cargo accepts the dotted literal name, verified in a scratch crate). Pins added: `cargo_lock_pins_the_audited_dual_reqwest_stack` (lyrics.rs, parses Cargo.lock and fails when the dual reqwest 0.12/0.13 + base64 0.22/0.23 majors, the lyrics-helper 0.5.x release or the searcher's reqwest 0.13 dep changes either way), `remaining_ttl_reports_live_windows_and_leaves_purging_to_check` (cache.rs, the D28 read is read-only: expired entries report None unpurged until `check`), `search_negative_remaining_passthrough_tracks_the_window` (lyrics_cache.rs, the D28 passthrough). No production signature changed |

> SUPERSEDED 2026-11-20 (M7 release hygiene): the literal-named aggregate selectors added by
> D33 (`producer`), D35 (`gate`), D36 (`feature`) and D39 (`"Cargo.toml"`) were REMOVED from
> Cargo.toml before the 0.2.0 publish: they gated no code and pulled no dependency, and their
> names were slice-verification artifacts, not user-facing capability switches. The outer
> gate feature parser was fixed the same day (only valid bare feature tokens survive), so the
> failure mode D35 worked around cannot recur. Slice faces select through `artwork-local`,
> `lyrics-cache`, `lyrics-search`, `cfg` or `--all-features`; the D33-D39 rows above remain as
> historical records of the landing decisions, the Cargo.toml comment block documents the
> removal, and the post-removal gate is recorded in the M7 log entry of IMPLEMENTATION_STATUS.md.


Verification state of this slice (all run in the implementation session,
narrow commands only; the full matrix is the outer script's job). Counts are
post-review (D13-D26 added the review tests):

| Gate | Command | Result |
|---|---|---|
| Core | `cargo test --locked --no-default-features` | 111 lib + 1 integration (qr_terminal_png) passed, 0 failed (pre-M6 69 all preserved) |
| Lyrics facade | `cargo test --locked --features lyrics --lib lyrics::` | 8 passed (models::lyrics 6 + lyrics:: 2) |
| Conversions | `cargo test --locked --features lyrics-conversions --lib lyrics_convert::` | 13 passed |
| Artwork core | `cargo test --locked --no-default-features --features artwork --lib artwork::` | 5 passed |
| Artwork local | `cargo test --locked --no-default-features --features artwork-local --lib artwork` | 10 passed (artwork 5 + registry slot 1 + artwork_local 4) |
| Lyrics cache | `cargo test --locked --no-default-features --features lyrics-cache --lib lyrics_cache::` | 4 passed |
| Lyrics cache + search | `cargo test --locked --no-default-features --features lyrics-cache,lyrics-search` (full run) | 122 lib + 1 integration passed, 0 failed |
| Clippy | `cargo clippy --locked --lib --tests -- -D warnings` on no-default, `artwork-local,lyrics-cache,lyrics-search`, `lyrics,lyrics-conversions,lyrics-cache` | all exit 0 |
| Format | `cargo fmt --all -- --check` | exit 0 |
| Deps | `cargo tree --locked --no-default-features --features lyrics-cache` | 78 unique crates, identical to `lyrics` (zero new dependencies) |

m6-lyrics-layering outer-gate re-run (D33 `producer` feature, 2026-11-20, all
numbers measured in the landing session):

| Gate | Command | Result |
|---|---|---|
| Slice gate | `cargo test --locked --features producer` | 212 lib + 1 integration (qr_terminal_png) + 1 doctest passed, 0 failed |
| Isolated gate | `cargo test --locked --no-default-features --features producer --lib` | 119 passed, 0 failed |
| Slice lint | `cargo clippy --locked --features producer --all-targets -- -D warnings` | exit 0 |
| Slice tests | `cargo test --locked --features producer --lib -- lyrics::` | 14 passed (models::lyrics 8 including `format_label_is_not_the_word_timing_contract`, lyrics:: 6 including the four §7.1 ms pins) |
| Format | `cargo fmt --all` then `cargo fmt --all -- --check` | exit 0 |
| Docs (observed, D34) | `RUSTDOCFLAGS="-D warnings" cargo doc --locked --all-features --no-deps` | fails on pre-existing cross-slice private intra-doc links, untouched by this slice |

m6-artwork-resolver outer-gate re-run (D36 `feature` aggregate selector,
2026-11-20, all numbers measured in the landing session; the pre-fix
rejection was reproduced live by temporarily removing the selector:
`cargo test --locked --features feature --no-run` errored with "the package
'musid-dli' does not contain this feature: feature", the D35 failure mode):

| Gate | Command | Result |
|---|---|---|
| Slice gate | `cargo test --locked --features feature` | 228 lib + 1 integration (qr_terminal_png) + 1 doctest passed, 0 failed |
| Slice lint | `cargo clippy --locked --features feature --all-targets -- -D warnings` | exit 0 |
| Compile check | `cargo test --locked --features feature --no-run` (selector restored) | exit 0 |
| Format | `cargo fmt --all` then `cargo fmt --all -- --check` | exit 0 |

m6-search-aggregation re-verification (2026-11-20, zero code delta this
session: the §5.1-§5.5 face with D6/D7/D24/D25 was already landed; this run
re-measured the gates. The slice face is core (search.rs, always compiled),
so the outer script's literal `feature` gate (the D36 selector, which pulls
default features + the artwork pair) exercises it unchanged; no selector
redefinition, per minimal modification):

| Gate | Command | Result |
|---|---|---|
| Slice gate | `cargo test --locked --features feature` | 228 lib + 1 integration (qr_terminal_png) + 1 doctest passed, 0 failed |
| Slice lint | `cargo clippy --locked --features feature --all-targets -- -D warnings` (re-linted after touching sources) | exit 0 |
| Narrow unit | `cargo test --locked --features feature --lib search::` | 6 passed, 0 failed |
| Core isolation | `cargo test --locked --no-default-features --lib search::` | 6 passed, 0 failed (107 lib tests in that profile) |
| Format | `cargo fmt --all` then `cargo fmt --all -- --check` | exit 0 |

m6-resolve-quality-fallback re-verification (2026-11-20, the §6.2-§6.4 face
was already landed in the working tree; this session re-measured the gates
and added the D37 test pins only, with zero production-code delta. The slice
face is core (resolve.rs, models/stream.rs accessor, cache.rs negative
domain), so the outer script's literal `feature` gate (the D36 selector)
exercises it unchanged):

| Gate | Command | Result |
|---|---|---|
| Slice gate | `cargo test --locked --features feature` | 233 lib + 1 integration (qr_terminal_png) + 1 doctest passed, 0 failed |
| Slice lint | `cargo clippy --locked --features feature --all-targets -- -D warnings` | exit 0 |
| Narrow unit | `cargo test --locked --features feature --lib resolve::` | 12 passed, 0 failed (8 landed + 4 D37 pins) |
| Data face | `cargo test --locked --features feature --lib models::stream` | 5 passed, 0 failed (incl. R9 `ordered_fallback_endpoints_mirror_string_ordering_with_metadata`) |
| Negative domain | `cargo test --locked --features feature --lib cache::` | 16 passed, 0 failed (R1 TTL semantics incl. `negative_cache_records_and_expires_per_category`) |
| Core isolation | `cargo test --locked --no-default-features --lib resolve::` | 12 passed, 0 failed |
| Format | `cargo fmt --all` then `cargo fmt --all -- --check` | exit 0 |

m6-health-semantics re-verification (2026-11-20, the §4.1-§4.3 face was
already landed in the working tree; this session re-measured the gates and
added the D38 call-site pins only, with zero production-code delta. The slice
face is core (health.rs, search.rs and resolve.rs failure sites, lyrics.rs
namespaced keys), so the outer script's literal `feature` gate (the D36
selector) exercises it unchanged):

| Gate | Command | Result |
|---|---|---|
| Slice gate | `cargo test --locked --features feature` | 236 lib + 1 integration (qr_terminal_png) + 1 doctest passed, 0 failed |
| Slice lint | `cargo clippy --locked --features feature --all-targets -- -D warnings` | exit 0 |
| Narrow unit | `cargo test --locked --features feature --lib health::` | 7 passed, 0 failed (R5 set: 24-row class matrix, counters fill/reset/legacy-Unknown, auth never trips Closed, half-open auth release, probe round trip incl. attempts/retry_after_ms, legacy probe JSON defaults) |
| Search-face pin | `cargo test --locked --features feature --lib search::` | 8 passed, 0 failed (incl. the two D38 pins) |
| Resolve-face pin | `cargo test --locked --features feature --lib resolve::` | 13 passed, 0 failed (12 landed incl. D37 + 1 D38 pin) |
| Core isolation | `cargo test --locked --no-default-features --lib health::` | 7 passed, 0 failed (114 lib tests in that profile) |
| Format | `cargo fmt --all` then `cargo fmt --all -- --check` | exit 0 |

m6-lyrics-helper-audit verification (2026-11-20, all numbers measured in the
landing session. The slice is an audit: production-face delta is the literal
`"Cargo.toml"` aggregate selector (D39) plus three test pins; no existing
signature, cache semantics or test changed. Dependency facts and the
keep-vs-replace decision are landed in DEPENDENCY_AUDIT.md 2026-11-20):

| Gate | Command | Result |
|---|---|---|
| Literal slice gate | `cargo test --locked --features Cargo.toml` | 234 lib + 1 integration (qr_terminal_png) + 1 doctest passed, 0 failed (pre-fix the command errored "the package 'musid-dli' does not contain this feature: Cargo.toml", reproduced live) |
| Slice lint | `cargo clippy --locked --features Cargo.toml --all-targets -- -D warnings` | exit 0 |
| Core isolation | `cargo test --locked --no-default-features` | 122 lib + 1 integration passed, 0 failed |
| Gate union | `cargo test --locked --no-default-features --features gate` | 141 lib + 1 integration passed, 0 failed |
| lyrics-cache face | `cargo test --locked --no-default-features --features lyrics-cache --lib search_negative_remaining_passthrough` | 1 passed, 0 failed (132 filtered) |
| Cache-domain pin | `cargo test --locked --no-default-features --features gate --lib remaining_ttl_reports` | 1 passed, 0 failed |
| Lock pin | `cargo test --locked --no-default-features --features gate --lib cargo_lock_pins` | 1 passed, 0 failed |
| Core lint | `cargo clippy --locked --no-default-features --lib --tests -- -D warnings` | exit 0 |
| Deps: `lyrics` | `cargo tree --locked --no-default-features --features lyrics -e normal --prefix none` (dedup rule in DEPENDENCY_AUDIT.md) | 98 unique crates |
| Deps: `lyrics-cache` | same command, `--features lyrics-cache` | 98 unique crates, `diff` vs the `lyrics` tree empty (delta 0) |
| Deps: `lyrics-search` | same command, `--features lyrics-search` | 216 unique crates, +118 over `lyrics`; reqwest 0.12.28 + 0.13.5 and base64 0.22.1 + 0.23.1 dual majors confirmed against Cargo.lock:2180-2181/2218-2219 and 128-135; `source-netease,lyrics-search` graph shows both reqwest majors live |
| Build cost | `CARGO_TARGET_DIR=<fresh> cargo build --locked --offline --no-default-features --features lyrics-search` | 35.05 s, exit 0, 919 MB target dir, aws-lc-sys 0.47.0 native build compiles on Linux without extra setup |
| No-OpenSSL check | `grep name = "native-tls"/"openssl-sys" Cargo.lock` | no hits (only openssl-probe, the rustls-platform-verifier cert-discovery face) |
| Format | `cargo fmt --all` then `cargo fmt --all -- --check` | exit 0 |
| Voicefox upstream | `git -C /media/emo/code/voicefox status --porcelain` | empty (untouched, read-only reference) |

Second review round consolidated gate battery (2026-11-20). The fix session
re-measured this battery after the D27-D31 fixes; the documentation pass then
re-ran the identical commands on the delivered tree and records both values,
so the table states what reproduces. Four reported counts predate the late
D33-D39 test additions and differ from the re-measured tree; the substance
(every listed run all-pass) holds in both columns:

| Gate | Command | Reported (fix session) | Re-measured (documentation pass) |
|---|---|---|---|
| Core | `cargo test --locked --no-default-features` | 112 lib + 1 integration (qr_terminal_png) all passed | 122 lib + 1 integration, 0 failed (the D39 block above records the same 122) |
| Lyrics facade | `cargo test --locked --features lyrics --lib lyrics::` | 9 passed | 14 passed (the D33 composition: models::lyrics 8 + lyrics:: 6) |
| Conversions | `cargo test --locked --features lyrics-conversions --lib lyrics_convert::` | 13 passed | 13 passed |
| Artwork core | `cargo test --locked --no-default-features --features artwork --lib artwork::` | 5 passed | 5 passed |
| Artwork local, `artwork_local::` | `cargo test --locked --no-default-features --features artwork-local --lib artwork_local::` | 4 passed | 4 passed |
| Artwork local, `local_cover::` | `cargo test --locked --no-default-features --features artwork-local --lib local_cover::` | 10 passed | 10 passed |
| Artwork local combined | same feature set, `--lib artwork` (both modules plus the registry slot test) | 10 passed | 10 passed |
| Lyrics cache | `cargo test --locked --no-default-features --features lyrics-cache --lib lyrics_cache::` | 4 passed | 5 passed (4 landed plus the D39 pin `search_negative_remaining_passthrough_tracks_the_window`) |
| Gate union full | `cargo test --locked --no-default-features --features lyrics-cache,lyrics-search` | 125 lib + 1 integration all passed | 141 lib + 1 integration, 0 failed (the D39 gate-union block above records the same 141) |
| Lint x3 | `cargo clippy --locked --lib --tests -- -D warnings` on no-default, `lyrics-cache,lyrics-search`, `artwork-local,lyrics-cache,lyrics-search` | all exit 0 | all exit 0 |
| Format | `cargo fmt --all -- --check` | exit 0 | exit 0 (a no-default test re-run after formatting reconfirmed the 122 count) |
| Deps | `cargo tree` for `lyrics-cache` vs `lyrics` | equal, 78 unique crates (older counting rule; 98 by the D39 rule) | 99/99 unique normal-dependency crates, tree diff empty (zero delta under every counting rule) |

The full --all-features/CI matrix was not run in the fix session and was not
run in this documentation pass either; it stays the outer script's job. The
second review round's per-item evidence (the D27 cancellation pins
`cancelled_search_reports_cancelled_failures_per_source` at src/lyrics.rs:614
and `cancelled_run_is_never_cached_as_absence` at src/lyrics_cache.rs:862, the
D28 pin `narrowed_success_with_blocked_sources_is_provisional` at
src/lyrics_cache.rs:894, the D22 test rename `penalized_examples_stay_medium`
at src/matcher.rs:520, the D29 pin
`romanization_provenance_survives_materialization_by_comparison` in
src/lyrics.rs and the D30 delegation `search_many_typed(None, None)` at
src/lyrics.rs:253) is verified present in the delivered tree by this pass.
