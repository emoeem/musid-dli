# musid-dli 后续开发总计划书

**项目名称：** musid-dli  
**项目定位：** Rust 原生聚合音乐平台 Provider 库  
**计划版本：** V1.0  
**开发语言：** Rust 2024 Edition  
**目标：** 将多个音乐平台的音乐搜索、歌曲信息、播放地址解析、歌单、专辑、歌手、排行榜、歌词、用户账户及本地音乐能力，统一封装为一个可扩展、可复用、可独立集成的 Rust 音乐数据与资源解析库。

---

# 一、项目背景与目标

## 1.1 项目背景

当前 Linux、Windows、macOS 等桌面音乐播放器通常需要接入多个音乐平台。

不同平台之间存在以下问题：

1. API 接口不同，数据结构不统一。
2. 搜索、歌曲详情、播放地址解析逻辑各自独立。
3. 音质等级、播放地址、版权限制和地区限制处理方式不同。
4. 用户登录、Cookie、二维码认证等机制不统一。
5. 歌词格式和歌词时间轴存在差异。
6. 歌单、专辑、歌手、排行榜等资源接口缺乏统一抽象。
7. 部分项目依赖 JavaScript 插件或运行时，增加部署和维护成本。
8. 多个播放器重复实现相同平台的 Provider，造成维护成本和代码重复。

因此，本项目不应该只是一个网易云音乐 API 封装，也不应该只是某个播放器的内部模块。

**musid-dli 的核心定位是：构建一个独立、原生、统一、可扩展的 Rust 音乐平台聚合库。**

## 1.2 核心目标

项目最终需要实现：

- 多音乐平台统一接入。
- 统一歌曲、歌手、专辑、歌单、排行榜等数据模型。
- 统一搜索接口。
- 统一播放资源解析接口。
- 统一音质选择与降级机制。
- 统一歌词获取和歌词格式转换。
- 统一账户认证与会话管理。
- 统一缓存、重试、错误处理和健康状态管理。
- 支持本地音乐资源。
- 支持多个 Provider 并行扩展。
- 支持按需启用 Provider，避免不必要的依赖。
- 不依赖 Electron、Node.js 或外部 JavaScript 运行时。
- 能够作为 Rust Library 被其他项目直接引用。

## 1.3 非目标

以下内容不属于当前核心库的职责：

1. 不开发完整的桌面播放器 UI。
2. 不负责音频解码、播放引擎和音频设备管理。
3. 不实现播放器的播放队列、窗口管理和桌面交互。
4. 不直接复制 VoiceFox 的播放器架构。
5. 不把某一个音乐平台的私有数据结构暴露为整个库的公共数据模型。
6. 不在没有实际 API 依据的情况下虚构 Provider 能力。
7. 不为了功能数量而创建没有真实实现的空壳接口。
8. 不在本项目中擅自实现音频下载器、DRM 绕过或版权保护规避功能。

---

# 二、项目目录与现状

## 2.1 项目路径

新项目：

`/media/emo/code/musid-dli`

参考项目：

`/media/emo/code/voicefox`

重要约束：

- musid-dli 是独立项目。
- 不修改 VoiceFox。
- 不将 VoiceFox 作为运行时依赖。
- 允许参考 VoiceFox 的数据结构、Provider 设计和 API 实现。
- 不直接复制整个 VoiceFox 源码。
- 参考代码时必须检查许可证、版权声明和上游实现。
- 不擅自初始化 Git、提交或推送。

## 2.2 当前项目基础结构

当前已经建立 Rust Library 项目，主要代码模块如下：

```text
musid-dli/
├── Cargo.toml
├── README.md
├── docs/
│   ├── IMPLEMENTATION_STATUS.md
│   └── VOICEFOX_AUDIT.md
├── examples/
│   └── netease_smoke.rs
└── src/
    ├── lib.rs
    ├── models/
    │   ├── mod.rs
    │   └── playlist.rs
    ├── provider/
    │   └── traits.rs
    ├── registry.rs
    ├── search.rs
    ├── matcher.rs
    ├── resolve.rs
    ├── health.rs
    ├── cache.rs
    ├── session.rs
    ├── session_file.rs
    ├── local.rs
    ├── formats.rs
    ├── lyrics/
    └── source_netease.rs
```

实际目录以工作区文件为准，后续执行者必须先检查当前代码，不得根据计划书假定文件一定存在。

## 2.3 已有功能

### A. 核心基础设施

已经建立或实现了以下模块：

- 统一数据模型。
- Provider Trait 抽象。
- Provider Registry。
- 搜索接口。
- 歌曲匹配器。
- 播放资源解析。
- Provider 健康状态。
- 缓存。
- Session 管理。
- Session 文件管理。
- 本地音乐支持。
- 歌词模块。
- 格式转换模块。

后续开发首先要审查这些模块的实际实现，不要重复创建相同功能。

### B. Feature 设计

当前 Cargo Feature 设计：

```toml
[features]
default = ["local", "lyrics", "session-file"]

lyrics-search = []
source-youtube-music = []
source-netease = []
```

其中：

- 默认功能以本地音乐、歌词和 Session 文件支持为主。
- 网易云音乐 Provider 按需启用。
- YouTube Music Provider 按需启用。
- `--no-default-features` 应保持最小核心依赖。

后续不能为了方便直接把所有平台依赖加入默认 Feature。

### C. 网易云音乐 Provider

当前网易云 Provider 已经实现了较多功能。

