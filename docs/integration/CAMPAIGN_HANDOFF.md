# musid-dli + VoiceFox 整合战役交接文档

Updated: 2026-11-21 (mid-campaign handoff, written while V2 is in flight)

This document is the operational handoff for continuing the integration campaign in a NEW session. The auto-loaded memory files (`musid-dli-integration-campaign`, `musid-dli-progress`, `musid-dli-workflow`, `voicefox-runtime-config`, `music-provider-protocol-gotchas`) carry the durable facts; this document carries the campaign state and the step-by-step continuation procedure.

## 1. What is already done (all evidence recorded in the linked docs)

| Phase | State | Evidence |
|---|---|---|
| M0 baseline audit | DONE | docs/integration/ (six audit docs), gate table in BASELINE_AUDIT.md |
| M1 transport | DONE | src/transport.rs (injectable Transport + profiles + centralized retry), 10/10 provider migrations, TRANSPORT_DESIGN.md |
| M2 error/auth/registry | DONE | Error category() + seven-state AuthState + multi-account sessions + Registry queries (AUTH_REGISTRY_DESIGN.md), 12/12 slices |
| M3 model alignment | DONE | StreamInfo semantics (advisory size, structured fallback endpoints, checksum, Range/DASH) + conversions + serde compat (MODEL_ALIGNMENT_DESIGN.md), 5/5 slices |
| M4 provider parity | DONE | Qianqian + JOOX revived from live re-probes and IMPLEMENTED (src/source_qianqian.rs, src/source_joox.rs); 11 parity gaps fixed incl. all M3 dead-field producers; matrix re-anchored; 11/11 slices |
| M5 local library | DONE | src/local_scan.rs + tag_write.rs + local_cover.rs + local_watch.rs (10/10 slices); features local-cover / local-watch |
| M6 capability faces | DONE | src/artwork.rs + artwork_local.rs + lyrics_cache.rs + explainable matcher + cache domains + health semantics (9/9 slices; RESOURCE_DESIGN.md) |
| M7 release | DONE | feature namespace cleaned (four literal gate selectors removed, superseding notes in RESOURCE_DESIGN.md §12 and FEATURE_DEPENDENCY_MATRIX.md), v0.2.0 bumped + CHANGELOG.md, commit 6b4e09b + 61d121b + docs commit pushed, CI run 44638096513 ALL GREEN, crates.io v0.2.0 published (immutable), GitHub release https://github.com/emoeem/musid-dli/releases/tag/v0.2.0 |
| V0 voicefox adapter | DONE | voicefox docs/MUSID_DLI_ADAPTER.md (45-pair mapping spec), source/src/adapter_musid.rs + 29 fixture tests, feature musid-dli-adapter (musid-dli 0.2.0 from crates.io, dev-dep at V0); voicefox CI run 44760036549 SUCCESS |
| V1 dual-stack | DONE | musid-dli promoted to optional real dependency; opt-in dual-stack routing on search/track/stream-url/lyrics faces (shadow-by-default + replacement mode + fallback counters); second review F1-F5 fixed (stream probe backgrounded, cover_cache flaky root-fixed); voicefox CI run 44858909406 SUCCESS; default tree has zero musid-dli |
| V2 per-source migration | IN FLIGHT | run dwfrun-a0d795db (see section 3) |

Test counts at last full gate: musid-dli cargo test --locked --all-features 813+1+1, no-default 122+1; voicefox lx-source adapter lib 258+2 ignored, default 218+2.

## 2. Campaign-level decisions already made (do not re-derive)

