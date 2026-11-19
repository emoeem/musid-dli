# LOCAL_ENHANCEMENT_DESIGN.md

M5 本地音乐能力增强的设计与落地面（盘点基线：docs/integration/LOCAL_LIBRARY_PARITY.md，2026-11-19 再锚定版；盘面 JSON 的切片 m5-s1…m5-s10）。落地状态（2026-11-20）：**十个切片 s1 至 s10 全部落地并通过各自门禁**：s1（扫描核重做）、s2（CUE 进扫描）、s3（标签写安全化，外层门禁 feature cfg）、s4（指纹/增量/统计）、s5（重复/缺失/损坏诊断）、s6（元数据回填规范化）、s7（歌词读取）、s8（封面读取缓存，新 feature local-cover）、s9（watcher 事件抽象，新 feature local-watch）、s10（取消/进度/并发）。第二轮评审 6 条（P2-1 子树盲区契约、P2-2 原语拒符号链接、P3-3 canonical 签名键、P3-4 promote 不变量措辞、P3-5 实测数、快路径取消逐条计数 nit）逐条修复完毕（§3.1/§3.2/§3.5/§3.6/§4.1/§4.2/§7/§8，钉住测试 T31 至 T34；证据命令与数字见 LOCAL_LIBRARY_PARITY.md 修复轮证据表）。

所有行号为 2026-11-20 现场实读锚点。上游参照 voicefox `source/src/local/{scanner.rs,mod.rs,metadata.rs}` 与 `source/src/cover_cache.rs`（只读，不修改）。

## 0. 硬边界（本批全部满足）

| 边界 | 落点 |
|---|---|
| 写操作安全：临时文件 + 原子替换（平台允许时），失败不破坏源文件 | §4 tag_write 原子写策略 |
| 不隐式修改用户文件：用户音频文件在库内没有任何写路径，唯一的用户文件写面（write_metadata）只能由调用方显式触发；s8 新增的封面缓存写面只落 **host 指定的缓存目录**（read_cover 传入或 ScanOptions.cover 配置，缺省零封面工作零写盘），用户音频仍只读 | §4/§3.11 |
| 失败不中止：坏文件/权限错误/walker 错误记台账继续扫 | §5 台账 |
| 大库不一次性全量进内存：流式产出，库核不持有条目累积 | §6/§7 |
| 取消 / 进度 / 并发上限 | §6 |
| 不绑死 UI runtime：std-only（无 tokio/async，无 notify），事件=回调或调用方自取 | §5/§6 |
| 9 个 Provider 与现有测试不破坏：兼容层行为逐字保留 | §3.7 |
| no-default 边界保持：新代码全部挂在 feature 后面，零新依赖 | §2/§8 |

## 1. 现状与问题（盘点结论复述，现场复核）

1. **失败语义相反**：`FilesystemLibrary::scan` 收集 `Result`，第一个坏文件以 `Error::Provider` 终止整次扫描（src/local.rs:44-55, 71-74, 164-168；parity L8）。voicefox 是台账继续扫（scanner.rs:216-222）。
2. **标签写为原地写**：`download::write_metadata` 用 lofty `save_to_path` 直写源文件（download.rs:1218-1262；voicefox metadata.rs:65-67 同样原地写）。两侧都没有原子性；M5 硬边界要求临时文件+原子替换，属**两侧皆无的新工作**。
3. **无指纹/增量/统计/取消/进度/并发**：每次扫描全量重读标签；目录签名、`FileFingerprint`、`LocalScanStats`、取消面全部缺失（parity L4/L5/L6/L10；s10 为 musid-only，上游全文核读无 cancel/progress/并发）。
4. **CUE 不进扫描**：`AUDIO_EXTENSIONS` 无 `cue`（local.rs:58-60），`CueHandler::inspect_tracks`（formats_cue.rs:44-79，M4 已落地）只能由 host 显式调用（parity L21）。
5. **无"来源 vs 播放"双路径概念**：`LocalTrack{path, metadata}` 单路径（local.rs:30-33）；voicefox `LocalSong.file_path`（来源）与 `song.file_path`（播放）分离（mod.rs:22-28）。

## 2. 模块落位（最小修改）

| 单元 | feature 门 | 内容 |
|---|---|---|
| `src/local.rs`（已有） | `local` | 保持不动 + 两处最小改动：`AUDIO_EXTENSIONS` 改 `pub(crate)`（local_scan 复用）；`read_metadata` 抽出 `pub(crate) fn read_metadata_inner(path) -> Result<LocalMetadata, lofty::error::LoftyError>`，公开函数签名与行为逐字不变（`Error::Provider` 包装点不变），供扫描核做错误分类 |
| `src/local_scan.rs`（新增） | `local` | M5 扫描核：ScanOptions/指纹/目录签名/台账/流式引擎/增量/取消/进度/并发/CancelToken |
| `src/tag_write.rs`（新增） | `cfg(any(feature = "local", feature = "download"))` | 标签写共享单元：`MetadataEdit` + `write_metadata` + `atomic_replace` 原语。不让 download 蕴含 local，保持 FEATURE_DEPENDENCY_MATRIX §4 记录的"download 直接用 dep:lofty、与 local 无耦合"不变。外层门禁 feature `cfg = ["local", "download"]`（Cargo.toml:74-82，2026-11-20 补）把该 any() 并集钉成可选面供外层按名选择（`cargo test/clippy --features cfg`），两 feature 各自可选性与模块门均不变，`cfg` 本身不 gate 任何代码 |
| `src/download.rs`（已有） | `download` | 删除本地定义，改 `pub use crate::tag_write::{MetadataEdit, write_metadata};`，公开路径 `download::write_metadata`/`download::MetadataEdit` 不变（download.rs:1466 测试模块 `use super::*` 与 24 个 download 测试零改动） |
| `src/local_cover.rs`（新增，s8） | `local-cover` | 封面读取单元：内嵌提取 + 解码校验缓存 + 外置识别 + `read_cover` 组合面 + 扫描槽接线（§3.11）。缓存目录策略（默认目录/预算/sweep）归 host，越界不变 |
| `src/local_watch.rs`（新增，s9） | `local-watch`（= `local` + `dep:notify`） | watcher 事件流抽象（§5.2，parity L7）：WatchEvent 映射 + 双交付面 + 静默窗去抖 + StopHandle。零 tokio/runtime，平台 FFI 只随本 feature 支付 |
| `src/lib.rs` | （无） | 增 `pub mod` 接线（s1-s7 两个 + s8 一个 + s9 一个） |

新依赖：s1-s7 **零**（全部用已有依赖：walkdir/lofty/encoding_rs/cue-rw/std）；s8 落地引入 `image 0.25`（png/jpeg 解码，default-features=false）与 `sha2 0.10`（内容哈希），均挂在 `local-cover` 后不进默认面（沿用 ncm/local 分离先例），已按 DEPENDENCY_AUDIT 惯例审计（docs/DEPENDENCY_AUDIT.md 2026-11-20 补两行）；s9 落地引入 `notify 8.2`（resolved 8.2.2，CC0-1.0，上游 voicefox 同 pin），挂在 `local-watch` 后不进默认面（docs/DEPENDENCY_AUDIT.md 2026-11-20 补第三行；会话开场 grep 证据：Cargo.lock 当日 notify 计数为 0）。

## 3. 扫描核 API（src/local_scan.rs）

### 3.1 ScanOptions（s1，2026-11-20 落地）