已实现的代码能力包括：

1. 歌曲搜索。
2. 歌曲详情。
3. EAPI AES 请求加密。
4. 播放地址解析。
5. 音质降级。
6. 官方播放地址与重定向回退。
7. 文件大小、MD5 和 URL 有效期等信息。
8. 歌词获取。
9. Cookie Session。
10. 二维码登录 Key 生成。
11. 二维码登录状态轮询。
12. 登录风险控制状态处理。
13. 请求重试和退避。
14. 歌单详情。
15. 歌单歌曲列表。
16. 歌单分类。
17. 热门歌单。
18. 歌单搜索。
19. 用户歌单接口。
20. 专辑信息和专辑歌曲。
21. 歌手信息。
22. 歌手热门歌曲。
23. 歌手专辑列表。
24. 排行榜目录。
25. 排行榜歌曲。
26. 歌曲、歌单和专辑链接解析。

注意：上述内容表示代码中已有对应实现，不代表所有接口已经通过真实网络验证。

## 2.4 当前已知测试结果

最近一次验证记录：

| 检查项目 | 结果 |
|---|---|
| cargo fmt --all -- --check | 通过 |
| cargo build --no-default-features | 通过 |
| cargo test --no-default-features | 7 通过 |
| cargo test --all-features | 21 通过 |
| cargo clippy --all-targets --all-features -- -D warnings | 未通过，存在一个待修复问题 |
| cargo tree --no-default-features -e normal | 尚未完成最终检查 |
| cargo package --list | 尚未完成最终检查 |

Clippy 已知问题：

`src/source_netease.rs` 第 117 行附近存在可合并的嵌套 if：

```rust
if response.status().as_u16() == 429 || response.status().is_server_error() {
    if attempt < BACKOFF_MS.len() {
        tokio::time::sleep(Duration::from_millis(delay)).await;
        continue;
    }
}
```

建议修改为：

```rust
if (response.status().as_u16() == 429 || response.status().is_server_error())
    && attempt < BACKOFF_MS.len()
{
    tokio::time::sleep(Duration::from_millis(delay)).await;
    continue;
}
```

修改时必须先确认当前代码是否已经发生变化。

另外，之前的远程编辑请求曾经处于 Pending 状态，不能假设该修改已经成功。

---

# 三、总体开发路线

采用分阶段开发方式，严格按照以下顺序推进：

```text
Phase 0：现状恢复与完整审计
        ↓
Phase 1：核心架构完善
        ↓
Phase 2：网易云 Provider 收尾
        ↓
Phase 3：VoiceFox Provider 迁移研究
        ↓
Phase 4：QQ 音乐 Provider
        ↓
Phase 5：其他音乐平台 Provider
        ↓
Phase 6：账户认证与 Session 统一
        ↓
Phase 7：歌词、格式与资源匹配完善
        ↓
Phase 8：本地音乐和扩展能力
        ↓
Phase 9：性能、稳定性与安全审计
        ↓
Phase 10：文档、发布与生态集成
```

每个阶段必须完成自身验收后才能进入下一阶段。

不允许跳过基础架构审计直接批量迁移 Provider。

---

# 四、Phase 0：现状恢复与完整审计

## 4.1 目标

首先恢复当前项目的准确状态，确认之前未完成的代码修改和验证任务。

这一阶段只做检查和必要的小修复，不进行大规模架构重构。

## 4.2 执行任务

### 任务 0.1：检查工作区

检查：

- 当前目录。
- Cargo.toml。
- 所有 Rust 源文件。
- README。
- docs。
- examples。
- 是否存在未跟踪文件。
- 是否存在意外修改。

如果项目尚未初始化 Git，不要擅自初始化。

如果已经存在 Git 仓库，只读检查：

```bash
git status --short --branch
```

不执行 reset、clean、checkout、stash 等破坏性操作。

### 任务 0.2：确认 Clippy 修改

检查：

`src/source_netease.rs`

确认第 117 行附近的嵌套 if 是否已经修复。

如果没有修复，只做最小修改。

然后执行：

```bash
cargo fmt --all -- --check

cargo clippy --all-targets --all-features -- -D warnings
```

必须确保 Clippy 完全通过。

### 任务 0.3：确认最小依赖

执行：

```bash
cargo tree --no-default-features -e normal
```

重点检查：

- 默认禁用所有 Feature 后是否仍然引入 reqwest。
- 是否存在不必要的 HTTP 依赖。
- 是否存在不必要的 TLS 实现。
- 是否存在重复依赖。
- Feature 之间是否存在循环或隐式耦合。

如果 no-default-features 下仍然引入网络依赖，必须分析来源并修复。

### 任务 0.4：检查发布完整性

执行：

```bash
cargo package --list
```

检查：

- 源文件是否被包含。
- README 是否被包含。
- License 是否存在。
- 文档是否完整。
- 示例是否能够正确编译。
- 是否误包含缓存、临时文件或本地配置。

## 4.3 验收标准

- 工作区状态明确。
- 所有待处理编辑请求已确认结果。
- Clippy 通过。
- no-default-features 依赖边界符合预期。
- Cargo Package 文件列表合理。
- 没有误修改 VoiceFox。

---

# 五、Phase 1：核心架构完善

## 5.1 目标

在增加更多平台之前，确保 Provider 架构足够稳定。

未来可能同时接入十几个甚至更多音乐平台，核心抽象必须避免为每个平台单独写一套业务逻辑。

