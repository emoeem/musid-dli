# musid-dli

[![CI](https://github.com/emoeem/musid-dli/actions/workflows/ci.yml/badge.svg)](https://github.com/emoeem/musid-dli/actions/workflows/ci.yml)

[English](README.md) | 简体中文

独立的 Rust 音乐元数据与多平台 Provider 聚合库。

musid-dli 不是音频播放器或解码器——它向宿主应用返回流元数据（URL、请求头、音质、有效期、备用 CDN），播放由宿主完成（VoiceFox 使用 lx-player + libmpv2）。

## 能力边界

- **在线 Provider**：搜索、流地址解析、歌词、歌单、专辑、歌手、排行榜、链接直解、登录认证。
- **本地音乐库**：增量目录扫描（逐文件指纹 + 目录签名，含无递归监听器宿主的显式 force 契约）、流式逐条输出（可取消/进度/并发上限）、失败台账、重复/缺失/损坏诊断、元数据归一（缺标签回填）、歌词读取（内嵌 + 外挂 .lrc）、封面读取（带校验缓存，可选 `local-cover` 特性）、原子标签写入、文件变更事件流（可选 `local-watch` 特性）；与在线 Provider 相互独立。
- **格式处理器**：CUE 分轨（多轨元数据切分、GB18030/宽松 sheet 回退）与 NCM 容器（独立 `ncm` 特性，上游依赖单独审计）。
- **下载**：独立可选服务；VoiceFox 侧下载器暂保留至对齐。
- **播放/DSP/输出**：不在范围内。

## 已落地能力

### 基础设施

- 统一 Transport 层（可注入 `Transport` + `ClientProfile` 配置 + 集中重试退避 + `MockTransport` 离线测试），0.2.1 起对外公开，宿主可注入自己的代理/超时预算或全自定义 transport。
- 统一错误体系（稳定 `ErrorCategory` + 无凭据的结构化元数据）、多账号会话（provider + account 双键、七态认证状态、导入/导出/刷新/吊销、损坏文件隔离）、Provider 注册表（能力查询、重复注册冲突策略、健康快照）。
- 资源能力：封面解析器 trait + 本地封面实现（`artwork-local`）、域拆分缓存（正/负/过期/会话，全部有界）、可解释匹配打分、五级 Provider 健康语义、聚合搜索（分页续传/取消/逐源类型化错误）、resolve 质量链（实际音质回落记录）、歌词分层（原文/翻译/罗马音/逐字/纯文本）+ 歌词缓存（负缓存 + 过期窗口）。

### 在线 Provider（11 个原生 + 2 个特殊）

| Provider | 主要能力 | 登录 |
|---|---|---|
| 网易云 `netease` | 搜索、音质回落流解析、歌词、歌单/专辑/歌手/榜单、链接直解 | Cookie + 扫码 |
| QQ 音乐 `qqmusic` | zzc 签名歌词、vkey 阶梯流解析、歌单/专辑/榜单、链接解析 | QQ/微信扫码（可播流需 Cookie） |
| 酷狗 `kugou` | KRC 逐字歌词解密、登录态签名 v5 流、歌单/榜单/专辑/歌手/链接 | 扫码 |
| 酷我 `kuwo` | yeelion-XOR 歌词（含逐字 lrcx）、移动流 + Web 签名无损、歌单/榜单/专辑/链接/封面 | 未接入 |
| 咪咕 `migu` | TEA 解密 MRC 逐字歌词、匿名流解析（服务端尺寸修正）、歌单/榜单/专辑 | 未接入 |
| 哔哩哔哩 `bilibili` | WBI 签名搜索与 DASH 音频、收藏、四榜三端点榜单 | 扫码 |
| 千千 `qianqian` | tracklink 音质阶梯（免费曲匿名完整文件、VIP 曲匿名 NotFound）、歌单分类/列表/详情、专辑、链接解析 | 未接入（上游无认证位） |
| JOOX `joox` | 加密 song-id 流地址、JSONP LRC 歌词、歌单搜索/容器、链接解析 | 未接入（上游无认证面） |
| Jamendo | CC 目录、x-jam-call SHA1 签名、内嵌流地址（无损优先） | 无 |
| Apple Music | JWT 引导、30 秒试听流（完整曲目为 DRM）、纯文本歌词、容器 | 无 |
| 5sing | 三档流地址 + 备份、LRC 歌词、歌单元数据 | 无 |
| YouTube Music（可选） | RustyPipe 搜索与流地址（GPL-3.0，独立 `source-youtube-music` 特性） | 无 |

每个 Provider 的解析路径都由 `tests/fixtures/<provider>/` 下的离线响应 fixture 钉住；只读连通性冒烟测试见下文「快速开始」。上游确实缺失的面（如 JOOX 无登录、千千歌手位为幻影标志）均如实登记，不做假实现。

### 本地音乐库（`local` 及其扩展）

扫描核心支持增量复用、失败台账继续扫、CUE 多轨、重复/缺失/损坏诊断、歌词（内嵌 + 外挂 GB18030 降级）、封面（内嵌缓存 + 同目录外挂检出）、原子标签写、NCM 容器解密（独立 `ncm` 特性）；监听器为库侧事件流抽象，自动重扫由宿主调度。与 VoiceFox 本地库的双栈对账见 `docs/integration/LOCAL_LIBRARY_PARITY.md`。

## Cargo features

- `default` = local + lyrics + session-file
- `local` = lofty 元数据 + walkdir 扫描
- `lyrics` = 离线歌词解析/生成/解密门面（网络搜索关闭）
- `lyrics-search` = 可选并发歌词搜索（NetEase、QQ、酷狗、汽水、LRCLIB、AMLL TTML DB）
- `session-file` = 原子、权限受限的 provider 会话持久化
- `download` = 下载引擎核心：HTTP range/多分段 + 重试与逐块停滞检测、候选 CDN 回落（含网易云节点重写）、尺寸/MD5 完整性门、原子 .part 暂存（无字节级断点续传，与上游一致）、标签/歌词收尾；任务编排与 UI 由宿主实现
- `source-youtube-music` = 可选 RustyPipe Provider（GPL-3.0）；需独立缓存目录；helper 不在 PATH 时用 `RustyPipeProvider::new_with_botguard`
- `source-netease` / `source-qqmusic` / `source-kugou` / `source-kuwo` / `source-migu` / `source-bilibili` / `source-jamendo` / `source-apple` / `source-fivesing` / `source-qianqian` / `source-joox` = 各平台原生 Provider（能力明细见英文 README 对应条目；解析路径全部由离线 fixture 钉住）
- `lx-js-bridge` = 可选 LX Music JS 自定义音源桥（stdio 上的 JSON-lines 封套、内置 NodeJsEngine 子进程、search/musicUrl/lyric 包装器；对假引擎单测；真实用户脚本的端到端接线由宿主提供）
- `artwork-local` / `local-cover` / `local-watch` / `ncm` / `cfg` / `stream-semantics` / `track-conversions` / `lyrics-conversions` / `lyrics-cache` / `collection-models` / `auth-model-gates` 等 = 分阶段落地特性（语义见 `Cargo.toml` 注释与 `docs/integration/` 对应设计文档）

## Transport 注入（0.2.1 起）

每个 Provider 的 `with_transport(Http)` 构造器公开：宿主通过 `ClientProfile` 注入自己的代理/超时/连接池预算，或用 `Http::new(my_transport)` 挂完全自定义的 `Transport` 实现（QQ 音乐与咪咕另有第二句柄 `with_transports`）。`new()` 调用方完全不受影响——纯增量变更。组合示例见 `src/transport.rs` 模块文档与 `docs/integration/TRANSPORT_DESIGN.md` §3.1。

## 快速开始（NetEase）

```rust,no_run
use musid_dli::{source_netease::NeteaseProvider, ProviderRegistry};

let provider = NeteaseProvider::new();
let mut registry = ProviderRegistry::new();
provider.register(&mut registry);
```

各模块提供只读连通性冒烟测试：`cargo run --features source-netease --example netease_smoke [MODULE...]`。模块：`search track stream lyrics playlist chart album artist link auth user`（默认 `search track stream lyrics`）。冒烟测试不下载、不播放音频；`auth` 只生成二维码 key 并轮询一次状态，`user` 在无会话 Cookie 时礼貌跳过。QQ 音乐有等价的 `qqmusic_smoke`（匿名时 QQ 自身拒绝可播流地址，模块码 10006）；酷狗 `kugou_smoke`（匿名流模块报 SKIPPED——酷狗无登录不供 URL）；酷我 `kuwo_smoke`、咪咕 `migu_smoke`、Jamendo/Apple/5sing/千千/JOOX 各有专属冒烟示例，行为细节见英文 README。

## 文档索引（docs/integration/）

- `TRANSPORT_DESIGN.md` — Transport/Retry 设计 + 宿主注入（§3.1）
- `AUTH_REGISTRY_DESIGN.md` — 错误/认证/会话/注册表设计
- `MODEL_ALIGNMENT_DESIGN.md` / `MODEL_MAPPING.md` — 数据模型与资源协议语义对齐
- `LOCAL_ENHANCEMENT_DESIGN.md` / `LOCAL_LIBRARY_PARITY.md` — 本地库增强设计 + 与 VoiceFox 的双栈对账
- `RESOURCE_DESIGN.md` — 封面/歌词缓存/匹配/健康/搜索/Resolve 设计
- `PROVIDER_PARITY_MATRIX.md` — Provider 能力对账矩阵
- `FEATURE_DEPENDENCY_MATRIX.md` — 特性依赖矩阵
- `CI_REQUIREMENTS.md` — CI 要求
- `CAMPAIGN_HANDOFF.md` — VoiceFox 整合战役收官存档

## License

MIT