```rust
pub struct ScanOptions {
    /// 目录深度上限；0 = 不限（voicefox scanner.rs:231-238 口径）。
    pub max_depth: u32,                    // 默认 0
    /// 按目录名排除（不含路径模式）。默认移植上游 EXCLUDED_DIRS
    /// [".stfolder", "node_modules", ".git", ".Trash"]（scanner.rs:24, 242-246），
    /// 调用方可追加；根目录本身（depth 0）不受排除与隐藏规则影响。
    pub excluded_dirs: Vec<String>,        // 默认 = 移植表
    /// 跳过 `.` 开头的文件与目录。默认 false = 与现行为、上游行为一致
    /// （上游无隐藏过滤；默认值选择"零行为变化"）。gitignore 式规则明确不做。
    pub skip_hidden: bool,                 // 默认 false
    /// 符号链接策略。Skip = 现行为 follow_links(false)（local.rs:66），
    /// 符号链接条目整体不进扫描；Follow = walkdir follow_links(true)，
    /// 循环链接由 walkdir 以错误条目上报并进台账。
    pub symlinks: SymlinkPolicy,           // 默认 Skip
    /// 绕过目录签名快路径（§7）。监听器等"已知有变化"的调用方必须置位：
    /// 目录 mtime 不反映原地修改，且**已存在子目录内部**的增删只移动该子
    /// 目录自身的 mtime，不移动根签名（Artist/Album/track 布局下在既有歌手
    /// 目录里加专辑即此情形，无 force 会一直漏检）。**host 契约**：没有
    /// 递归 watcher 的 host 必须周期性 force 或自行递归比对。两轮之间改变
    /// 其它选项（skip_hidden/excluded_dirs/max_depth/symlinks）**不会**使快
    /// 路径失效（签名只反映根的直接子项），改选项需配 force 或由 host 按
    /// 选项键控签名。
    pub force: bool,                       // 默认 false
    /// 元数据解析阶段 worker 数；1 = 纯串行（现行为）。发现阶段恒为单线程惰性。
    pub max_parse_workers: usize,          // 默认 1
    /// 并行时按发现顺序产出（重排序缓冲）；false = 完成即产出。
    pub ordered_output: bool,              // 默认 true
}
pub enum SymlinkPolicy { Skip, Follow }
// Follow：同一实体经 N 条链接可达即产出 N 个条目（source_path 各异、独立计入
// discovered/parsed、独立占增量 key）；循环链接由 walkdir 以错误条目上报进台账，
// 不会挂起扫描。
```

`Default` 为上表右列；`max_parse_workers=0` 归一为 1。

### 3.2 指纹与目录签名（s4，照抄口径，2026-11-20 落地）

```rust
/// 文件系统指纹：size + mtime 纳秒。纯 fs 元数据，无内容哈希
/// （voicefox scanner.rs:47-66，含"编辑器同秒保存"纳秒动机注释）。
pub struct FileFingerprint { pub size: u64, pub modified_unix_nanos: u128 }
impl FileFingerprint { pub fn from_path(path: &Path) -> std::io::Result<Self> }

/// 目录签名：目录自身 size + mtime。**只有该目录的直接子项**发生
/// 增删/重命名才移动它：原地修改不移动任何签名，已存在子目录内部的变更
/// 只移动那个子目录自己的签名。根签名匹配仅证明"根的直接子项无增删"，
/// 不证明整树无变化；有正向变化证据（watcher 事件、定期全量）的重扫必须
/// force，无 watcher 的 host 必须周期性 force 或自行递归比对（§7）。
/// （scanner.rs:68-96、mod.rs:200-217 同为根级口径。）
pub struct DirSignature { pub size: u64, pub modified_unix_nanos: u128 }
impl DirSignature { pub fn from_path(path: &Path) -> Option<Self> }
```

粒度告警（照抄上游的已知限制）：exFAT/FAT32（Windows 可移动盘常见）mtime 粒度
2 秒，粒度窗口内的同尺寸原地改会被指纹漏检；上游 scanner.rs:47-66 同病，如实
记录不做修正。

### 3.3 台账（s1，2026-11-20 落地）

```rust
pub struct ScanFailure { pub path: PathBuf, pub error: String, pub kind: ScanFailureKind }
pub enum ScanFailureKind { Permission, Io, Parse }
```

分类规则（对上游"静默丢弃 walker 错误"（scanner.rs:247 `filter_map(Result::ok)`）的超集，升级为记录）：

- **walkdir 错误**：`walkdir::Error::io_error()` 的 `io::ErrorKind::PermissionDenied` 记 `Permission`，其余记 `Io`；
- **指纹 stat 失败**：同上规则；
- **lofty 读取失败**（普通音频与 CUE 引用音频）：`lofty::error::ErrorKind::Io(io)` 按 io kind 二分，其余 lofty 错误记 `Parse`。实证（2026-11-20 probe）：坏字节的 `.mp3` 在 MPEG 帧同步搜索中以 io 错误收尾，记 `Io`；`.flac`/`.wav`/`.ogg` 的坏魔数记 `Parse`；两类都进台账、都不中止。
- **CUE 解析失败**（formats_cue 全管线，含引用音频缺失/编码不可解码）：`Parse`（该阶段无法可靠分离底层 IO 类别，如实记录阶段）。

台账条目不进入条目流；同一文件下轮扫描仍会重试解析（指纹仅在解析成功后跟随条目持久化，与上游一致：voicefox 的复用分支也要求前态存在该文件的歌，scanner.rs:153-165）。

### 3.4 ScanEntry：来源/播放双路径模型（s2，2026-11-20 落地）

```rust
pub struct ScanEntry {
    /// 来源文件：普通音频 = 自身；CUE 分轨 = 所属 .cue（增量 key，§7）。
    pub source_path: PathBuf,
    /// 播放文件：普通音频 = source_path；CUE 分轨 = ContainerTrack.audio_path。
    /// 路径解析与上游逐字同构（cue.parent().join(FILE 名).canonicalize()），
    /// `../` 或绝对 FILE 名可指向扫描根之外；包含性由 host 自担（§9）。
    pub play_path: PathBuf,
    /// 轨身份：普通音频 = None；CUE = "{cue 路径}#{两位序号}"（scanner.rs:353 口径）。
    pub container_track: Option<String>,
    /// CUE 分轨起始偏移（INDEX 01，毫秒）；普通音频 = 0。
    pub start_ms: u64,
    pub metadata: LocalMetadata,
    pub fingerprint: FileFingerprint,
    /// 本条是否直接复用上次条目（true = 未重解析）。
    pub reused: bool,
    /// L24 接线（scanner.rs:203-210 + scanner.rs:381-385），可直接并入 Track.extra：
    /// `file_size`、`file_modified_unix_nanos`（键名照抄 lx_core model/song.rs:8）、
    /// CUE 条目另加 `cue_file`/`cue_track`/`cue_start_ms`。
    /// 与上游差异（有意超集）：上游只给普通音频写 file_* 两键，本库对 CUE 条目同样写入。
    pub extra: BTreeMap<String, String>,
}
```

- `metadata.audio` 保持 L13 语义：每条产出的条目都携带测量 AudioSpec。CUE 条目的 spec/容器时长来自对 `play_path` 的 `read_metadata_inner`（每个去重后的音频文件读一次）；title/artist/album 取 `ContainerTrack`（formats_cue 已含 "Track NN"/未知艺术家/专辑回落，formats_cue.rs:240-272），`metadata.duration_ms` = 分轨时长。**已知成本**：`inspect_tracks` 内部已读一次基底元数据（formats_cue.rs:188-196），扫描核为取 spec 再读一次 = 每个被改动的 CUE 音频多一次 lofty 解析；仅发生在指纹变化时，接受并记录（上游为一次读，因上游 cue 构建直接持有 base）。
- `LocalMetadata`、`LocalTrack` 类型不动；`LocalTrack` 兼容面继续由 `read_track` 提供。
- 已知限制（照抄上游）：CUE 指纹只看 .cue 文件自身；**引用音频被原地替换而 sheet 未变时不会触发重解析**（voicefox 同 scanner.rs:147-165）。设计上不做"音频指纹并入"，保持 parity 口径，差异如实记录。

### 3.5 ScanState：前态由 host 传入（无库内状态，s4，2026-11-20 落地）

```rust
/// 上次扫描的沉淀物。musid 无内存库：host 持有并传入，纯函数式更替。
#[derive(Debug, Default, Clone)]
pub struct ScanState {
    pub entries: HashMap<PathBuf, Vec<ScanEntry>>,      // key = source_path
    pub dir_signatures: HashMap<PathBuf, DirSignature>, // key = 扫描根（canonicalize 后）
}
impl ScanState {
    pub fn absorb(&mut self, event: &ScanEvent);        // 事件流 → 状态的单步折叠
    pub fn entries_under(&self, root: &Path) -> impl Iterator<Item = (&PathBuf, &Vec<ScanEntry>)>;
    pub fn fingerprint_of(&self, source: &Path) -> Option<FileFingerprint>;
    /// 记录签名；root 与 scan_stream 同样 canonicalize（符号链接/相对根的
    /// 键也能对齐引擎查找键）。
    pub fn set_dir_signature(&mut self, root: &Path, signature: DirSignature);
    /// 推荐折叠面：完整且干净的扫描（dir_signature=Some）才记录签名；取消轮
    /// 或台账非空的扫描会**移除**已存签名，防止下轮快路径跳过它从未见过的
    /// 文件（权限修复只更新 ctime 不更新 mtime；修复后的损坏文件同理）。
    /// root 同样 canonicalize 后做键。
    pub fn apply_summary(&mut self, root: &Path, summary: &ScanSummary);
}
```