## 5.2 统一数据模型

检查并完善以下数据结构：

### Track

至少包含：

- id
- source
- title
- artists
- album
- duration
- artwork
- isrc
- track_number
- disc_number
- explicit
- extra_metadata

其中平台特有字段应放入扩展元数据，不应不断向公共模型添加平台专属字段。

### Artist

至少包含：

- id
- source
- name
- aliases
- avatar
- description
- extra_metadata

### Album

至少包含：

- id
- source
- title
- artists
- artwork
- release_date
- track_count
- description
- extra_metadata

### Playlist

至少包含：

- id
- source
- title
- description
- cover
- creator
- track_count
- play_count
- tags
- extra_metadata

### PlaylistCategory

当前已经存在基础结构，需要检查字段是否合理：

- id
- name
- group
- count
- hot

### StreamInfo

至少包含：

- url
- quality
- format
- bitrate
- size
- md5
- expires_at
- headers
- source

注意：URL、Header、Cookie 等敏感信息不能直接写入普通日志。

### Lyrics

统一支持：

- LRC。
- 增强型 LRC。
- 逐字歌词。
- 歌词翻译。
- 歌词与翻译关联。
- 歌词元数据。

## 5.3 Provider Trait 体系

检查现有 Trait 是否存在职责混乱。

建议最终形成以下接口体系：

```text
Provider
├── SearchProvider
├── TrackProvider
├── StreamProvider
├── LyricsProvider
├── PlaylistProvider
├── AlbumProvider
├── ArtistProvider
├── ChartProvider
├── CollectionProvider
├── LinkResolver
└── AuthProvider
```

设计要求：

1. Provider 基础 Trait 只保留公共信息和能力描述。
2. 可选能力通过独立 Trait 表示。
3. 不支持的接口应明确返回 Unsupported。
4. 不能用空列表伪装接口已实现。
5. 不允许强制每个平台实现其不支持的功能。
6. 同一 Provider 可以注册多个能力 Trait。
7. Trait 方法应具有一致的错误类型。
8. 异步接口应考虑对象安全和动态分发需求。

特别注意：

当前已经有 PlaylistProvider、AlbumProvider、ArtistProvider 等接口。后续先检查现有签名，不要直接推翻重写。

## 5.4 Registry 完善

Registry 应支持：

- Provider 注册。
- Provider 注销。
- 按 source ID 查询。
- 按能力查询。
- 查询 Provider 版本。
- 查询 Provider 健康状态。
- 查询是否支持认证。
- 查询是否支持特定音质。
- 查询 Provider 优先级。

还应考虑同一 Provider 的多个实例，例如：

- 不同账户。
- 不同地区。
- 不同 Cookie。
- 不同网络环境。

不要让全局 Registry 直接承担账户状态管理。

## 5.5 统一错误体系

建议建立统一错误分类：

```text
ProviderError
├── Network
├── Timeout
├── RateLimited
├── AuthenticationRequired
├── AuthenticationExpired
├── PermissionDenied
├── RegionRestricted
├── CopyrightRestricted
├── NotFound
├── Unsupported
├── Parse
├── InvalidResponse
├── Unavailable
└── Internal
```

错误应保留底层错误信息，但不应将 Cookie、Token 或签名参数暴露出来。

## 5.6 验收标准

- 核心数据模型稳定。
- Trait 之间职责清晰。
- Registry 能正确注册和查询不同能力。
- 错误分类统一。
- 无平台专属字段污染公共模型。
- 不破坏 no-default-features 编译能力。

---

# 六、Phase 2：网易云 Provider 收尾

## 6.1 目标

先把当前已有的网易云实现收口，作为后续 Provider 的参考实现。

这一阶段不应直接开始 QQ 音乐迁移。

## 6.2 修复和补充测试

优先检查：

`src/source_netease.rs`

需要覆盖以下解析逻辑：

### A. 歌单

- 歌单基础信息。
- 歌单分类。
- 热门歌单。
- 歌单搜索。
- 歌单歌曲 ID 列表。
- 歌单歌曲详情批量解析。
- 空歌单。
- 超大歌单。
- 不完整响应。
- 歌单删除或不可访问。

### B. 专辑

- 专辑详情。
- 专辑歌曲。
- 多歌手。
- 空专辑。
- 缺失封面。
- 缺失发行日期。

### C. 歌手

- 歌手详情。
- 歌手热门歌曲。
- 歌手专辑。
- 分页。
- 空数据。
- 无效 ID。

### D. 排行榜

- 排行榜目录。
- 排行榜详情。
- 排行榜歌曲。
- 排行榜失效。
- 排行榜数据为空。

### E. 链接解析

- 歌曲链接。
- 歌单链接。
- 专辑链接。
- URL 查询参数。
- URL Fragment。
- 短链接。
- 非法 URL。
- 无效资源 ID。

### F. 登录

- QR Key 生成。
- QR 状态轮询。
- 登录成功。
- 登录过期。
- 风控。
- 二维码超时。
- 网络中断。
- Cookie 失效。

## 6.3 网络接口验证

必须区分两类测试：

**单元测试：**

使用固定 Fixture 测试解析逻辑。

**在线 Smoke Test：**

真实访问网易云 API，验证接口当前是否可用。

不得只因为编译成功就宣称接口正常。

在线 Smoke Test 至少验证：