- Master plan: /media/emo/code/MUSID_DLI_VOICEFOX_AUTONOMOUS_IMPLEMENTATION_PLAN.md (M0-M7 done; V0-V5, F0 remain; phase order strict).
- Batch discipline: docs/tasks.md sections 15/18 (one coherent slice per batch, no fake results, no dash characters in any .md, git constraints).
- Full-autonomy grant + push/release/publish authorization + no-confirmation directive: see memory `musid-dli-integration-campaign` (nothing needs user confirmation; QR scans and Migu SMS stay human).
- V-phase dependency policy: musid-dli 0.2.0 from crates.io (never absolute paths; relative path deps break CI clean checkouts).
- V2 migration order (from the V0/V1 design docs): jamendo/5sing/apple first, then bilibili/migu, then kuwo/kugou/qq, then netease, then qianqian/joox. soda and youtube-music do NOT migrate (soda has no musid-dli source, ytm rides RustyPipe).
- Deletion rule (V4): old voicefox implementations are deleted ONLY after that source passes parity; qianqian/joox are implemented in musid-dli since M4 so they are migratable now; soda stays voicefox-only.
- Optimization campaign after F0: review both projects, build a shortcomings ledger, then burn the remaining token budget on audit-backed improvements, dual-track parallel (two repos), everything through fmt/clippy/tests/CI.

## 3. What is running right now + how to handle it

V2 run `dwfrun-a0d795db-4...` (full id dwfrun-a0d795db-d549-4964-83e3-4f808b3870f2) is IN FLIGHT at handoff time, owned by the session that wrote this document. State at handoff: inventory/design/reviews settled (cache), jamendo + batch-1 (fivesing/apple/bilibili/migu/kuwo/kugou, 35.8M-token deep session) settled, batch-2 (qqmusic/netease/qianqian/joox + reg-soda/reg-ytm registrations) was executing a long ask (3h22m) in the per-source migration phase; docs phase and voicefox full gate are ahead.

Continuation procedure in a NEW session:

1. Open the new session in /media/emo/code/musid-dli (the memory index auto-loads; read this file first).
2. Check the run: call GetWorkflowRun with the id above (or ListWorkflowRuns for this project).
3. Three cases:
   - status completed: read the result; proceed to section 4 (V2 closeout).
   - status interrupted (this session was closed mid-run): call ResumeWorkflowRun with the same id; finished steps replay from the journal, the interrupted ask re-dispatches (workspace files already written persist, so it completes fast). The stop reason for a session exit is `interrupted`, which is the normal resume path.
   - status stopped (provider quota, code 1005): wait for the quota reset (daily 09:00) then ResumeWorkflowRun; nothing is lost.
   - status stopped (user): the user stopped it on purpose; ask what they want before resuming.
4. Escalations pending on the run (GetWorkflowRun lists them) must be answered with ResolveWorkflowQuestion before the parked subagent continues.

## 4. V2 closeout (after the run completes)

1. Read the run result; if any slice failed, fix inline in /media/emo/code/voicefox (the failure text carries the gate output).
2. Re-verify the four gates by hand: `cargo fmt --all -- --check`, `cargo clippy -p lx-source --features musid-dli-adapter --all-targets -- -D warnings`, `cargo test -p lx-source --features musid-dli-adapter --lib`, `cargo test -p lx-source --lib --release` (all from /media/emo/code/voicefox; expect adapter lib 258+/0/2 and default 218+/0/2, growing with V2 parity tests).
3. Commit everything with the repo's gitmoji convention and push with the TLS retry loop: `for i in $(seq 1 8); do out=$(git push origin main 2>&1); if echo "$out" | grep -q "main -> main"; then echo PUSH_OK; break; fi; sleep 20; done` (GnuTLS flakes are constant on this network; retries always clear them).
4. Watch voicefox CI: `gh run list -R emoeem/voicefox --limit 1 --json databaseId` then a retry-polling loop on `gh run view <id> -R emoeem/voicefox --json status,conclusion` (see section 6 for the loop shape). Green closes V2.

## 5. Remaining phases after V2 (run inline, NOT as workflows; the user chose inline fast mode for speed)