与上游差异（有意）：voicefox 把 songs/fingerprints 拆两张表（mod.rs:88-96）；musid 把指纹内嵌在条目上（`ScanEntry.fingerprint`），单一记账不漂移。**失败文件因此不占指纹表**，下轮重解析（与上游对失败文件的行为一致，见 §3.3）。`ScanState` 不含任何库状态、线程或 runtime，序列化/持久化归 host。

### 3.6 引擎入口（s1/s10，2026-11-20 落地）

```rust
pub enum ScanEvent { Entry(Box<ScanEntry>), Failure(ScanFailure) }
// Entry 装箱：ScanEntry 是流中最大载荷，失败事件不为其付尺寸代价（clippy large_enum_variant）。

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct ScanProgress { pub discovered: usize, pub parsed: usize, pub failed: usize, pub reused: usize }
// 口径 = LocalScanStats 前四项的实时值（mod.rs:52-59）；removed 仅在结束时计入 Summary。

pub struct ScanControl<'a> {
    /// 进度回调：恒在**调用线程**上触发；回调内不得再进入本库（防重入约束，
    /// 文档明示）。共享引用 `&dyn Fn`（协变）+ Copy 快照：host 用内部可变性
    /// （Cell / Arc<AtomicUsize>）承接计数，不用捕获 &mut；整个 ScanControl
    /// 因此**可跨多次扫描复用**，scan_stream 不消费它。
    pub progress: Option<&'a dyn Fn(ScanProgress)>,
    /// 取消令牌（std-only）。取消后：不再发现新文件；已入队/已完成的解析结果仍会产出
    /// （不丢已做功）；`removed` 统计在取消轮跳过（结果集不完整，避免误报）。
    pub cancel: Option<&'a CancelToken>,
}

pub struct ScanSummary {
    pub stats: LocalScanStats,           // discovered/reused/parsed/failed/removed
    pub failures: Vec<ScanFailure>,      // 台账便利视图（低频；条目不在此重复）
    pub cancelled: bool,
    /// 本次根目录签名，**仅在完整且干净的扫描**（非取消且台账为空）才给出；
    /// 取消轮或含台账条目的扫描返回 None，因为这类 walk 没看全树，折叠其
    /// 签名会让下轮快路径跳过它从未观察到的文件（权限修复只更新 ctime 不
    /// 更新 mtime，修复后的损坏文件同理）。推荐用 `ScanState::apply_summary`
    /// 折叠：它同时会在不完整 walk 后移除已存的旧签名。
    pub dir_signature: Option<DirSignature>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CancelToken(Arc<AtomicBool>);   // std-only；Clone 共享，cancel()/is_cancelled()

pub fn scan_stream<'a>(
    root: &Path,
    options: &ScanOptions,
    previous: &'a ScanState,
    on_event: &'a mut dyn FnMut(ScanEvent),
    control: &mut ScanControl<'_>,   // 每次调用重借：ScanControl 可复用
) -> ScanSummary;

/// 便利层：collect 全部条目（host 侧内存自担；库核仍是流式）。测试与小型库用。
pub fn scan_entries(root: &Path, options: &ScanOptions, previous: &ScanState)
    -> (Vec<ScanEntry>, ScanSummary);
```

**典型增量折叠**（前态/新态分离，避免借用冲突；文档示例照此）：

```rust
let mut next = ScanState::default();
let mut control = ScanControl::default();   // 可跨轮复用
let summary = scan_stream(&root, &options, &previous, &mut |e| next.absorb(&e), &mut control);
next.apply_summary(&root, &summary);
// host 自行决定何时以 next 替换 previous；取消轮或失败轮的 next 不完整，
// 见 §6 取消语义与 §7 签名策略
```

根路径先 `canonicalize`（失败回退原值，voicefox scanner.rs:599-601 同口径）；条目路径以该根为前缀，前态 key 匹配因此稳定。`ScanState::set_dir_signature`/`apply_summary` 对 root 做同样 canonicalize，签名键与引擎查找键自动对齐（符号链接根/相对根都不会静默失效）；前态条目/指纹由 host 原样传递即可。**host 契约**：目录签名只证明根的直接子项无增删，无 watcher 的 host 必须周期性 force 或自行递归比对（§7）。

### 3.7 兼容层承诺（既有测试全绿）

`FilesystemLibrary::scan` / `read_track` / `scan_audio_files` 签名与行为**逐字保留**：一错即全败的旧语义只存在于这条旧面上（local.rs:44-91），新语义只在新核上。理由：旧接口无台账出口，"静默跳过失败"会让坏文件无声消失；需要容错的 host 迁移到 `scan_stream`。`scan_audio_files` 仍只产音频扩展名（不含 cue），`FilesystemLibrary` 不获得新方法。本批实测（2026-11-20 文档批次复跑）：all-features 738 lib + 1 integration + 1 doc passed, 0 failed（数值随套件增长，不作为固定承诺；批次报告所报 684 在交付树上未复现，复跑窄门禁数字见 LOCAL_LIBRARY_PARITY.md 修复轮证据表）。

### 3.8 诊断纯函数（src/local_scan.rs，s5，2026-11-20 落地）

全部为无库状态的纯函数：前态、缺失列表与台账由 host 持有并传入（§9），failure 的唯一生产者仍是扫描引擎。上游锚点：`mod.rs:495-512`（duplicate_groups）、`mod.rs:643-658`（duplicate_key）、`mod.rs:259-269` 与 `mod.rs:627-641`（missing 差集与跨轮保留）、`mod.rs:467-470`（corrupt_files 别名）。

```rust
pub struct DuplicateGroup { pub key: String, pub entries: Vec<ScanEntry> }
// 组内条目按 (source_path, container_track) 排序（source_path 对齐上游
// songs.sort_by(file_path)），组按 key 排序，单例组丢弃。
pub struct MissingLocalEntry { pub path: PathBuf, pub track: ScanEntry }
// path = 消失的 source_path；track = 该来源最小容器身份的代表条目
// （普通音频 None 先于 CUE "#01"/"#02"，play_path 兜底全序）。
pub fn duplicate_groups<'a>(entries: impl IntoIterator<Item = &'a ScanEntry>)
    -> Vec<DuplicateGroup>
pub fn missing_entries<'a>(retained: &[MissingLocalEntry], previous: &ScanState,
    current: impl IntoIterator<Item = &'a ScanEntry>, root: &Path)
    -> Vec<MissingLocalEntry>
pub fn corrupt_files(failures: &[ScanFailure]) -> Vec<ScanFailure>
```

- **duplicate 口径**：normalize = 空白折叠（`split_whitespace().join(" ")`，s6 起与读层共享 `normalize_text` 实现）+ 全角折半角（U+FF01..U+FF5E，s6 新增，仅键层，§3.9）+ 小写；上游逐字部分（`duplicate_key` mod.rs:643-658 的空白折叠与小写）不变。键 = title/artist/album 经 normalize 后以 `\u{1f}` 连接，尾接**精确** duration_ms（无容差，上游逐字比对 `duration.as_millis()`）。与 matcher.rs:98-103 的去符号 normalize 语义**分开实现、不混用**：matcher 会把 `song a` 与 `song-a` 折叠到一起，duplicate 键不会，有测试双向钉住（变体同组、去符号不同组）。musid 的 `LocalMetadata` 字段可空，None 归一为空串（s6 起扫描产出的条目因 L15 回填恒有名，该规则仍约束 host 手工构造的任意输入条目），两条同 duration 的无标签条目因此同组，rustdoc 明示。输入为任意条目序列：`scan_entries` 结果、`state.entries.values().flatten()` 等。
- **missing 口径**：差集 = 前态在 root 下、本轮未见且**磁盘上已不存在**的来源（mod.rs:259-269；存在性检查把"被选项变化藏住但文件还在"的条目挡在报告外）。`retained` 为 host 跨轮持有的缺失列表：本轮路径回归的记录被清除（mod.rs:636-641 的 `retain(!present)`），其余原样保留；按 path 去重保留最老记录（一张消失的多轨 CUE = 一条记录，上游 `deduplicate_missing` mod.rs:627-635 同构），结果按 path 排序。在场判定取 current 的**全局**条目集（本轮扫过的所有根）：存在性检查使该口径与上游"差集用根内集合 + 折叠用全局 present"等价，且回归路径无论本轮扫了哪个根都会清账；root 与 `set_dir_signature`/`apply_summary` 同款 canonicalize，符号链接/相对根自动对齐。取消轮不得折叠（结果集不完整，§6.3，会同时误报缺失与误清记录）。"路径回归即消失"由纯函数形态自然成立：每轮折叠后提交 next 态，回归路径下轮不再出现在差集，retained 记录被在场判定清除。
- **corrupt_files 口径**：s1 台账（`ScanSummary.failures` 或 host 跨轮台账切片）的语义化读取，逐字别名语义：同序、不过滤、不去重、零新机制（本片只定义查询面）。
- 测试：T38 至 T44（§8）。