1. 搜索。
2. 歌曲详情。
3. 歌词。
4. 播放资源解析。
5. 歌单分类。
6. 热门歌单。
7. 歌单详情。
8. 歌单歌曲。
9. 排行榜。
10. 专辑。
11. 歌手。
12. 链接解析。
13. 二维码登录 Key 生成。

用户歌单需要真实登录后再测试。

不允许为了 Smoke Test 下载完整音频。

## 6.4 修复 Smoke Test 示例

检查：

`examples/netease_smoke.rs`

补充可独立运行的测试入口。

建议提供：

```bash
cargo run --features source-netease --example netease_smoke
```

示例应支持按测试项目选择执行，避免一次性请求大量接口。

例如：

```text
search
track
stream
lyrics
playlist
chart
album
artist
link
auth
```

需要登录的功能应明确提示用户配置 Session，不要将认证失败当作普通 API 故障。

## 6.5 更新文档

重点更新：

- README.md
- docs/IMPLEMENTATION_STATUS.md
- docs/VOICEFOX_AUDIT.md
- examples/netease_smoke.rs

删除过时描述，例如：

- 网易云歌单尚未实现。
- 专辑接口尚未实现。
- 歌手接口尚未实现。
- 排行榜接口尚未实现。
- 重试机制尚未实现。

但是必须根据实际代码和测试结果更新，不能简单将所有功能标记为已完成。

建议使用以下状态：

| 状态 | 含义 |
|---|---|
| Planned | 尚未开发 |
| Implemented | 已有代码 |
| Unit Tested | 单元测试通过 |
| Integration Tested | 集成测试通过 |
| Live Verified | 真实网络验证通过 |
| Experimental | 实验性实现 |
| Unsupported | 平台不支持 |

## 6.6 验收标准

- 网易云所有已实现接口有对应解析测试。
- Smoke Test 可以按模块执行。
- 真实网络验证结果有记录。
- 文档与代码保持一致。
- 登录功能没有泄漏敏感数据。
- Clippy 和所有测试通过。

---

# 七、Phase 3：VoiceFox Provider 迁移研究

## 7.1 目标

以 VoiceFox 为主要参考项目，系统性迁移其成熟的音乐平台 Provider 能力。

VoiceFox 不是 musid-dli 的运行时依赖，而是 Provider 实现和接口行为的参考来源。

## 7.2 VoiceFox 已知 Provider

此前审计识别出以下 12 个内置在线 Provider：

1. Kuwo。
2. Kugou。
3. QQ Music。
4. NetEase Cloud Music。
5. Migu。
6. Bilibili。
7. Qianqian。
8. JOOX。
9. 5sing。
10. Jamendo。
11. Apple Music。
12. Soda。

另外还有：

- Local。
- LX Music JavaScript Custom Source。

此前 VoiceFox 审计记录：

`docs/VOICEFOX_AUDIT.md`

旧审计中 lx-source 的测试发现约 220 项，其中 218 项通过、2 项忽略；其他模块也有各自的测试结果。

这些数字只是历史审计记录，后续迁移前必须重新检查 VoiceFox 当前版本。

## 7.3 迁移前必须完成的调查

不要直接复制代码。

每个 Provider 都必须建立独立调查记录：

```text
Provider 名称：
上游目录：
主要源文件：
上游 API：
搜索能力：
歌曲详情：
播放地址：
音质支持：
歌词：
歌单：
专辑：
歌手：
排行榜：
链接解析：
认证方式：
Cookie 需求：
地区限制：
依赖：
测试覆盖：
许可证：
维护状态：
迁移风险：
```

重点调查：

1. 上游接口是否仍然有效。
2. 是否依赖外部 JS Runtime。
3. 是否依赖浏览器或 WebView。
4. 是否需要特定加密算法。
5. 是否需要特殊 HTTP Header。
6. 是否需要登录。
7. 是否有接口频率限制。
8. 是否存在地区限制。
9. 是否有成熟的 Fixture。
10. 是否可以使用纯 Rust 实现。

## 7.4 优先级建议

建议按照以下顺序推进：

**第一批：**

- QQ Music。
- Kuwo。
- Kugou。
- Migu。

原因：这些平台可以覆盖较常见的中文音乐搜索和播放资源需求。

**第二批：**

- Bilibili。
- Qianqian。
- JOOX。

**第三批：**

- Apple Music。
- Jamendo。
- 5sing。
- Soda。

优先级只是建议，最终必须以接口可用性、实现难度、维护情况和测试覆盖为依据。

## 7.5 特别要求：JavaScript Custom Source

VoiceFox 中的 JavaScript Custom Source 不应直接成为 musid-dli 的基础依赖。

需要单独研究：

- JS Source 的 API 协议。
- JS Source 的输入输出数据结构。
- JS Source 的签名和加密能力。
- JS Source 的运行时依赖。
- 是否存在安全风险。

如果未来要兼容 JS Source，应设计为独立的可选适配层。

Rust 原生 Provider 必须保持独立，不得因为 JS 兼容层导致默认核心依赖 Node.js。

## 7.6 验收标准

- 完成所有目标 Provider 的上游调查。
- 每个平台都有独立迁移记录。
- 许可证和依赖已确认。
- 迁移顺序经过评估。
- 没有直接复制整个 VoiceFox 项目。
- 明确每个 Provider 的真实能力与限制。

---

# 八、Phase 4：QQ Music Provider

## 8.1 目标

完成第一个大型平台迁移项目。

QQ Music 应作为第二个主要 Provider，用于验证当前架构是否能承载不同平台的 API 风格。