- V3 (local/lyrics/artwork integration): voicefox consumes musid-dli local library + lyrics cache + artwork faces per docs/integration/LOCAL_LIBRARY_PARITY.md and RESOURCE_DESIGN.md; opt-in feature musid-dli-adapter already covers the dependency; add integration tests; SourceManager policies stay untouched.
- V4 (delete redundant implementations): per source, delete the old voicefox provider implementation ONLY after that source's parity is green and the user-facing faces are covered; qianqian/joox/soda keep voicefox paths where musid-dli lacks coverage (soda entirely); every deletion is one commit with the parity evidence in the message.
- V5 (voicefox CI): add the musid-dli-adapter feature to voicefox's existing workflow jobs (test with the feature), add rust-cache if missing, verify all-platform green; also fix the MINGW64 deprecation warning annotation if trivial.
- F0 (final acceptance): write the acceptance report against the master plan section 19 checklist with real evidence links (CI run URLs, release URLs, gate outputs); honestly list the human-required items (four QR scans, Migu SMS, kuwo VIP lossless, YT live).
- Optimization campaign (post-F0): review both projects end to end, build the shortcomings ledger, then burn the remaining token budget on audit-backed improvements; dual-track parallel workflows (one per repo) are the throughput play; keep main green on both repos with regular pushes.

## 6. Operational knowledge (learned the hard way, keep using)

- TLS/GnuTLS push flakes on this network: ALWAYS wrap git push / gh calls in retry loops (8 attempts, 20-30s sleeps); they always clear.
- cargo config now caps jobs=4 and slims test/dev debuginfo to line-tables-only (~/.cargo/config.toml): this fixed the SIGKILL/OOM loop that ate hours of V2 (parallel rustc test compilations with full debuginfo + concurrent lld, each 700-800MB). Do not raise jobs back; if a build still OOMs, drop to jobs=2.
- Runner toolchain is newer than local rustc 1.99: runner-only clippy lints surface as CI failures (needless_bool case); fetch with `gh run view <id> --log-failed`, strip ANSI with `sed 's/\x1b\[[0-9;]*m//g'`, fix, re-push.
- rustdoc -D warnings: doc links on public items to private items are errors; use plain backtick spans (17 sites fixed once; the pattern recurs with every new module).
- Feature-gate parsing in phase scripts: only the first valid bare-name token survives (full-width brackets, "+" connectors and prose annotations were all seen); the M5 s3 precedent added a documented union feature named `cfg` instead of parser hacks when a union is genuinely needed.
- Quota walls: the run stops with stop-reason provider code 1005; the runtime auto-resumes after the 09:00 reset; never rebuild, never lose work.
- git add -A is safe in both repos (.zcode is ignored; musid-dli .gitignore covers target/, *_qr.png, .env; voicefox tree was clean before each V-phase commit); verify with git status before every commit anyway.
- voicefox commits follow the repo's gitmoji + short description convention; musid-dli uses plain conventional style. Never force, never rewrite history, never stage user files outside the campaign set.
- The workflow escalation mechanism works: subagents honestly escalate script bugs (three so far: cfg() in --features, annotated feature fields, literal "+" tokens); fix the script and AmendWorkflow (the escalation sits at the cache boundary, so the cost is one re-run ask).
- voicefox CI has no clippy job today; the double_must_use exception in core/src/lib.rs documents the async-trait macro expansion issue; V5 should add a clippy job rather than widening the allow.

## 7. File map

- musid-dli: docs/IMPLEMENTATION_STATUS.md (authoritative status log, 161+ lines), CHANGELOG.md, docs/integration/*.md (audit + designs), tests/fixtures/* (per-provider, READMEs carry provenance), src/*.rs (11 providers + transport + download + local stack + artwork + lyrics cache).
- voicefox: docs/MUSID_DLI_ADAPTER.md (adapter spec + V1/V2 status), source/src/adapter_musid.rs, source/tests/fixtures/musid/, core/src/lib.rs (double_must_use note).
- Workflows: draft scripts under musid-dli/.zcode/workflow-drafts/ (M-series and voicefox V-series); run journals are server-side, ids in this document.
- Master plan: /media/emo/code/MUSID_DLI_VOICEFOX_AUTONOMOUS_IMPLEMENTATION_PLAN.md (sections 12-14 cover V0-V5, section 19 the final checklist).