### 3.9 元数据回填与规范化（src/local.rs + 扫描核接线，s6，2026-11-20 落地）

- **落点**：原始 lofty 读取仍是 `read_metadata_inner`（逐字原值、LoftyError 类型不变，错误分类面不受影响）；finalize = 字段规范化 + 回填，落点在其后、条目组装之前。三处消费：`read_metadata`（公开面，`TagFirst` 默认，parity L15）、新增 `read_metadata_with_policy`（显式策略，两分支皆公开可观测）、扫描核 `parse_source` 普通音频分支（`TagFirst` 默认；`ScanOptions` 不加字段，M5 核心 API 面不动）。
- **策略枚举（item c）**：`MetadataReadPolicy { TagFirst（默认）, FilenameFirst }`。`TagFirst` = 标签胜出，空缺回填：title 取文件词干（词干不可用兜底 未知歌曲，上游 `metadata.rs:86-112` + `file_stem` `metadata.rs:193-198`），artist/album 取 未知艺术家/未知专辑。`FilenameFirst` = 词干压过标签 title（词干不可用时保留标签 title），artist/album 无文件名来源、两策略同链。CUE 面优先级是容器层既有行为（上游 `scanner.rs:351-378`，formats_cue 轨表覆写基底），不受此枚举管辖，有扫描级测试钉住：带标签音频 + sheet 轨表，条目取 sheet 值、基底 spec 仍来自原始读取。
- **规范化口径（item b）**：字段统一 trim + Unicode 空白折叠（`split_whitespace().join(" ")`，与 duplicate 键共享同一实现 `normalize_text`，键口径不变）；折叠后为空的字段（空串、纯空白）归 `None` 触发回填，上游只滤 exact-empty，musid 先规范化故纯空白同样算空（差异记录在案）。大小写折叠与全角折半角只存在于比较键：`duplicate_normalize` 在共享折叠之上加 U+FF01..U+FF5E 全角折半角再小写，读层与 CUE 面的存储值只折叠空白，绝不落盘（有测试双向钉住：存储值保留全角与大小写、键折叠同组）。
- **容器适配器面保持原值**：formats_cue 的三处基底读取从 `read_metadata` 改挂 `read_metadata_inner`（原值；基底仅被 duration/spec 消费），inspect 面的 `title == None` 钉值测试原样全绿，s6 语义不进容器面。
- **duplicate 键变化**：折叠实现与读层共享后，键在空白折叠 + 小写之上新增全角折半角；既有键语义（`\u{1f}` 连接、精确 duration、不去符号、matcher 分离）逐字不变，T38/T39/T40 全绿。
- **兼容**：既有测试零改动零删除（含 formats_cue inspect 的 None 钉值、T35 夹具钉值表），fixture 断言面未触碰；`FilesystemLibrary` 与扫描核共享同一 finalize，读与扫两条路同值。

### 3.10 歌词读取（src/local.rs + 扫描核接线，s7，2026-11-20 落地）

- **组合面（主交付）**：`local::read_lyrics(path) -> Option<String>`（parity L18 组合口径，上游 provider 面 `read_local_lyric`，voicefox `source/src/local/mod.rs:822-830` 同序）：先外挂 `audio.with_extension("lrc")`，存在且 trim 非空则**逐字返回外挂内容（不 trim）**；否则取内嵌。两侧皆无可用歌词或读取失败一律 `None`（上游 `.ok().flatten()` 同款，不因坏文件失败；需要 lofty 错误面时用 `read_embedded_lyrics`）。仅小写 `lrc` 后缀（上游同）。
- **内嵌读取（parity L17）**：`local::read_embedded_lyrics(path) -> Result<Option<String>>`（上游 `read_embedded_lyric`，`metadata.rs:180-190`）：lofty 把 ID3v2 USLT、Vorbis `LYRICS`、MP4 lyrics 等统一映射到 `ItemKey::Lyrics`，`tags().iter().find_map` 取第一个 trim 非空的值并 trim；纯标签半面 `embedded_lyric_from_tag` 私有（上游同名同实现）+ 纯标签测试钉住（上游 `metadata.rs:363-372` 同款）。错误包 `Error::Provider`，与 `read_metadata` 同款。
- **外挂解码（parity L18）**：`read_sidecar_lyrics` 私有（上游 `read_text_file`，`mod.rs:838-846`）：UTF-8 优先，非法 UTF-8 降级 GB18030（GBK/GB2312 超集）；decode 不失败，双编码皆无效的字节出 U+FFFD。上游经 `tracing` 的 warn 日志在 local feature 下无 sink，降级行为改为 rustdoc 记录。
- **优先级钉住**：sidecar → embedded，与上游 `mod.rs:822-830` 同序；空白外挂落到内嵌，两边皆空 `None`。测试覆盖 ID3v2（MP3 夹具）与 Vorbis comments（FLAC 夹具）内嵌、GB18030 外挂（运行时合成字节，且 UTF-8 中文逐字往返证明 UTF-8 先行）、优先级三态。
- **扫描集成（可选开关）**：`ScanOptions.read_lyrics`（默认**关**，歌词全文不进默认扫描产物，守内存边界）+ `EXTRA_LYRICS = "lyrics"`：开启时 parse 阶段对**普通音频条目**调 `read_lyrics` 把全文放进 `extra[EXTRA_LYRICS]`，串行/并行两路共用同一 parse（`ParseJob.read_lyrics` 透传），指纹未变的复用路径从先前状态带出文本。边界两条（rustdoc 同步）：CUE 条目不带歌词（一张 sheet 摊开 N 轨共享一份按文件计时的文本，逐轨复制是 N 份重复 + 轨级时间戳错位；sheet 自身的外挂也不进扫描范围，host 自行调 `read_lyrics`）；外挂 `.lrc` 变化不移动音频指纹，增量轮会复用旧文本直到 host force。
- **写侧不重做**：`MetadataEdit.lyrics`（tag_write，s3 落地）是唯一写面；读测经 s3 原子写落签再读回（`ItemKey::Lyrics` 两侧对上）。解析本体不动（`lyrics::parse_document`）。

### 3.11 封面读取（src/local_cover.rs，s8，2026-11-20 落地）