## 8.2 执行前调查

必须先在 VoiceFox 当前源码中找到 QQ Music Provider。

重点检查：

- Provider 注册位置。
- 搜索接口。
- 歌曲详情。
- 播放地址。
- 音质等级。
- 歌词接口。
- 歌单接口。
- 专辑接口。
- 歌手接口。
- 排行榜接口。
- 链接解析。
- 登录机制。
- Cookie 管理。
- 请求签名。
- 参数加密。

如果 VoiceFox 的实现依赖其他项目或 JS 代码，也必须追溯其实际来源。

## 8.3 建议代码结构

在确认项目当前模块布局后，新增独立 Provider 模块：

```text
src/
└── source_qqmusic.rs
```

如果 Provider 规模继续增长，可逐步改为：

```text
src/
└── sources/
    ├── mod.rs
    ├── netease/
    └── qqmusic/
        ├── mod.rs
        ├── api.rs
        ├── models.rs
        ├── search.rs
        ├── stream.rs
        ├── playlist.rs
        ├── album.rs
        ├── artist.rs
        ├── lyrics.rs
        └── auth.rs
```

目录拆分应以实际复杂度为依据，不要为了目录数量过早拆分。

## 8.4 功能实现顺序

建议按以下顺序：

1. Provider 注册。
2. 基础信息与能力声明。
3. 搜索。
4. 歌曲详情。
5. 播放地址解析。
6. 音质选择与降级。
7. 歌词。
8. 歌单。
9. 专辑。
10. 歌手。
11. 排行榜。
12. 链接解析。
13. 登录与 Session。
14. 网络异常处理。

每完成一个模块，就补充测试。

## 8.5 重点技术问题

必须重点验证：

- 歌曲 ID 和媒体 ID 的映射。
- 音质参数。
- URL 签名。
- 资源过期时间。
- Cookie 失效。
- 请求频率限制。
- API 响应结构变化。
- 地区和版权限制。
- 不同歌曲版本的匹配。

不能通过伪造播放地址、绕过 DRM 或规避版权保护来实现所谓的播放成功。

## 8.6 验收标准

QQ Music Provider 必须：

- 可以独立启用。
- 不影响网易云 Provider。
- 通过单元测试。
- 通过 Fixture 解析测试。
- 通过真实网络 Smoke Test（仅对实际可访问的功能）。
- 能够明确返回 Unsupported、AuthenticationRequired 等错误。
- 不将平台内部数据结构泄漏到公共模型。

---

# 九、Phase 5：其他音乐平台 Provider

## 9.1 目标

逐步完成 VoiceFox 其他 Provider 的 Rust 原生实现。

## 9.2 迁移顺序

每个 Provider 单独建立 Feature：

```toml
[features]
source-netease = []
source-qqmusic = []
source-kw = []
source-kg = []
source-migu = []
source-bilibili = []
source-qianqian = []
source-joox = []
source-apple-music = []
source-jamendo = []
```

具体命名必须统一，并在开发前检查是否与已有 Feature 冲突。

## 9.3 单个平台迁移模板

每个平台都必须经过以下流程：

```text
上游代码审计
    ↓
API 与协议调查
    ↓
许可证与依赖检查
    ↓
公共模型映射
    ↓
Provider Trait 实现
    ↓
单元测试
    ↓
Fixture 测试
    ↓
网络 Smoke Test
    ↓
错误处理
    ↓
文档更新
    ↓
Feature 独立编译
    ↓
合并验收
```

## 9.4 不同平台的能力差异

不能要求所有平台都实现完全相同的接口。

例如：

- 某平台可能不提供排行榜。
- 某平台可能不支持歌手专辑分页。
- 某平台可能需要登录才能获取高音质。
- 某平台可能只提供公开试听资源。
- 某平台可能只支持特定地区。
- 某平台可能不提供逐字歌词。

必须通过 Provider 能力声明明确表示。

---

# 十、Phase 6：账户认证与 Session 统一

## 10.1 目标

让多个音乐平台的登录机制可以统一管理。

但不能把所有平台的认证方式强行设计成同一种形式。

## 10.2 支持的认证方式

需要逐步支持：

- Cookie。
- QR Code。
- OAuth。
- 用户名密码（仅在平台明确支持且安全可行时）。
- Token。
- 匿名 Session。
- 平台专用认证流程。

## 10.3 统一 Auth 抽象

建议设计：

```text
AuthProvider
├── begin_login()
├── poll_login()
├── refresh_session()
├── validate_session()
├── logout()
└── session_status()
```

以上是设计方向，最终接口需要考虑不同平台的异步登录流程和状态差异。

## 10.4 Session 管理

必须区分：

- Provider ID。
- Account ID。
- Session ID。
- 登录状态。
- Cookie。
- Token。
- 过期时间。
- 更新时间。
- 地区信息。

一个 Provider 可以存在多个账户。

## 10.5 安全要求

1. 不在日志中打印 Cookie。
2. 不在日志中打印 Token。
3. Session 文件应有合理的文件权限。
4. 不允许默认明文保存敏感认证信息而不作说明。
5. 为敏感数据设计加密存储扩展点。
6. 支持 Session 失效检测。
7. 支持主动注销。
8. 不将账户凭证写入 Git。
9. 不在测试 Fixture 中放入真实用户 Cookie。

## 10.6 验收标准

- 多 Provider Session 可以独立管理。
- 多账户之间不会相互覆盖。
- Session 过期能够被识别。
- 认证失败能够明确分类。
- 敏感信息不会泄漏。

