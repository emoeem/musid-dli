# musid-dli + VoiceFox 整合战役交接文档

Updated: 2026-11-22 (campaign closeout — M0-M7 + V0-V5 + F0 all landed)

**战役状态：已完成到 F0。** 本文档不再是「进行中」的续跑程序，而是收官
存档 + 下一战役（优化轮）的入口。验收证据：`/media/emo/code/FINAL_ACCEPTANCE_REPORT.md`。

## 1. 最终状态（全部有 CI run 或本地门禁证据）

| 阶段 | 状态 | 证据锚点 |
|---|---|---|
| M0-M7 musid-dli | DONE, v0.2.0 发布 | CI 44638096513/44639207004/44912919271 全绿；crates.io + GitHub release v0.2.0 |
| V0 adapter / V1 dual-stack | DONE | voicefox CI 44760036549 / 44858909406；MUSID_DLI_ADAPTER.md §1-§11 |
| V2 逐源迁移（11 源数据面） | DONE | voicefox CI 44914042424；§12 切片 1-11 |
| V3 本地/歌词缓存/封面 | DONE | CI 44929545640；§13.1/13.2/13.3 + LOCAL_LIBRARY_PARITY.md 对账 |
| V4 legacy 删除 | DONE（2/12 源） | joox + qianqian 全删升主实现；其余 10 源缺口账本 §14.0；CI 44932665600（旧门拦截，已修）/ 44932996496 / 44933775046 |
| V5 CI 强化 | DONE | rust-cache 五 job + adapter clippy 步 + MINGW64 抑制（§14.2/§15） |
| F0 验收 | DONE | FINAL_ACCEPTANCE_REPORT.md（§19 清单逐项证据 + 诚实披露 + 人肉项 + 后续建议） |

## 2. 已知缺口（如实，下一战役的输入）

1. 9 个 voicefox legacy 源保留：musid-dli 0.2.0 缺集合端点（fivesing
   playlist_tracks / jamendo 容器面为 Unsupported 桩，已验证）与登录面
   （wy/kg/tx/bili QR、kg login.rs、tx 个人歌单、bili SESSDATA）；kw/mg
   集合 trait impl 齐但逐面真实性未核验。
2. Transport 注入缺口（§4）：voicefox 代理/超时不到 musid provider——
   joox/qianqian 升主实现后对其全面生效。
3. parity 计数对 joox/qianqian 静默（无对账对象，语义终结非缺陷）。
4. MINGW64 → UCRT64 迁移延后（libmpv ABI 重验）。
5. 歌词缓存为内存 TtlCache（重启即失），可选 disk 化。

## 3. 下一战役：优化轮（交接文档既定）

- 老流程照旧：review 两仓 → shortcomings ledger → 烧剩余 token 预算做
  audit-backed 改进 → 双轨并行（每仓一个 workstream）→ 一切过
  fmt/clippy/tests/CI → 主干常绿常推。
- 候选输入：本文件 §2 的缺口清单、FINAL_ACCEPTANCE_REPORT.md §6 建议、
  voicefox/docs/MUSID_DLI_ADAPTER.md §14.0 账本。

## 4. 操作知识（继续有效）

- TLS/GnuTLS push/gh 抖动：一律 8 次重试循环（20-30s sleep），必过。
- cargo jobs=4 + line-tables-only（~/.cargo/config.toml）防 OOM；勿调回。
- runner clippy 比本地新：CI 红 fetch `gh run view --log-failed` 去 ANSI
  修；doc list 续行缩进（doc_lazy_continuation）与步骤名裸冒号（YAML）
  两个坑都踩过并已记录。
- voicefox gitmoji 惯例；musid-dli conventional；永不 force。
- git add -A 安全（两仓 ignore 已覆盖），每次 commit 前仍核 status。
- feature-gate 解析：只认首个合法裸名 token；union 用命名 feature
  （cfg / artwork-local 先例）。
- quota 1005 → 09:00 自动续，工作不丢。
- voicefox lx-source default 现为 [musid-dli-adapter-joox,
  musid-dli-adapter-qianqian]（V4 flip）；零 musid 边界 =
  --no-default-features，tree 门三段锁死（ci.yml musid-stack job）。