- **feature 与依赖**：`local-cover = ["local", "dep:image", "dep:sha2"]`（Cargo.toml），沿用 ncm/local 分离先例：image（png/jpeg 解码，default-features=false）与 sha2（内容哈希）是显著编译/体积权重，不进默认 `local`（Cargo.toml:19）。运行时依赖审计 2026-11-20（docs/DEPENDENCY_AUDIT.md 补两行）：image 0.25.10（MIT OR Apache-2.0，dev 侧已有 png 面）、sha2 0.10（RustCrypto，Apache-2.0 OR MIT，纯 Rust，转译依赖 cpufeatures）。hex 不引入：std 手写小写 hex，与上游 `hex::encode` 同串（差异记录在案）。
- **内嵌提取（parity L19 读侧）**：`embedded_cover_bytes(path) -> Result<Option<Vec<u8>>, LoftyError>` = 首标签首图（`primary_tag().or_else(first_tag())`，musid 读层惯例；上游 `extract_cover` metadata.rs:201-228 用 `first_tag()` 单选，单标签文件两态等价，差异记录在案）。缓存键 `embedded_cover_path(cache_dir, audio_path, picture)` = sha256(audio 路径字节 + 图片字节) 命名 `{hash}.jpg`（上游 `embedded_cover_path` cover_cache.rs:106-114 逐字键配方；目录改为调用方传入；扩展名与上游同为占位，真实格式内容嗅探）。解码校验 `probe_dimensions`（cover_cache.rs:61-70 同语义：完整解码 + 非零宽高，失败 None）。落盘 `store_bytes(target, bytes) -> Result<(u32,u32), String>` 走 s3 公共原语 `atomic_replace`（§4.1 预告的"损坏缓存重建 = stage 重写 + 同一替换面"兑现）：tmp 先写后 probe、rename 前 decode 校验，任何失败无 tmp 残留、target 未动；缺父目录自动 create_dir_all（上游 store_bytes cover_cache.rs:81-97 同）；符号链接 target 被原语拒绝（T34 同族）。损坏缓存文件先 remove 再 store（上游 metadata.rs:214-222 同序），故坏图永不入缓存。
- **外置识别（musid-only，voicefox 无此能力，session 内 grep 复核）**：`external_cover_path(audio_path) -> Option<PathBuf>`：父目录 read_dir 一次，`EXTERNAL_COVER_NAMES`（cover/folder/front/album/albumart 各 × jpg/jpeg/png，musid 自定约定序）小写匹配（不区分大小写），目录跳过、文件与符号链接可中、不可读条目跳过；只识别不校验不拷贝（返回目录文件自身路径；host 需要时可自行调 probe_dimensions）。
- **组合面**：`read_cover(audio_path, cache_dir, priority) -> Option<PathBuf>`，`CoverPriority { EmbeddedFirst（默认）, ExternalFirst }`。EmbeddedFirst：内嵌缓存命中 → 损坏重建 → 校验落盘，store 失败降级外置（上游此处返 None，musid 无内嵌可服务时优先给目录封面，差异记录在案）；ExternalFirst 反向。外置面永不写缓存目录。返回值恒为路径，永不是图片字节。
- **解码范围限制（如实记录）**：image 只开 png/jpeg，其余容器（bmp/webp 等）probe 失败 = 视同损坏 → 不缓存 → 组合面降级外置；这是本片依赖预算的文档化限制，非正确性缺陷。
- **扫描集成（s8 item c）**：`ScanOptions.cover: CoverScanOptions { cache_dir: Option<PathBuf>, priority }`（cfg feature local-cover；默认空 = 零封面工作、零缓存写、产物零图片字节，内存边界）。配置后普通音频条目填 `extra[EXTRA_COVER_CACHE_PATH]`；CUE 条目按被引用音频去重解析一次、同 sheet 各轨共享同一路径：与 s7 歌词"轨级文本错位所以整面跳过"不同，封面路径是单一共享值，逐轨复制是精确的（对比记录在案）。复用轮沿用前态路径（与歌词同机制）。写面声明：本单元唯一写 = host 指定缓存目录内的缓存文件（§0 边界行已同步改写）。
- **越界维持**：默认缓存目录（ProjectDirs/dirs）、512 预算、sweep、remote_cache_path（远程封面缓存）一律不做（§9 原有越界声明维持）。
- 测试：T52 至 T57（§8）。

## 4. 原子写策略（src/tag_write.rs，s3，2026-11-20 落地）

### 4.1 写路径

```
target（用户音频文件）
  stage(tmp)：fs::copy(target → tmp)             // 源字节副本，失败即返，源未动
    lofty read_from_path(tmp)                    // 与旧实现同一读取面
    编辑 tag（title/artist/album/cover/lyrics；主标签不存在则按
             primary_tag_type 自动创建；不支持写标签的容器拒绝，
             语义逐字保留 download.rs:1222-1230；lofty 0.22 默认
             features 下内置 FileType 均支持各自 primary 类型，该分支
             对自然容器防御性不可达，可观测拒绝面 = read 层失败，
             T37 钉住）
    lofty save_to_path(tmp)                      // 只写临时副本
    File::options().write(true).open(tmp).sync_all()   // fsync
    回读校验：lofty 重读 tmp，断言字段落盘（title/artist/album 相等、
             lyrics 非空、pictures 非空，仅对 Some 字段断言）
  atomic_replace：rename(tmp → target)           // POSIX 原子替换
```

- 临时名：同目录 `.musid-tmp-{pid}-{seq}{原扩展名}`（手工命名，**不引 tempfile**；带扩展名保证 lofty 的扩展名探测可用；seq 为进程级 `AtomicU64`。voicefox cover_cache.rs:49-77 手工 `.part` 先例）。
- 回读校验在 **rename 之前**对 tmp 执行：任何失败（含校验失败）则删除 tmp、返回 Err，源文件字节未变。rename 后内容与 tmp 逐字节一致，无需二次校验。
- 平台语义（硬边界"平台允许时"）：POSIX rename 原子替换；Windows `std::fs::rename`（MoveFileExW + MOVEFILE_REPLACE_EXISTING）同样覆盖已存在文件，但目标被其它进程打开时拒绝，此时回退 `remove_file(target)` + rename（一次重试覆盖瞬时错误）。**tmp 删除的精确不变量**：只在"首次 rename 失败且 target 不存在"时删除 tmp（此刻 target 从未被触碰）；一旦进入回退分支，staged 副本是 target 移除后的唯一幸存内容，该分支任何失败都**保留 staged 副本并在错误信息中给出其路径**，绝不删除（否则 target 与副本同失 = 内容彻底丢失）。进入回退本身的前置条件是 staged 副本存在（副本已丢失时不删 target）。附带后果：POSIX 上单纯 EACCES（目录只读）会让首次 rename 失败、target 仍在、tmp 保留 = 用户音乐目录留下一个 `.musid-tmp-*`，路径写在错误信息里，内容可恢复，可接受。
- durability 边界：rename 后**不**对所在目录 fsync，承诺是"进程内可见的失败不损源"，不是掉电级的崩溃安全；需要崩溃持久性的 host 在写成功后自行 fsync 目录（POSIX 可行，Windows 无对应 std 面）。
- `pub fn atomic_replace<T>(target: &Path, stage: impl FnOnce(&Path) -> Result<T, String>) -> Result<T, String>` 为公共原语，s8 封面缓存直接复用（损坏缓存重建 = stage 重写 + 同一替换面）。原语入口与 write_metadata 同样**拒绝符号链接 target**（staged replace 会把链接顶替成常规文件）。
- `write_metadata(path, edit) -> Result<(), String>` 签名不变；`MetadataEdit` 字段不变；cover 构造失败（损坏图片字节）在 stage 内先于任何落盘发生。

### 4.2 与旧行为的语义差异（共两处）

写成功后源文件 mtime/内容变化 = 预期；写失败后源文件与旧实现不同：**保证未变**（旧实现 lofty 直写，中途失败可能留下半写文件，这是本片修复的核心缺陷）。

第二处：**符号链接拒绝写**。旧实现 lofty save_to_path 直写路径，恰好穿透链接改写目标；新实现 fs::copy 跟随链接取内容、rename 会把链接无声替换成常规文件，破坏用户的链接布局。因此 `write_metadata` 与公共原语 `atomic_replace` 对符号链接 target 都直接返回 Err（提示先解析链接再操作目标），链接与目标字节都不动。

## 5. 事件抽象

### 5.1 本批落地的事件面

- `ScanEvent::{Entry, Failure}`：扫描产出流（§3.6）；
- `ScanProgress`：计数快照回调（§3.6），与 `LocalScanStats` 同口径；
- `CancelToken`：std-only 协作取消（§6.3）。
- `WatchEvent`（s9）：已于 2026-11-20 落地（`src/local_watch.rs`，feature `local-watch`），见 §5.2。

### 5.2 WatchEvent（s9，2026-11-20 落地：src/local_watch.rs，feature local-watch）