---

# 十一、Phase 7：歌词、格式与歌曲匹配

## 11.1 歌词系统

目标是建立统一歌词处理体系。

支持：

- LRC。
- Enhanced LRC。
- 逐字歌词。
- 歌词翻译。
- 歌词罗马音。
- 歌词时间轴。
- 歌词元数据。
- 歌词格式转换。

与 `lyrics-helper` 生态的集成必须保持边界清晰。

外部库应通过适配层接入，不应让核心歌词模型依赖某个特定平台的格式。

## 11.2 歌曲匹配

需要支持跨平台匹配。

例如：

```text
网易云歌曲
    ↓
提取歌曲信息
    ↓
匹配 QQ Music
    ↓
匹配 Kuwo
    ↓
匹配 Kugou
    ↓
返回候选资源
```

匹配不能只依赖歌曲名称。

建议综合：

- 歌曲名称。
- 歌手名称。
- 专辑名称。
- 时长。
- ISRC。
- 版本信息。
- 音质信息。
- 语言。
- 歌曲类型。

需要特别处理：

- Live 版本。
- Remix 版本。
- 伴奏版本。
- 翻唱版本。
- 纯音乐版本。
- 不同发行版本。
- 同名歌曲。
- 多歌手合作。

匹配器应返回候选列表和可信度，而不是直接把低可信度匹配当作正确结果。

## 11.3 格式转换

建立统一格式映射：

- 平台专属歌曲模型 → Track。
- 平台专属歌单模型 → Playlist。
- 平台专属专辑模型 → Album。
- 平台专属歌手模型 → Artist。
- 平台专属歌词 → Lyrics。

避免每个平台重复实现公共数据转换逻辑。

## 11.4 验收标准

- 不同平台数据能够映射到统一模型。
- 歌词时间轴转换正确。
- 歌曲匹配能够识别常见版本差异。
- 错误匹配可以被解释和追踪。

---

# 十二、Phase 8：本地音乐与扩展能力

## 12.1 本地音乐

完善 Local Provider：

- 本地文件扫描。
- 文件元数据读取。
- 封面提取。
- 歌词关联。
- 文件格式识别。
- 文件变化检测。
- 本地音乐搜索。
- 本地专辑和歌手聚合。

## 12.2 格式支持

优先考虑常见音乐格式：

- MP3。
- FLAC。
- WAV。
- AAC。
- M4A。
- OGG。
- OPUS。
- WMA。
- ALAC。

具体格式支持应依据实际使用的 Rust 元数据库和测试结果。

注意：

musid-dli 负责音乐资源和元数据管理，不负责替代 mpv、GStreamer 等专业音频播放引擎。

## 12.3 扩展 Provider

后续应允许第三方开发者自行创建 Provider。

建议提供：

- Provider 开发文档。
- 最小示例。
- Trait 使用说明。
- Feature 设计说明。
- Fixture 测试模板。
- 错误处理规范。
- 能力声明规范。

---

# 十三、Phase 9：性能、稳定性与安全审计

## 13.1 性能测试

重点测试：

- 多 Provider 并发搜索。
- 多歌曲批量详情。
- 大型歌单解析。
- 缓存命中率。
- Session 查询。
- 歌曲匹配。
- 歌词解析。
- 大量 Provider 注册。

建议建立 Benchmark。

不能只通过主观感觉判断性能。

## 13.2 并发与稳定性

检查：

- 请求超时。
- 请求取消。
- 重试策略。
- 指数退避。
- 随机抖动。
- 限流。
- 并发上限。
- 连接池。
- 缓存过期。
- Provider 故障隔离。

某一个平台不可用时，不应该影响其他 Provider。

## 13.3 安全审计

重点检查：

1. Cookie 和 Token 泄漏。
2. URL 注入。
3. 不可信链接解析。
4. 重定向安全。
5. SSRF 风险。
6. 文件路径穿越。
7. 本地 Session 文件权限。
8. 依赖漏洞。
9. 日志敏感数据。
10. 不安全的 TLS 配置。

对播放地址解析和链接解析，尤其需要限制不可信 URL 的处理范围。

## 13.4 依赖审计

建议使用：

```bash
cargo tree
cargo tree --no-default-features
cargo audit
cargo deny check
```

如果工具未安装，先记录缺失情况，不要因此直接修改系统环境或全局依赖。

## 13.5 验收标准

- 无严重安全问题。
- 大型歌单不会导致明显内存问题。
- Provider 之间相互隔离。
- 网络请求有超时和限流机制。
- 依赖树合理。
- Benchmark 有可复现记录。

---

# 十四、Phase 10：文档、发布与生态集成

## 14.1 文档结构

最终建议形成：

```text
docs/
├── ARCHITECTURE.md
├── DATA_MODEL.md
├── PROVIDER_GUIDE.md
├── PROVIDER_STATUS.md
├── AUTHENTICATION.md
├── SESSION_SECURITY.md
├── ERROR_HANDLING.md
├── FEATURE_FLAGS.md
├── TESTING.md
├── BENCHMARKS.md
├── SECURITY.md
├── VOICEFOX_AUDIT.md
└── IMPLEMENTATION_STATUS.md
```

不要求一次性全部创建。

应随着对应阶段逐步编写，避免产生没有实际内容的空文档。

## 14.2 README

README 必须说明：

