# Changelog

All notable changes to musid-dli are documented here. Versions follow the crate's Cargo.toml.

## 0.2.0 (2026-11-20)

Second release: the full integration campaign (M0-M6) plus the remaining tasks.md schedule. The crate grows from a NetEase-only adapter to an 11-provider aggregation library with a unified network/error/session core, a download engine and a local-library stack.

### Added: online providers (11 total, every one with offline fixture tests)

- QQ Music (zzc signing, vkey ladder, playlists, albums, charts, link parsing, QQ + WeChat scan login), Kugou (KRC word lyrics, signed v5 streams with legacy v3 fallback, QR login, playlists/charts/albums/artists fallback), Kuwo (yeelion lyrics incl. word-level lrcx, web-signed lossless, playlists/albums/charts/links/covers), Migu (TEA-decrypted MRC, signed search, H5 strategy streams, playlists/charts/albums/links), Bilibili (WBI-signed search and DASH, reference expansion, QR login, favorites, charts), Jamendo, Apple Music (preview-only by contract), 5sing, Qianqian and JOOX (both revived 2026-11-19 after live re-probes found their endpoints answering again).
- Optional YouTube Music provider (RustyPipe) and the LX Music JS bridge (JSON-lines envelope, in-tree Node engine, provider wrappers; host wires user scripts).

### Added: infrastructure

- Unified transport layer (src/transport.rs): injectable transport seam, per-provider client profiles, centralized retry policy reusing the pure classifier; 10 provider migrations verified.
- Unified error semantics: category() classification, structured metadata (provider, retryability, user hint), no credentials in errors; all providers categorized.
- Authentication: seven-state AuthState, provider+account session isolation, session file atomic writes/corruption recovery/redaction; Registry capability query surface with multi-instance keys.
- Download engine core (src/download.rs, feature download): Range probing, single/multipart/strict-bounded chunked fetch, per-piece retry, candidate CDN fallback with NetEase node rewrite, atomic .part staging, size/MD5 integrity, tag/lyric finish.
- Local library: streaming scanner with failure ledger, CUE containers as scan products, incremental scan with fingerprints and directory signatures, duplicate/missing/corrupt diagnostics, safe tag writing (temp file + atomic replace), embedded + sidecar lyrics, cover extraction with validated cache (feature local-cover), watcher event abstraction (feature local-watch, no runtime binding).

### Added: capability faces

- Artwork resolver trait + LocalArtwork (feature artwork / artwork-local).
- Lyrics cache with positive/negative/expiry domains (feature lyrics-cache), explainable matcher scoring with version-difference matrix, search pagination + cancellation handles + per-provider isolation, health semantics (unreachable/rate-limited/auth-expired/maintenance/unknown), StreamInfo semantics (advisory size, structured fallback endpoints with per-URL headers/size/expiry, checksum algorithm, Range/DASH consumption decisions) and the StreamInfo-to-DownloadRequest adapter.

### Changed

- Error/category and auth-state wiring across every provider (vip/region/copyright/session-expired map to explicit categories).
- Models carry serde-backward-compatible extensions; round-trip and old-JSON compatibility tests pin the shapes.

### Infrastructure

- GitHub Actions CI (nine jobs: fmt, clippy all-features -D warnings, default/all/no-default tests, per-feature isolated checks enumerated from cargo metadata, MSRV 1.89, tri-platform builds with NASM on Windows, docs -D warnings, dependency integrity, gate).
- Every capability ships with offline fixture tests (reconstructed or captured, provenance documented per tests/fixtures/README); live smoke examples per provider stay opt-in local tools.

### Deferred (recorded, with reasons)

- Soda: CENC-encrypted containers conflict with the StreamInfo-only contract until a host-side decryption contract exists.
- QRC 3DES (QQ word lyrics): no verifiable key reference.
- Kuwo/Migu artist endpoints: none exist upstream.
- Live verification of QR logins and the lossless web tier: human-required (docs/HUMAN_TEST_CHECKLIST.md).