```
feature local-watch = ["local", "dep:notify"]。notify 8.2（resolved 8.2.2，CC0-1.0，rust-version 1.88，
上游 voicefox 同 pin "notify = \"8.2\""，voicefox/Cargo.toml:84）；依赖审计见
docs/DEPENDENCY_AUDIT.md 2026-11-20 条目。notify 拉平台 FFI（inotify/FSEvents/ReadDirectoryChangesW）
加 notify 自身事件线程，批量扫描 host 不应支付，与 ncm/local-cover 同一分离理由。

API 面（src/local_watch.rs）：
- WatchEvent { path: PathBuf, kind: WatchEventKind }；
  WatchEventKind = Created | Modified | Removed | Renamed | Other
- WatchOptions { debounce: Duration }：默认 250ms = MIN_DEBOUNCE，低于下限钳到 250ms（mod.rs:558）
- WatchSink：Callback(Box<dyn FnMut(WatchEvent) + Send>) 或
  Channel(std::sync::mpsc::Sender<WatchEvent>)，调用方任选；零 tokio/runtime（硬边界：本模块只用
  std::thread + std::sync::mpsc，notify 默认 features 的内部通道同为 std mpsc，notify lib.rs:170-183）
- watch_recursive(roots, options, sink) -> Result<StopHandle, String>；
  watch_recursive_channel(roots, options) -> Result<(mpsc::Receiver<WatchEvent>, StopHandle), String>
- StopHandle：Drop 即置 stop + wake + join（mod.rs:61-75 语义）；join 完成后 channel 面确定断开，
  即"已全停"的可观测信号；drop 前已交付的事件留在通道缓冲可继续排空

语义（对上游 mod.rs:528-616 的重造）：
- 过滤：Access(Close(Read))/Open/Read 非破坏访问事件不上报（is_non_mutating_access 逐字移植
  mod.rs:619-627）。Linux 实证：notify inotify 监视掩码为 ATTRIB|CREATE|OPEN|DELETE|CLOSE_WRITE|
  MODIFY|MOVED_FROM|MOVED_TO，不含 ACCESS/CLOSE_NOWRITE（notify inotify.rs:427-434），故 Linux
  可观测的被滤形态只有 Open（如 read_dir 打开被监视目录）；Close(Read)/Read 形态对其它平台有效，
  过滤纯函数的三个形态在测试中全钉（不依赖平台）。
- 映射：Create → Created；Modify(Name) → Renamed（inotify MOVED_FROM/TO：一次 rename 产旧路与
  新路两条）；Modify 其它 → Modified；Remove → Removed；Access 幸存形态（Linux 每次写文件后的
  Close(Write)）、Other、Any → Other；notify 后端错误 → 一条 Other 证据事件（error.paths 首路或
  空路），不无声丢弃：证据可能在错误后缺失，host 与其它事件同样反应。
- 去抖：静默窗合并。事件流静默一个窗口后整批交付；窗内同路径首个 kind 胜出、按首见序交付
  （create 后紧跟 edit 的 burst 呈现为 Created；Linux Close(Write) 尾随事件被吸收，不掩盖信息
  kind）。批上限 COALESCE_MAX_PATHS = 256 条不同路径或批龄 8 个窗口先交付：持续 churn 不会饿死
  交付、批不会无界膨胀；被上限切在边界上的路径可能在相邻两批各出现一次（每窗一事件语义的自然
  结果，有测试钉住 1 或 2 次/路径的界）。这是对上游的收口加强：上游只排空已入队积压并逐事件
  重扫（mod.rs:575-577）。
- 已知实证细节：增删被监视目录的直接子项会移动目录自身 mtime，dir watch 报 IN_ATTRIB，即根路径
  一条 Modified 事件与条目自身事件并存；根事件同样是真实变化证据。
- watch 边界：非目录 root 静默跳过（上游 mod.rs:545-555 同）；空 roots 返回惰性 StopHandle、
  不起线程；事件路径派生自 watch root 原形，host 应以与扫描相同的规范根形态 watch（ScanState
  签名键由引擎侧 canonicalize，§7）。
```

**与扫描核的接缝**：`WatchEvent` 是"已知变化证据"；host 触发重扫必须 `ScanOptions::force = true` 绕过目录签名快路径（目录 mtime 不反映原地修改，mod.rs:592-596；`ScanOptions::force` rustdoc 与 §7 同口径）。**不做**（越界，§9）：库内自动重扫、watch_generation、线程持有库状态；`StopHandle` 只持 stop/wake/join，无 `Arc<LocalSource>` 弱引用。

## 6. 取消与并发模型（s10，2026-11-20 落地）

### 6.1 发现/解析两级流水线

- **发现级**（恒单线程惰性）：调用线程上 walkdir 逐条产出；逐文件做指纹 stat + 复用判定（命中复用立即产出，不进解析级）；目录排除/隐藏/深度/符号链接在此级生效。walkdir 惰性 + 逐条产出 = 大库不一次性全量进内存（边界由发现级保证，与 worker 数无关）。
- **解析级**：`max_parse_workers = 1` 时在调用线程内联解析（**零线程**，等价现行为）；>1 时 `std::thread::scope` 起 N worker，从共享有界队列取源文件路径解析，结果经 channel 回主线程。**两条路径共用同一 panic 防护**（parse_guarded/catch_unwind）：lofty 对畸形输入的 panic 记入台账（kind=Parse），不会穿透 scan_stream 杀死调用方线程；worker 侧的队列锁免毒（poisoned guard 一律 into_inner 恢复），主线程持有原始 sender 在 scope 内即 drop，因此"全部 worker 死亡时 recv 返回 Err"是真实可达的兜底分支而非死代码。

### 6.2 内存形态

- 工作队列只存**源文件路径**（无字节、无元数据），容量 = `4 × workers`（下限 8）；主线程在队列饱和时先排空结果再入队，无死锁。
- worker 只持单文件的解析缓冲（lofty 读取面）；结果通道无界但主线程持续排空，积压上限 ≈ in-flight（队列容量 + worker 数）。
- 重排序缓冲（ordered_output=true）按发现序号排空，深度不超过 in-flight。
- 库核自身不累积条目：条目经 `on_event` 交给调用方；`ScanSummary` 只含统计 + 失败台账。

### 6.3 取消语义

- `CancelToken` = `Arc<AtomicBool>`，`cancel()` 置位；发现级逐文件检查，worker 逐任务检查。
- 取消后：不再发现新文件；**已入队任务不丢弃而由 worker 快速吞掉**（置位即跳过解析、回报 Cancelled 结果），in-flight 记账精确、无死锁；已完成的解析结果全部产出（不丢已做功）。
- 取消检查覆盖全部产出面：发现级逐文件、worker 逐任务、**目录签名快路径逐条目**（快路径整批发射前态条目，循环内也检查令牌，取消即停，不豁免）。
- `summary.cancelled = true`；`stats.removed` 在取消轮为 0（结果集不完整，避免把"没扫到"误报成"被删"）；`stats` 其余字段反映已完成部分；`dir_signature` 在取消轮为 None（部分产出不给签名）。
- 取消轮的折叠态不完整：host 不应以取消轮的 `next` 整体替换前态（或自行与前态合并未扫子树），文档明示。

### 6.4 并发约束

- 进度/条目回调恒在**调用线程**触发（worker 只与通道交互），回调内禁止再进入本库（防重入/死锁，文档明示）。
- `max_parse_workers > 1` 时产出保序（发现序）可选：`ordered_output`；false 时完成序产出，测试保证两模式条目集合一致。

## 7. 增量扫描规则（s4 汇总，2026-11-20 落地）

对每个发现的媒体文件（音频 + .cue；发现 = 指纹 stat 成功）：

1. `FileFingerprint::from_path` 失败则进台账（Permission/Io），继续；
2. 前态指纹相同 **且** 前态有该来源的条目：克隆前态条目产出（`reused=true`），`stats.reused += 条目数`；不解析标签；
3. 否则解析：`.cue` 走 `CueHandler::inspect_tracks` + 基底读取（§3.4）；音频走 `read_metadata_inner`；产出新条目（`reused=false`），`stats.parsed += 1`（文件数口径，scanner.rs:216 同）；失败进台账，`stats.failed += 1`；
4. `stats.discovered` = 指纹 stat 成功的媒体文件数（含后来解析失败者，scanner.rs:143-147 口径，含 .cue）；
5. **removed**（仅非取消轮）：前态在本根下的来源数减去本轮见到的其中之数。口径差异如实记录：上游为全局指纹表差值（mod.rs:304-310），musid 为逐根切片差值，单根等价、多根求和等价。

**目录签名快路径**（根级，上游 mod.rs:200-217 同级）：`!force && 前态签名(root) == DirSignature::from_path(root) && 前态在本根下有条目`，则全部前态条目按路径序产出（`reused=true`，逐条计入 reused），跳过全树遍历；不成立则全树惰性遍历 + 逐文件指纹复用。**签名只证明根的直接子项无增删**（§3.2）：已存在子目录内部的增删只移动子目录自身的 mtime，根签名不变，快路径整树跳过、新文件在根的直接子项变动或 force 之前不可见（有钉行为测试）。**host 契约**：本批无 watcher（s9 未来批），没有递归 watcher 的 host 必须周期性 force 或自行递归比对；上游的"无变化证据"由递归 watcher 兜底，musid 暂无该兜底，故此契约必须显式化。**签名折叠策略（对上游的收口）**：引擎只在完整且干净的扫描（非取消、台账为空）给出 `ScanSummary.dir_signature`；`ScanState::apply_summary` 在取消轮/失败轮会移除已存签名。动机：chmod 修复权限只更新 ctime 不更新 mtime，修复后的损坏文件同理，若失败轮照记签名，下轮快路径会让恢复的文件在树内发生增删前一直不可见（上游 mod.rs:271-274 无条件记签名 + force 才能救，musid 因把 walker 错误升级为记账而更易触达，故在此收口）。签名键由 `set_dir_signature`/`apply_summary` 按 scan_stream 同款 canonicalize 规范化，符号链接/相对根不会静默失效（有测试：经符号链接根扫描 + apply_summary 后，未变更树上快路径命中、discovered=0）。两轮间改变 skip_hidden/excluded_dirs/max_depth/symlinks 不会使快路径失效，改选项需配 force 或按选项键控签名。**已知窗口（照抄上游）**：签名粒度 = mtime 纳秒，同纳秒双写理论可漏检；文件级指纹兜底所有非快路径。