- 项目定位。
- 支持的平台。
- Feature 列表。
- 安装方法。
- 基础使用方式。
- 搜索示例。
- 播放地址解析示例。
- 歌词示例。
- Provider 扩展方法。
- Session 配置。
- 许可证。
- 当前开发状态。

所有示例必须与真实 API 保持一致。

## 14.3 Rust API 质量

最终应做到：

- 公共 API 有文档。
- 示例能够编译。
- 类型命名一致。
- 错误类型清晰。
- Feature 依赖合理。
- semver 变更有记录。
- 不随意暴露内部实现。

## 14.4 发布前检查

执行：

```bash
cargo fmt --all -- --check

cargo build --no-default-features

cargo test --no-default-features

cargo test --all-features

cargo clippy --all-targets --all-features -- -D warnings

cargo package --list

cargo package
```

如果支持的目标平台包括 Windows、macOS 和 Linux，还应安排对应平台的 CI 编译测试。

---

# 十五、统一开发规范

## 15.1 每批次固定工作流程

所有后续 AI 编程代理必须遵循：

```text
1. 阅读项目文档
2. 检查当前代码
3. 检查上游实现
4. 确认本批次目标
5. 制定最小修改计划
6. 执行代码修改
7. 添加或更新测试
8. cargo fmt
9. cargo test
10. cargo clippy
11. 检查 Feature 影响
12. 检查依赖变化
13. 更新文档
14. 输出完成报告
```

不能跳过源码检查直接写代码。

## 15.2 最小化修改原则

- 优先复用已有模型和 Trait。
- 不重复创建相同功能。
- 不随意修改公共 API。
- 不无理由升级大量依赖。
- 不顺手重构无关模块。
- 不为了测试通过而删除测试。
- 不通过降低检查严格程度解决问题。
- 不把失败的在线 API 测试伪装成通过。

## 15.3 Git 操作约束

除非用户明确授权：

- 不提交。
- 不推送。
- 不创建 Release。
- 不修改远程分支。
- 不执行 reset。
- 不执行 clean。
- 不执行 checkout 覆盖用户修改。
- 不执行 stash。
- 不执行 rebase。

如果用户要求提交或推送，应先精确检查并暂存本批次相关文件。

## 15.4 真实测试与模拟测试分离

所有 Provider 测试必须区分：

| 测试类型 | 目的 |
|---|---|
| Unit Test | 测试内部函数 |
| Fixture Test | 测试真实格式的固定响应 |
| Integration Test | 测试模块之间的交互 |
| Live Smoke Test | 验证真实平台接口 |
| Benchmark | 验证性能 |
| Security Test | 验证安全边界 |

网络接口测试可能受到限流、地区、账号和平台变更影响。

不能因为在线接口暂时失败，就直接认定本地解析代码错误。

同样，也不能因为 Fixture 测试通过，就宣称真实平台接口已经验证。

---

# 十六、每个 Provider 的统一验收清单

每增加一个 Provider，必须完成以下清单：

- [ ] 已调查上游源码。
- [ ] 已检查许可证。
- [ ] 已检查依赖。
- [ ] 已确认 API 来源。
- [ ] 已建立 Feature。
- [ ] 已实现 Provider 注册。
- [ ] 已实现能力声明。
- [ ] 已实现搜索。
- [ ] 已实现歌曲详情。
- [ ] 已实现播放地址解析（若平台支持）。
- [ ] 已实现音质映射。
- [ ] 已实现歌词（若平台支持）。
- [ ] 已实现歌单（若平台支持）。
- [ ] 已实现专辑（若平台支持）。
- [ ] 已实现歌手（若平台支持）。
- [ ] 已实现排行榜（若平台支持）。
- [ ] 已实现链接解析（若平台支持）。
- [ ] 已实现认证（若平台需要）。
- [ ] 已实现请求超时。
- [ ] 已实现错误处理。
- [ ] 已实现重试或限流策略。
- [ ] 已补充单元测试。
- [ ] 已补充 Fixture 测试。
- [ ] 已执行网络 Smoke Test 或明确记录无法验证的原因。
- [ ] 已执行 Feature 独立编译。
- [ ] 已执行 Clippy。
- [ ] 已更新文档。

不支持的能力必须明确标记，不得为了通过清单而创建空实现。

---

# 十七、建议的最终项目架构

以下是目标结构示意，后续根据项目实际规模逐步演进，不要求一次性重构：

```text
musid-dli/
├── Cargo.toml
├── README.md
├── LICENSE
├── docs/
├── examples/
├── benches/
├── tests/
└── src/
    ├── lib.rs
    │
    ├── models/
    │   ├── track.rs
    │   ├── artist.rs
    │   ├── album.rs
    │   ├── playlist.rs
    │   ├── stream.rs
    │   └── lyrics.rs
    │
    ├── provider/
    │   ├── mod.rs
    │   ├── traits.rs
    │   ├── registry.rs
    │   ├── capabilities.rs
    │   └── error.rs
    │
    ├── search/
    ├── matcher/
    ├── resolve/
    ├── lyrics/
    ├── formats/
    ├── cache/
    ├── health/
    ├── session/
    ├── auth/
    ├── local/
    │
    └── sources/
        ├── mod.rs
        ├── netease/
        ├── qqmusic/
        ├── kuwo/
        ├── kugou/
        ├── migu/
        ├── bilibili/
        ├── qianqian/
        ├── joox/
        ├── apple_music/
        ├── jamendo/
        └── other/
```