## 8. 测试矩阵（全部临时目录 + 运行时合成音频，无真实用户目录）

| # | 测试 | 覆盖 |
|---|---|---|
| T1 | 排除目录按名剪枝 + 调用方追加 | s1 L3 |
| T2 | skip_hidden 两分支（默认 false 保持现行为） | s1 |
| T3 | max_depth=1 与 0=不限 | s1 L1 |
| T4 | symlink Skip（不进扫描）+ Follow（进扫描，unix 门） | s1 |
| T5 | chmod 000 文件：进台账 Permission，同目录其余文件照常产出（unix 门） | s1 L8 |
| T6 | 坏字节 .mp3 进台账 Io、坏魔数 .flac 进台账 Parse（实证分类），均不中止 | s1 L8 |
| T7 | chmod 000 子目录：walker 错误进台账（unix 门） | s1 |
| T8 | 流式形态：事件逐条到达、summary 不含条目、串/并结果一致 | s1/s10 |
| T9 | CUE 条目双路径/轨身份/cue_* extras/基底 spec | s2 L21/L24 |
| T10 | CUE 引用音频缺失：进台账不中止 | s2 |
| T11 | 二次扫描全复用（parsed=0），条目相等 | s4 L4 |
| T12 | 原地改（同尺寸重写）：指纹失配重解析 | s4 L5 |
| T13 | 目录签名快路径吞掉原地改；force 绕过后重解析（stale 变新） | s4 L6 |
| T14 | 删除文件：removed=1、折叠态不再含该条 | s4 L10 |
| T15 | CUE 一 key 多轨整体复用、与整轨音频不互相顶替 | s4 |
| T16 | 统计口径（discovered 含 cue/失败文件；failed 计数） | s4 L10 |
| T17 | 取消前置于扫描：0 条目 + cancelled；回调内取消：部分 + cancelled + removed=0 | s10 |
| T18 | progress 计数终值 == stats | s10 |
| T19 | workers=1 与 4（ordered）逐条相等；unordered 集合相等；大合成树（500 文件嵌套目录）内存形态（§6.2）：逐条单事件、summary 无条目、N=4 ordered 与串行逐字相等、unordered 集合相等 | s10 |
| T20 | 标签成功路径 + 回读校验 + 无 tmp 残留 | s3 |
| T21 | 损坏图片字节：Err 且源字节不变、无 tmp 残留 | s3 |
| T22 | atomic_replace stage 失败/成功两分支 | s3 |
| T23 | promote 回退失败保留 staged 副本（目录目标构造，错误含副本路径）；staged 缺失时不进入破坏性回退、target 不动 | s3 |
| T24 | 符号链接拒绝写：链接不被顶替、目标字节不变（unix 门） | s3 |
| T25 | 串行与并行解析共用 panic 台账兜底（parse_guarded 直测） | s10 |
| T26 | 快路径发射中取消：部分 + cancelled + 无签名 | s10 |
| T27 | 失败 walk 不给签名且 apply_summary 移除旧签名；权限修复后文件可见（unix 门） | s4 L6 |
| T28 | apply_summary 三分支（干净/失败/取消） | s4 |
| T29 | 同一 ScanControl 跨两轮扫描复用（进度回调持续工作） | s10 |
| T30 | Follow：一实体 N 链接产 N 条目；循环链接进台账不挂起（unix 门） | s1 |
| T31 | 已存在子目录内加文件：根签名不变、快路径漏检（钉行为）；force 后可见（host 契约） | s4 L6 |
| T32 | 符号链接根扫描 + apply_summary：键规范化，未变更树上快路径命中（discovered=0） | s4 L6 |
| T33 | 快路径取消：多轨组内取消逐条计数（reused == 已发射数） | s10 |
| T34 | atomic_replace 原语拒绝符号链接 target（unix 门） | s3 |
| T35 | 提交夹具语料全扫（tests/fixtures/cue 8 张 sheet：6 张可扫 sheet 经扫描管线复现 formats_cue 钉值表，含 utf8 转义与序号回退、gbk GB18030、legacy 逐字 token/INDEX 00 忽略/词干专辑/未知艺术家、multi_file 逐轨 play_path、catalog 两路解析器轨号；双路径/轨身份/cue_* extras/基底 spec/一 key 多轨分组；音频为运行时合成 stub） | s2 L21 |
| T36 | 提交夹具语料的失败两张 sheet（index_seconds_out_of_range 严格 INDEX 语法、missing_audio 引用音频缺失）：入台账 kind=Parse、错误文案含适配器钉值、扫描继续 | s2 L8 |
| T37 | 不支持容器拒绝语义保持：非音频字节入音频扩展名 → read 层 Err（failed to read tags）、源字节不变、无 tmp 残留（实证 lofty 0.22.4 默认 features：内置 FileType 均支持 primary 类型，"does not support tag writing" 分支逐字保留、防御性不可达） | s3 |
| T38 | duplicate_groups 纯函数：大小写/空白变体同组、精确 duration 分组、不同题名单例丢弃、键形逐字（\u{1f} 连接 + duration_ms）、组内按 (source_path, container_track) 排序、ScanState 迭代输入等价 | s5 L12 |
| T39 | duplicate 键不去符号：`song a` 与 `song-a` 不同组（与 matcher.rs:98-103 的去符号 normalize 分开，钉住）；无标签字段归空串、同 duration 无标签条目同组 | s5 |
| T40 | 真实扫描重复组：标签写回的 WAV（同曲名大小写/空白变体 + 不同时长 + 不同曲名）经扫描管线分组，键与组内序实证 | s5 L12 |
| T41 | missing：删除后上报（path + 代表条目）；仅从本轮结果缺席但文件存在（选项变化形态）不上报；真实重扫同口径单记录 | s5 L9 |
| T42 | missing CUE 收敛：一张消失的双轨 CUE = 一条记录，代表条目 = 最小容器身份（#01） | s5 L9 |
| T43 | missing 跨轮保留与回归清除：本轮未扫到该路径的轮保留记录、路径回归（重扫见该路径）清除、折叠后稳定为空 | s5 L9 |
| T44 | corrupt_files 视图：台账逐字透传（同序、等值）、空台账空视图，零过滤零去重 | s5 L8 |
| T45 | 读层回填（parity L15）：无标签文件（运行时 WAV stub）title=词干、artist/album=未知艺术家/未知专辑，audio/format 不受影响；经 write_metadata 落签后空串与纯空白字段同样回填 | s6 |
| T46 | 策略枚举两分支（s6 item c）：TagFirst 取标签 title，FilenameFirst 取词干（artist/album 两策略同链）；词干不存在（".."）时 TagFirst 兜底 未知歌曲、FilenameFirst 保留标签 title | s6 |
| T47 | 规范化不落盘折叠：全角字母与大小写存储逐字保留、空白（含全角空格）折叠生效（读层测试）；duplicate 键全角变体与半角变体同组且键为折叠形（扫描管线） | s6 |
| T48 | 扫描级回填与 CUE 面优先级：scan_entries 未标签音频 title=词干、未知*；带标签音频 + sheet 轨表 → 条目取 sheet 值（空白折叠后），基底 spec 仍来自原始读取 | s6/s2 |
| T49 | normalize_text 纯函数分支：None、空串、纯空白、多空白合并、全角空格折叠为存储空白 | s6 |
| T50 | 歌词读取（s7 L17/L18）：ID3v2（MP3 夹具）与 Vorbis comments（FLAC 夹具）内嵌读取 + 纯标签半面 trim/空过滤；GB18030 外挂降级（运行时合成字节）且 UTF-8 先行（UTF-8 中文逐字往返、GB18030 字节经降级解码）；优先级三态：非空外挂逐字胜出、空白外挂落内嵌、两侧皆无 None | s7 |
| T51 | 扫描集成（s7 §3.10）：read_lyrics 默认关（默认产物无 lyrics 键）、开启后普通音频条目 extra[EXTRA_LYRICS] 有全文、指纹未变轮复用路径保留文本；GB18030 外挂经扫描解码、CUE 条目不带 lyrics 键 | s7 |
| T52 | 缓存键纯函数（s8）：路径+图片字节内容寻址（同输入同键、任一变化换键）、64 hex + `.jpg` 占位、content_hash 稳定且全 hex | s8 L19 |
| T53 | 内嵌提取→缓存→命中：host 指定目录落盘、probe 尺寸钉值 (8,4)、二次调用同路径、缓存目录恰 1 文件无 tmp；扫描面与独立 read_cover 面同路径 | s8 L19 |
| T54 | 损坏缓存文件重建（先 remove 再 store、rebuilt 同键、无 tmp 残留）；损坏内嵌字节不入缓存（PNG magic 截断 33 字节经 write_metadata 落签，metadata.rs:434-446 同口径，缓存目录零文件）；store_bytes 校验先行（corrupt 拒且零残留、已有有效目标不被坏写顶替） | s8 L19 |
| T55 | 外置名探测：约定序（cover 优于 folder）、大小写不敏感（FOLDER.PNG 命中且保留原名）、未列名（back.jpg）不中、外置面零缓存写 | s8 |
| T56 | 优先级可配置（EmbeddedFirst 缓存路径胜出 / ExternalFirst 目录文件胜出）；缓存目录故障（chmod 500，unix 门）降级外置、无外置时 None；store_bytes 拒符号链接 target（unix 门，T34 同族） | s8 |
| T57 | 扫描槽（s8 item c）：默认产物无 cover 键（内存边界）；开启后普通音频 extra[EXTRA_COVER_CACHE_PATH]=缓存路径（probe 钉值）；CUE 两轨共享一路径、普通 wav 条目同值；复用轮保留路径；默认选项下 CUE 产物同样无键 | s8 item c |
| T58 | watcher 通道面（s9）：create/modify/remove 经真实 inotify 到达且 kind 映射正确；rename 产旧路+新路两条 Renamed | s9 L7 |
| T59 | 访问过滤（s9）：纯函数三形态（Close(Read)/Open/Read 滤、Close(Write)/Modify 不滤）+ 携路径的 Open 事件映射为零；live 面：read_dir + read 后流静默且监听器仍存活 | s9 L7 |
| T60 | 去抖合并（s9）：同文件 5 连写在 250ms 窗内合并为恰 1 条 Modified；事件 kind 映射表全枚举（含 Renamed 先于 Modify 臂序） | s9 |
| T61 | StopHandle Drop 停止（s9）：先证活性（事件到达），drop 后通道确定断开（join 完成的可观测信号），drop 后的写入不再产出事件 | s9 |
| T62 | 回调面（s9）：WatchSink::Callback 收到与通道面相同的事件（调用方任选两交付面） | s9 |
| T63 | kind 映射表 + Ok 事件按路径扇出 + 空路径表零事件（纯函数） | s9 |
| T64 | 去抖下限钳制（纯函数）：0/100ms 钳到 250ms、2s 透传、默认即 250ms（mod.rs:558） | s9 |
| T65 | 后端错误证据面（纯函数）：Err → 一条 Other 事件（error.paths 首路或空路），不无声丢弃 | s9 |
| T66 | 空 roots 惰性句柄：Ok 且 Drop 无挂起无 panic（上游 mod.rs:540-542 早退同） | s9 |
| T67 | 非目录 root 静默跳过（上游 mod.rs:545-555 同）：watch 成功、周边变化零事件 | s9 |
| T68 | 合并上限（s9）：300 文件连发跨越 256 上限分批交付，全路径不丢、每路径 1 或 2 次（上限切边界的跨批重复钉界）、无风暴 | s9 |

门禁命令（外层跑全量；本 ask 自跑窄命令见交付报告）：`cargo test --locked --features local-watch` 与 `cargo clippy --locked --all-targets --features local-watch -- -D warnings`（s9 字面门禁，`local-watch` 为门禁 feature）、`cargo test --locked --features local-cover` 与 `cargo clippy --locked --all-targets --features local-cover -- -D warnings`（s8 字面门禁，`local-cover` 为门禁 feature）、`cargo test --locked --features cfg` 与 `cargo clippy --locked --all-targets --features cfg -- -D warnings`（s3 字面门禁，`cfg` 为门禁 feature）、`cargo test --locked --features local local::`、`... local_scan::`、`... local_watch::`、`... --no-default-features --features "download,lyrics" download::`、`... --no-default-features --features download tag_write::`、`... --features "ncm,local" formats_`、`--no-default-features` 边界、`--all-features`、`clippy --all-targets --all-features -- -D warnings`、`cargo fmt --all`。

## 9. 越界声明（按硬边界剔除，不做）

内存库/scan_generation/stale 保护、remove_by_path、edit_metadata 内存刷新、MusicSource 搜索面（parity L23）、cover 缓存目录策略与 512 预算/sweep、gitignore 规则、库内自动重扫、watch_generation、tokio/runtime 化、真实音乐目录测试。前态（条目/指纹/签名/缺失列表）一律由 host 持有并传入。**CUE 引用路径包含性**：play_path 解析与上游逐字同构（cue.parent().join(FILE 名).canonicalize()），恶意或手误 sheet 可用 `../` 或绝对 FILE 名让 play_path 指向扫描根之外（元数据会照读、路径会交给播放器）；校验与沙盒归 host，本库不越界添加路径审查。

## 10. 未来切片接口预留核对

- s5（诊断）：已于 2026-11-20 落地（§3.8）：`duplicate_groups`/`missing_entries`/`corrupt_files` 全部为无库状态纯函数，输入为任意条目序列与 host 持有的前态/缺失列表/台账（比原计划的 `&ScanState` 绑定更宽，纯函数形态不变）；去符号 normalize（matcher.rs:98-103 语义）与空白折叠 duplicate key 分开实现并有测试钉住。
- s6（回填）：已于 2026-11-20 落地（§3.9）：`read_metadata` / `read_metadata_with_policy`（策略枚举 `TagFirst` 默认）与扫描核普通分支共用同一 finalize（落点 = `read_metadata_inner` 之后、条目组装之前）；容器适配器面（formats_cue）挂原始读取保持 inspect 钉值；字段 trim + 空白折叠与 duplicate 键共享实现，全角/大小写折叠仅在比较键；fixture 断言面零触碰。
- s7（歌词）：已于 2026-11-20 落地（§3.10）：`local::read_lyrics`（sidecar 优先 + GB18030 降级，Option 口径与上游组合面同序同滤）+ `local::read_embedded_lyrics`（`ItemKey::Lyrics` 跨标签 find_map + trim + 空过滤）+ `ScanOptions.read_lyrics` 开关（默认关）与 `EXTRA_LYRICS`；写侧 s3 不重做，解析本体 `lyrics::parse_document` 不动。
- s8（封面）：已于 2026-11-20 落地（§3.11）：`src/local_cover.rs`（feature local-cover，蕴含 local）交付 `embedded_cover_bytes`（首标签首图）+ `embedded_cover_path`（音频路径+图片字节 sha256 键）+ `probe_dimensions`（完整解码校验）+ `store_bytes`（经 s3 `atomic_replace` 原语，损坏重建同面）+ `external_cover_path`（同目录约定名探测，musid-only）+ `read_cover`（优先级可配置组合面）+ `CoverScanOptions` 扫描槽（默认空 = 零封面工作零图片字节）；缓存目录策略归 host，越界维持。依赖 image 0.25/sha2 0.10 已审计（docs/DEPENDENCY_AUDIT.md）。
- s9（watcher）：已于 2026-11-20 落地（§5.2）：`src/local_watch.rs`（feature `local-watch` = `local` + `dep:notify`，notify 8.2.2 已审计 docs/DEPENDENCY_AUDIT.md 2026-11-20 条目）交付 `WatchEvent`/`WatchEventKind` 映射（非破坏访问过滤逐字移植 mod.rs:619-627）、`WatchSink` 双交付面（回调或 std::sync::mpsc，零 tokio/runtime）、静默窗去抖（250ms 下限可配 + 256 路径/8 窗批上限）、`StopHandle` Drop 即停即 join（mod.rs:61-75 语义）；越界维持：无库内自动重扫、无 watch_generation、无线程持有库状态；host 以 WatchEvent 重扫必须 `ScanOptions::force = true`（§7）。
- s10（取消/进度/并发）：已于 2026-11-20 落地（§6）：`ScanControl{progress, cancel}` 每次调用重借、`CancelToken` std-only（Clone 共享），取消后不再发现新文件、已入队任务由 worker 快速吞掉、已产出不丢、取消轮 `removed=0` 且不给签名；评审 nit 修复：目录签名快路径发射改为逐条 `stats.reused += 1`，多轨 CUE 组内取消时已发射条目如实计入（测试 T33：cue 双轨 + wav 组成 3 条目、路径序下组内第一条后取消时 entries_seen == summary.stats.reused == 1，修复前为 0）。