架构原则：

1. 核心模块不依赖具体音乐平台。
2. 每个 Provider 独立实现。
3. 每个平台的网络请求和解析逻辑尽可能隔离。
4. 公共模型不包含平台私有业务逻辑。
5. 每个平台通过 Feature 按需启用。
6. 所有 Provider 共用统一错误、缓存、日志和 Session 基础设施。
7. 不让某个平台的异常影响其他平台。

---

# 十八、当前开发者的实际执行顺序

**后续执行人员不要从 Phase 1 直接开始。应先按以下清单处理。**

## 第一优先级：收尾当前工作

1. 恢复远程开发设备连接。
2. 检查之前 Pending 的代码修改是否完成。
3. 修复网易云 Clippy 问题。
4. 重新运行完整测试。
5. 检查 no-default-features 依赖树。
6. 检查 Cargo Package 文件。
7. 补充网易云解析单元测试。
8. 扩展 `netease_smoke.rs`。
9. 更新 README 和 IMPLEMENTATION_STATUS.md。
10. 确认 VoiceFox 工作区没有被修改。

## 第二优先级：完成网易云 Provider

1. 逐个检查歌单、专辑、歌手、排行榜和链接解析。
2. 补齐 Fixture 测试。
3. 执行可用的真实网络测试。
4. 测试二维码登录。
5. 验证用户歌单接口。
6. 整理已验证和未验证能力。
7. 将网易云 Provider 收口为稳定参考实现。

## 第三优先级：开始 QQ Music

1. 重新审计 VoiceFox 当前源码。
2. 定位 QQ Music Provider 的完整实现。
3. 调查 API 和加密协议。
4. 评估纯 Rust 实现方案。
5. 形成独立迁移计划。
6. 按功能逐步实现。
7. 补齐测试。
8. 通过完整质量检查。

## 第四优先级：批量扩展平台

按照 Kuwo、Kugou、Migu、Bilibili、Qianqian 等平台逐步推进。

每个平台独立验收，不能把多个未完成 Provider 合并成一个庞大的开发批次。

---

# 十九、最终完成标准

musid-dli 不以“接入了多少个平台”作为唯一完成标准。

真正达到可长期维护的成熟状态，需要满足：

### 架构

- 有稳定的公共数据模型。
- 有清晰的 Provider Trait。
- 有可靠的 Registry。
- 有统一错误体系。
- 有合理的 Feature 设计。

### 平台

- 多个主流音乐平台具备真实可用的 Provider。
- 各平台功能有明确能力声明。
- 不同平台的数据能够统一映射。
- 平台接口变更可以独立维护。

### 资源

- 搜索结果能够统一展示。
- 歌曲能够跨平台匹配。
- 播放资源能够按音质选择。
- 能够获取平台提供的歌词。
- 能够解析歌单、专辑和排行榜。

### 认证

- 多平台 Session 独立管理。
- 多账户互不干扰。
- 凭证安全保存。
- 登录状态能够正确识别。

### 质量

- 单元测试完整。
- Fixture 测试可靠。
- 关键接口有真实网络验证记录。
- Clippy 无警告。
- Feature 组合可以独立编译。
- 依赖树合理。
- 有基础性能测试。
- 有安全审计记录。

### 生态

- 能够独立作为 Rust Library 使用。
- 能够被其他 Rust 项目集成。
- 有明确的第三方 Provider 扩展规范。
- 不依赖 VoiceFox 运行。
- 不依赖 Electron 或 Node.js 作为核心运行环境。

---

# 二十、交给后续 AI 的执行指令

你现在是 musid-dli 项目的主要 Rust 架构师和开发工程师。

请严格按照本计划书执行，不要只输出建议或重复计划内容。

**开始执行时必须遵循以下要求：**

1. 首先检查 `/media/emo/code/musid-dli` 当前真实代码。
2. 阅读 README、docs、Cargo.toml 和现有 Provider 实现。
3. 阅读 `/media/emo/code/voicefox` 的相关源码和之前的审计记录。
4. 确认当前代码修改、测试结果和工作区状态。
5. 不要假定之前的远程操作已经成功。
6. 不要直接开始 QQ Music 或其他新 Provider，先完成网易云收尾。
7. 不要重写已经完成的模块。
8. 不要为了追求架构完整而一次性大规模重构。
9. 每个批次先分析、再制定最小修改计划，然后执行。
10. 每个批次必须有测试和验收结果。
11. 明确区分已实现、已测试、已集成、已真实网络验证。
12. 不要擅自提交、推送或执行破坏性 Git 操作。
13. 不得修改 VoiceFox 原项目。
14. 如果发现计划书与当前源码不一致，以当前真实源码为准，并记录差异。
15. 遇到平台 API 失效、认证失败或地区限制时，记录事实，不允许伪造成功结果。

**第一项具体任务：执行 Phase 0，并继续完成 Phase 2 的网易云 Provider 收尾。**

在 Phase 0 和 Phase 2 验收通过之前，不进入 QQ Music Provider 开发。

每次工作结束，必须输出：

```text
本批次目标：
检查过的文件：
修改过的文件：
新增功能：
修复问题：
新增测试：
测试结果：
Clippy 结果：
Feature 编译结果：
在线验证结果：
尚未解决的问题：
下一批次任务：
```

最终目标是把 musid-dli 建设成一个真正独立、原生、稳定、可扩展的 Rust 音乐平台聚合库，而不是简单地将多个 API 拼接在一起。