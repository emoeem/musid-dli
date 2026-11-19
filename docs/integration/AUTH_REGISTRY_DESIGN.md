# AUTH_REGISTRY_DESIGN.md: M2 core (unified error taxonomy, auth state machine, multi-account session model, registry query surface)

M2 核心切片（`error-taxonomy`、`auth-session`、`registry-capabilities`）的设计与落地记录。
目标语义基准：`docs/tasks.md` §5.4（Registry 完善）、§5.5（统一错误体系）、§10.2 至 10.6
（认证方式 / Auth 抽象 / Session 管理 / 安全要求 / 验收）、§15.2（最小化修改原则）。
上游基线：M0 `FEATURE_DEPENDENCY_MATRIX.md`（feature/依赖边界）、M1 `TRANSPORT_DESIGN.md`
（§1 provider 侧保留平台签名与 cookie 白名单；§6 download 引擎边界；§7 九 provider 迁移序）。
盘点结论（error.rs:6-21、session.rs:8-52、traits.rs:126-168、registry.rs:10-139 等）来自本
campaign 的实读/实跑记录，本设计逐条对应。§9 是评审修订记录（第一轮评审 10 条 + 第二轮评审 6 条，逐条落点）。

**落地状态（2026-11-19）：本设计覆盖的 M2 十二切片全部已落地并通过门禁**：三个核心层
切片（error-taxonomy、auth-session、registry-capabilities）加九个 provider/桥迁移切片
（netease、qqmusic、kugou、kuwo、bilibili、migu、轻认证三源、download 桥、lx-js 桥）；
第二轮评审遗留 6 条（§9.1 #11-16）已逐条修复并全绿重交。逐切片证据为各自 feature 域的
test/clippy 通过（汇总见 docs/IMPLEMENTATION_STATUS.md 的 M2 验证条目），逐切片落地记录
见 §9.2 至 §9.8；全量门禁（all-features/MSRV/三平台）记录由外层脚本追加。

M2 核心 = 三个核心层切片；九个 provider 的错误迁移与 AuthState 接线是后续独立切片
（§8 迁移序），本切片对它们零改动（唯一的例外：`ProviderSession` 加字段迫使四个结构体
字面量补 `..Default::default()`，见 §3.2 兼容记录）。

## 1. 统一错误体系（error.rs）

### 1.1 类别枚举与映射矩阵

`ErrorCategory`（`#[non_exhaustive]`，源 `src/error.rs`）是 24 个稳定类别。`Error::category()`
是全变体穷举的纯函数映射（编译器强制总函数；crate 内 match 不受 `#[non_exhaustive]` 影响）。
矩阵（变体 → 类别 / 可重试 / `provider_id()`）由 `error.rs` 的
`error_mapping_matrix_is_total_and_classified` 测试逐行钉死：

| `Error` 变体 | `ErrorCategory` | `is_retryable` | 备注 |
|---|---|---|---|
| `Provider { provider, message }`（旧） | `Provider` | false | 保留的 catch-all（未分类兜底），Display 逐字节不变 |
| `Unsupported(String)`（旧） | `Unsupported` | false | Display 逐字节不变；`provider_id()` 返回 None，理由见 §1.3 |
| `NotFound`（旧） | `NotFound` | false | 同上 |
| `Timeout`（旧） | `Timeout` | **true** | 旧变体留用（盘点：当前无构造点，死变体不删） |
| `InvalidInput(String)`（旧） | `InvalidInput` | false | 同上 |
| `Serialization(serde_json::Error)`（旧） | `Serialization` | false | `#[from]` 保留 |
| `Io(io::Error)`（旧） | `Io` | false | `#[from]` 保留 |
| `InvalidResponse { provider, code, detail }` 新 | `InvalidResponse` | false | 上游响应违反契约（缺字段/码表外/不可解析） |
| `Network { provider, detail }` 新 | `Network` | **true** | 连接/DNS/TLS 层失败 |
| `RateLimited { provider, retry_after, detail }` 新 | `RateLimited` | **true** | 429/平台限流码（netease 405）；`retry_after: Option<Duration>` |
| `Unavailable { provider, detail }` 新 | `Unavailable` | **true** | 5xx/维护/网关不可用 |
| `ExpiredResource { provider, detail }` 新 | `ExpiredResource` | false | 资源曾存在但已失效/下架（不等于 NotFound） |
| `AuthenticationRequired { provider, detail }` 新 | `AuthenticationRequired` | false | 需登录（kuwo 现文案 "login required"） |
| `AuthenticationFailed { provider, code, detail }` 新 | `AuthenticationFailed` | false | 凭据被拒（QR 403、密码错、签名拒） |
| `SessionExpired { provider, detail }` 新 | `SessionExpired` | false | 过期标记落点；Display 正文 = "login required or session expired"（netease/kugou/qq 现文案，source_netease.rs:227） |
| `PermissionDenied { provider, code, detail }` 新 | `PermissionDenied` | false | 服务端权限拦截（含风控 -412 穷尽后） |
| `VipRequired { provider, detail }` 新 | `VipRequired` | false | 需会员（source_qqmusic.rs:714-718 prose 的升级落点） |
| `RegionRestricted { provider, detail }` 新 | `RegionRestricted` | false | 地区限制（migu sid=USS 类） |
| `CopyrightRestricted { provider, detail }` 新 | `CopyrightRestricted` | false | 版权拦截（migu isCopyright 类） |
| `UnsupportedQuality { provider, quality, detail }` 新 | `UnsupportedQuality` | false | 请求音质不受支持（`quality: StreamQuality`） |
| `UnsupportedOperation { provider, detail }` 新 | `UnsupportedOperation` | false | 当前版本不支持的操作（bilibili 5 处 Unsupported 站点的目标类别） |
| `ProviderInternal { provider, detail }` 新 | `ProviderInternal` | false | provider 自身内部错误（JS 沙箱/下载校验类，后续切片消费） |
| `Protocol { provider, detail }` 新 | `Protocol` | false | 与上游的协议层错误（M1 transport / LX 桥契约层） |
| `Cancelled` 新（无载荷） | `Cancelled` | false | 宿主/用户取消 |

除 `Cancelled` 外，全部 M2 变体带可选 `detail: Option<ErrorDetail>`（§1.2）。

可重试性定义与两个既有先例对齐：

- `Network | Timeout | RateLimited | Unavailable` 为 true，其余 false。这与
  `retry.rs:68-80 classify_http`（429/5xx 重试）同族；
- `SessionExpired` **不**可盲重试：`retry.rs:15-24 StepOutcome::SessionExpired` 语义是
  "标记过期并终止本次调用"，恢复路径是 `refresh_session`/重登录（§2.4），不是 resend；
- `DownloadError::is_retryable`（download.rs:126-138）是引擎策略族（`RangesUnsupported`
  是策略切换、408 入重试集），不并入本表；桥接见 §8 切片 8（已落地，见 §9.7）。

### 1.2 底层信息保留与脱敏不变量（tasks.md 5.5 / 10.5）

tasks.md §5.5 明文要求"错误应保留底层错误信息，但不应将 Cookie、Token 或签名参数暴露
出来"。两个诉求分别落在两类槽位上：

- **结构化载荷**：provider id（注册表标识位）、平台 API 码（`code: Option<i64>`）、
  Retry-After（`retry_after: Option<Duration>`）、音质（`quality: StreamQuality`）；
- **受控文本槽 `detail: Option<ErrorDetail>`**：`ErrorDetail` 是独立 newtype
  （`impl Display + std::error::Error`），经 thiserror `#[source]` 接入错误链，宿主可用
  `std::error::Error::source()` 下探取回底层文本，`Error::detail()` 是便捷访问器。
  Display 在类别正文后追加 `": {detail}"`。

`ErrorDetail` 的构造契约（类型 doc comment 明文 + `detail_slot_preserves_underlying_text_per_tasks_md_5_5`
测试钉住）：只放**已脱敏的短分类文本**，即迁移切片将从 perr("{label}: {transport}")
（source_netease.rs:147、source_kugou.rs:185、source_qqmusic.rs:251/308 一类站点）搬进来的
底层文本，例如 transport 分类器的 Fatal 文本（"HTTP 429"、"API code 881: 参数异常"）与
调用点标签（"stream resolve"）。cookie、token、签名参数、session id、完整请求 URL 不得
进入该槽位；这是构造方契约而非类型系统保证（库无法机械脱敏任意文本），与旧
`Provider { message }` 槽位的暴露级别一致，且测试以真实迁移文本钉住无凭据材料。

对 §5.5 的边界承认（诚实记录）：`ErrorDetail` 保留了底层错误**文本**；底层错误**对象**
（如 `TransportError` 实例）不进 `#[source]` 链，因为那会把 M1 类型抬进公共 API 签名
（`source(): &(dyn Error + 'static)` 的借用约束与 Box 化会把 transport.rs 变成公共依赖）。
§8 迁移切片搬文本，不搬对象；需要对象时宿主在 provider 边界自己持有。

Display 与既有钉死文本的关系（诚实记录，修正上一版 §8 的过头说法）：M2 变体 Display
**不含** "provider {id} failed:" 前缀（provider_id() 是结构化访问器，§1.3）。因此 §8
迁移切片里，凡是钉死了 "provider {id} failed: {label}: {transport}" 全句的测试（如
source_netease.rs:1842、source_qqmusic.rs:3029）**必须更新**为新 Display 形态
（如 "network error: song details: HTTP 502"），逐字节匹配只在正文恰好相同时成立
（SessionExpired 正文 = "login required or session expired"，AuthenticationRequired 正文
= "login required"）。每个迁移切片负责核对并记录其测试翻新。

`Error` 加 `#[non_exhaustive]`（下游穷举 match 已因加变体而 breaking，补上后未来加变体
不再 breaking；crate 内 match 不受影响，实核过既有 match 全带 wildcard 分支）。

### 1.3 新增访问器

- `Error::category(&self) -> ErrorCategory`：总函数，§1.1 矩阵；
- `Error::provider_id(&self) -> Option<&str>`：`Provider` 及新变体返回 id；
- `Error::detail(&self) -> Option<&str>`：受控底层文本（§1.2）；
- `Error::is_retryable(&self) -> bool`：§1.1 定义；
- `Error::user_hint(&self) -> &'static str`：按类别的默认用户提示（宿主可自行本地化覆盖），
  非空由矩阵测试钉住。

关于旧 `Unsupported(String)` 的 `provider_id()` 恒 None 的代价与理由（评审记录）：该载荷
在 trait 默认实现（provider/traits.rs:119/138/145/153 传 `self.id()`）与 bilibili/apple/
migu/kuwo 站点传的是 provider id，但 source_youtube_music.rs:171（"YouTube returned a
DRM-protected stream; ..."）、source_jamendo.rs:416/421/429/434/448、source_fivesing.rs:535、
source_kugou.rs:1136 传的是整句描述。载荷语义不齐，从载荷推导 provider id 会把句子当 id
返回，故保持 None；"哪个 provider 不支持 X"的归因在 §8 迁移切片里由
`UnsupportedOperation { provider }`（typed 位）承担。本轮评审未改旧变体（改 Display/语义
违反最小修改），此结论记录在案。

### 1.4 兼容策略

- 七个旧变体**不改不删**；它们的 Display、`#[from]`、构造点全部不变（评审轮以
  `git diff` 核对旧七变体 Display 与 HEAD 逐字节一致）；
- `Error::Provider` 仍是合法 catch-all：九 provider 的 `perr`（盘点 46 个构造点）本切片
  零改动，逐步迁移交给 §8 的 provider 切片（迁移时 detail 槽位承接底层文本）；
- `TransportError`（transport.rs:199-234）、`DownloadError`（download.rs:106-138）、
  `ResolveFailure`（resolve.rs:154-165）本切片不动；`retry.rs` 分类器不动（M1 边界）；
- migu/fivesing 等钉死消息的测试不受影响（旧变体 Display 未动，新变体无消费者）。

## 2. 认证状态机（provider/traits.rs）

### 2.1 七态定义

`AuthState`（封闭集合，源 `src/provider/traits.rs`）：`LoggedOut`、`PendingQrScan`、
`Authenticated`、`Expired`、`Invalidated`、`Refreshing`、`ReauthorizationRequired`。

覆盖盘点缺口：`LoginStatus` 三态缺"待扫码/刷新中/需要重新授权/失效（不等于过期）"
（traits.rs:126-133），`QrLoginStatus` 已有七态但只是登录流程状态（traits.rs:141-150）。

语义要点：

- `Authenticated` 是"会话有效"，**不**隐含账号身份：kuwo 匿名 web token（kuwo.rs:722-736
  写入、kuwo.rs:1956-1961 却硬编码 LoggedOut）迁移后是 `Authenticated` + 匿名身份
  （`to_login_status` 折算为 `LoggedIn { user_id: None, .. }`，§2.3）；
- `Expired` = 可刷新/可重登（netease/qq/kugou `mark_session_expired` 的落点，
  source_netease.rs:117-122）；
- `Invalidated` = 平台侧吊销（改密/设备踢出），刷新不可恢复，必须重新登录（bilibili
  `login_expired` 不可达缺口的语义位，source_bilibili.rs:928）；
- `ReauthorizationRequired` = 会话存在但需交互式重授权（QR 风控落点，`RiskControl`）；
- `PendingQrScan` 覆盖"待扫码"与"已扫码待确认"，QR 码过期仍在流程内（重新出码）。

### 2.2 转移表与守卫

`AuthState::can_transition_to(&self, next) -> bool`（纯谓词）与
`AuthState::transition(&mut self, next) -> Result<(), AuthTransitionError>`（受控写）。
自转移一律允许（幂等标记，如 `mark_session_expired` 到达两次）。`Y`=允许，`N`=拒绝：

| from 与 to | LoggedOut | PendingQrScan | Authenticated | Expired | Invalidated | Refreshing | Reauth |
|---|---|---|---|---|---|---|---|
| LoggedOut | Y | Y（开始登录） | Y（凭据导入/直接登录） | Y（恢复已过期会话） | N | Y（会话恢复/导入校验） | N |
| PendingQrScan | Y（取消/流程结束） | Y | Y（扫码成功） | N | N | N | Y（扫码风控） |
| Authenticated | Y（注销） | Y（重新扫码登录） | Y | Y | Y | Y | Y |
| Refreshing | Y（注销/放弃，revoke 可达） | N | Y（刷新成功） | Y（refresh token 过期） | Y（刷新被拒） | Y | Y（要求重授权） |
| Expired | Y（清除） | Y | Y（重登成功） | Y | Y（平台判吊销） | Y（自动刷新） | N |
| Invalidated | Y（确认） | Y | Y（全新登录） | N | Y | N | N |
| Reauth | Y（放弃） | Y | Y（重授权成功） | N | N | N | Y |

评审修订：`Refreshing -> LoggedOut` 开放（logout 是文档化的 revoke，刷新在途也必须可达，
否则 provider 要么违规转移要么谎报 Authenticated）；`LoggedOut -> Expired` 开放（持久化
过期会话的恢复/导入：validate_session 读到过期存量时是合法到达）。表格由
`auth_state_transition_table_is_pinned` 测试全组合（7x7）钉死。`AuthTransitionError
{ from, to }` 是本模块的独立类型（不走 `crate::Error`，状态机守卫是编程契约，不是平台
错误）。

### 2.3 与旧类型双向折算

- `From<&LoginStatus> for AuthState`（无损）：`LoggedOut` 到 `LoggedOut`、`LoggedIn` 到
  `Authenticated`、`Expired` 到 `Expired`；
- `From<&QrLoginStatus> for AuthState`（有损，登录流程到会话状态）：`Waiting | Scanned | Expired`
  到 `PendingQrScan`（QR 过期=流程内重出码，不回退会话）、`Success` 到 `Authenticated`、
  `RiskControl` 到 `ReauthorizationRequired`、`ServerError | Failed` 到 `LoggedOut`（本次
  尝试结束且无会话）；M3 s5 起枚举扩为九态（MODEL_ALIGNMENT_DESIGN.md §6.1）：
  `NetworkError` 到 `PendingQrScan`（QR 仍有效，继续轮询）、`InvalidSession` 到
  `LoggedOut`（本地会话失效，需重出码），不新增 AuthState 变体、不新增转移边；
- `AuthState::to_login_status(&self, identity: Option<(&str, &str)>) -> LoginStatus`：
  反向折算需要身份（身份在 `ProviderSession`，不在状态里），因此是带参方法而非 `From`：
  `LoggedOut | PendingQrScan` 到 `LoggedOut`；`Authenticated | Refreshing` 到 `LoggedIn`
  （identity 为 None 时 user_id/display_name 均为 None，即匿名已认证态，旧模型以此表达）；
  `Expired | Invalidated | ReauthorizationRequired` 到 `Expired`（旧三态对"必须重登"的
  两态折叠）。

### 2.4 trait 生命周期方法（tasks.md 10.3）

`AuthenticationProvider` 新增两个**带默认实现**的异步方法（七个既有 impl 编译零改动）：

```rust
async fn refresh_session(&self) -> Result<AuthState, Error> {
    Err(Error::UnsupportedOperation {
        provider: Some(self.id().into()),
        detail: Some(ErrorDetail::new("session refresh is not implemented")),
    })
}
async fn validate_session(&self) -> Result<AuthState, Error> {
    Ok(AuthState::from(&self.login_status().await?))   // 默认本地推导，不发网络请求
}
```

- `revoke` 不新增方法：`logout()` 即撤销（tasks.md 10.5#7），doc comment 明确；配合
  §2.2 的 `Refreshing -> LoggedOut` 边，revoke 在刷新在途时同样可达；
- tasks.md 10.3 的 `begin_login()/poll_login()` 即既有 `QrAuthenticationProvider` 的
  `create_qr_login/poll_qr_login`，不重复建面；
- provider 侧接线（kuwo 硬编码 LoggedOut、bilibili 不可达 Expired、fivesing 死 impl、
  migu 死 SessionStore）全部留给 §8 的 provider 切片，本切片不动。

## 3. 多账号 Session 模型（session.rs）

### 3.1 (provider, account) 双键（tasks.md 10.4）

`SessionStore` 内部键从 `String`（provider 单键，多账号互相覆盖）改为
`BTreeMap<(String, String), ProviderSession>`，键为 `(provider_id, account_id)`：

- `pub const DEFAULT_ACCOUNT_ID: &str = "default"`；
- 新 API：`get_for / set_for / clear_for / list_accounts`（list 返回该 provider 下有会话的
  账号名升序表；实现用开上界扫描 + provider 前缀截断，评审修订：闭区间上界会漏首字符为
  U+10FFFF 的账号名，见 `list_accounts_sees_accounts_sorting_above_any_closed_bound`）；
  `set_for` 对 provider 与 account 均做非空校验（对齐 `set` 现有
  `InvalidInput` 语义，session.rs:37-39）；
- 旧 `get/set/clear` 签名与行为不变，委托到 `DEFAULT_ACCOUNT_ID`；旧调用方读写的是
  默认账号，与其它账号互不可见。

Session ID 缺口（tasks.md 10.4 "必须区分 ... Session ID" 的偏离记录）：10.4 列出的
Provider ID / Account ID / 登录状态 / Cookie / Token / 过期时间 / 更新时间 / 地区信息
全部落地（键 + 字段 + AuthState），唯 Session ID 未建独立维度。理由：当前存储模型是
`(provider, account) -> 单会话`，会话身份由键与会话内容唯一决定，显式 session_id 是冗余
标签；它只在"同账号多并发会话（多设备/多 token 簇）"成为需求时才有语义，届时加
session_id 作为键的第三维（M3 接线切片随多实例模型一起评估）。此为有意推迟，不是遗漏。

### 3.2 `ProviderSession` 扩展字段与文件兼容

新增三个可选字段，全部 `#[serde(default)]`，旧 JSON 文件（无这些键）继续加载：

```rust
pub expires_at: Option<u64>,   // unix 秒；None = 平台未给出/未知
pub updated_at: Option<u64>,   // unix 秒
pub region: Option<String>,    // 平台给出的地区标识（非凭据）
```

兼容记录：source_netease.rs:108、source_kugou.rs:123、source_bilibili.rs:152、
source_qqmusic.rs:172 四处 `ProviderSession { .. }` 字面量各补一行 `..Default::default()`
（字段全集构造的机械必要改动，值不变；这是本切片对 provider 文件的唯一触碰）。Debug
脱敏实现扩展覆盖新字段（expires_at/updated_at/region 均非凭据，可打印；cookie/token
值仍绝不打印）。

### 3.3 import/export（ProviderSession 与 JSON 字节互转）

- `ProviderSession::export(&self, provider, account) -> Result<SessionExport>`：
  JSON 信封 `{ format: 1, provider, account, session }`；
- `ProviderSession::import(provider, account, bytes) -> Result<ProviderSession>`：
  解析信封并**校验 provider/account 与目标一致**（拒绝跨 provider/跨账号注入凭据），
  format 不符则 `InvalidInput`；
- `SessionExport` 是字节容器 newtype：`as_bytes()/into_bytes()/provider()/account()`，
  **Debug 只打印 provider/account/字节数，绝不打印内容**（导出物脱敏测试钉住）。

### 3.4 并发与隔离

`SessionStore` 保持 `Arc<RwLock<BTreeMap<..>>>` 与 `unwrap_or_else(|e| e.into_inner())`
毒锁恢复语义（session.rs:29-31 现状）。多账号隔离并发测试：8 线程各写独立账号 50 次 +
1 个竞争读线程断言"读到的账号 A 值永远来自 A 的写入集"
（`multi_account_isolation_under_concurrency`）。

## 4. 文件持久化（session_file.rs，feature `session-file`）

### 4.1 账号路径布局

- 默认账号 = 旧路径 `root/{provider}.json`（不迁移旧文件即可继续加载）；
- 非默认账号 = `root/{provider}.{account}.json`（provider 与 account 同一字符白名单
  `alnum - _`，沿用并强化现路径穿越拒绝，session_file.rs:44-55）；
- 新 API：`load_for / save_for / clear_for / list_accounts`；旧 `load/save/clear` 委托默认
  账号，签名不变。

### 4.2 损坏隔离与恢复（评审修订：隔离只吃"损坏"类错误）

`load_for` 读到文件时：

1. **JSON 解析失败**（`serde_json::from_slice` 失败）或 **codec 以 Serialization 类别显式
   声明损坏**（SessionCodec 契约："Returning an Error::Serialization marks the stored
   file corrupt"）时，才触发隔离：rename-if-absent，首个损坏副本占用规范名
   `root/{原名}.corrupt`，后续同文件再损坏落到 `.corrupt-1`、`.corrupt-2`、...
   （上限 16 个编号副本，用尽后文件留在原地、仅报错）。**任何既有隔离副本都不被覆盖**
   （第二轮评审指出首轮"POSIX replace 静默覆盖"仍有数据丢失面：同一文件第二次损坏会
   销毁可能是唯一可抢救的第一副本，本轮以编号副本关闭），即"隔离"；
2. codec 返回**其它类别**的错误（加密 codec 的密钥库暂锁、KMS 限流等瞬态故障）**原样
   透传，文件原地不动**，恢复后重试即可。第一轮评审指出原实现"任意 decode 错误都触发
   隔离"，会把可恢复文件毁掉，本节为该修复的设计定稿；
3. 隔离后返回已分类错误（`ErrorCategory::Serialization`），store 以空态继续可用（下一次
   `load_for` 返回 `Ok(None)`），即"恢复"，无需手动 reset。

测试：`corrupt_file_is_quarantined_and_store_recovers`（垃圾字节 + 第二次损坏断言首副本
字节未被覆盖、新副本落 `.corrupt-1`）、
`transient_decode_failure_does_not_quarantine_or_destroy_the_file`（Io 类瞬态故障：错误
透传、文件原地、heal 后可读）、`codec_declared_corruption_quarantines_the_file`
（Serialization 类声明：隔离生效）。

### 4.3 加密存储扩展点（tasks.md 10.5#5）

```rust
pub trait SessionCodec: Send + Sync {
    fn encode(&self, plaintext: &[u8]) -> Result<Vec<u8>>;
    fn decode(&self, stored: &[u8]) -> Result<Vec<u8>>;
}
pub struct PlainCodec;   // 恒等编解码（明文 JSON），默认值
```

- `FileSessionStore::open(root)` 签名不变 = `PlainCodec`（默认行为与旧版逐字节一致）；
- `open_with_codec(root, codec)` 注入加密实现；save 走 `encode`，load 走 `decode`；
- decode 错误的语义分级见 §4.2（Serialization 类 = 损坏声明，其余 = 瞬态透传）；
- 明文默认不再是"未说明的明文"：`PlainCodec` doc comment 明确标注明文语义（10.5#4）。

### 4.4 保留项

原子写（tempfile + persist，session_file.rs:27-36）、目录 0o700（13-17）、文件 0o600、
路径穿越拒绝、既有 debug 脱敏与穿越测试全部保留不删；0o600 断言扩展到账号文件。

## 5. Registry 查询面（registry.rs）

### 5.1 注册冲突语义

- 新增 `try_register_*` 共 11 槽位：同 id 重复注册返回
  `Err(RegistryError::DuplicateRegistration { id, slot })`，**旧实例保持原位**；
- 既有 `register_*` 的 OR 合并（registry.rs:42-51）+ 静默覆盖（55-95）行为**保留并首次
  加测试钉住**（盘点：今天无任何测试）：这是既有公共行为，不破坏；
- `RegistryError`（本模块类型，`impl std::error::Error`）不并入 `crate::Error`：注册冲突
  是宿主编程错误，不是平台/运行时错误。

### 5.2 注销与两类视图（评审修订：历史与活注册面分离）

`unregister_*` 共 11 个（返回被移除实例）+ `unregister_all(id) -> bool`（清除全部槽位与
descriptor）。语义分两层，两类消费者各取所需：

- **历史视图** `descriptor(id)`：能力聚合是注册历史的 OR，单槽位注销保留 descriptor
  （`unregister_all` 才清除）；
- **活注册面** `provider_ids()` 与 `providers_with_capability()`：`provider_ids()` 只返回
  仍有至少一个槽位实例的 id；`providers_with_capability` 的谓词作用于**活能力**（当前
  槽位实例 `capabilities()` 的并集），单槽位注销立即不再匹配。第一轮评审指出原实现
  两查询都基于历史聚合，"unregister_search 后 track_search 查询仍返回该 id"，与
  tasks.md §5.4 "按能力查询"的意图不符；修复后两 API 的 doc comment 明确标注语义，
  测试 `capability_queries_reflect_live_registration_not_history` 钉住。

### 5.3 按能力查询

`providers_with_capability(pred: impl Fn(&ProviderCapabilities) -> bool) -> Vec<String>`
（升序，§5.2 活语义）。tasks.md §5.4 的"按能力查询/按槽位谓词"由此一个入口覆盖（槽位
存在性可由 `*_provider(id)` 访问器组合判定）。

### 5.4 能力描述升级

`ProviderDescriptor` 新增四个字段（旧字段不动）：

- `version: Option<String>`，来自 `MusicProvider::version()` 新默认方法（默认 None）；
- `auth: AuthCapability`，来自 `MusicProvider::auth_capability()` 新默认方法 + QR 槽位
  升级：`register_qr_authentication` 时 descriptor.auth 升为 `Qr`，**且仅 Qr 升级粘住**：
  `record()` 不回写已为 `Qr` 的 auth 位（评审修订：原实现"下一次任意槽位注册无条件覆盖
  auth"，使最终值依赖注册顺序、与 qr_auth 槽位仍挂着的实际状态矛盾）。**语义定稿（第二轮
  评审措辞修正）：粘住仅覆盖 Qr 一个值；其余值是 last-writer-wins**，与 legacy 覆盖式
  注册语义一致（同 id 换实例时 Cookie/None 的最终值按注册顺序而定，这不是 bug 而是既有
  行为的延续；`unregister_all` 清除 descriptor 时一并清除）。测试
  `descriptor_auth_upgrade_is_sticky_across_registration_order` 以三种注册顺序钉住；
- `priority: i32`，来自 `MusicProvider::priority()` 新默认方法（默认 0，越大越优先，
  registry 只呈现值、排序由宿主做）：补齐 tasks.md §5.4 点名的"查询 Provider 优先级"，
  第一轮评审指出该项整体缺失且无偏离记录，本轮按最小面实现（descriptor 字段 + trait
  默认方法）；
- `supported_qualities: Vec<StreamQuality>`，来自 `MusicProvider::supported_qualities()`
  新默认方法（空 = 未声明；衔接 models/stream.rs:5-12 音质阶梯）。

`AuthCapability`（`#[non_exhaustive]`）：`None | Cookie | Qr | OAuth | Anonymous | Password
| Token | Other`（评审修订：补 tasks.md §10.2 点名的"用户名密码"与"Token"，枚举未发布
先补位，M3 接线切片表达密码/token 登录时有落点）。
默认推导：`capabilities().authentication == true` 则 `Cookie`（九 provider 的真实机制），
否则 `None`；**单一 bool `capabilities.authentication` 保持不动**（九处 registration
测试钉住它）。

九 provider 的 `capabilities()` 块本切片不改（kugou artists 回退注释等随 §8 provider
切片核对）。

核对结果（kugou 切片，§9.2）：`KugouProvider` 的 `artists: true` 注释
"Artist lookup is a search-based fallback (no dedicated endpoint)" 与实现一致：
`artist(id)` 把 `id` 当歌手名直接回填（kugou.rs ArtistProvider 块），
`artist_top_tracks(id)` 走 `search` 后按 `artists.contains(name)` 过滤，均无专属端点。
注释措辞已随核对补上"id 即歌手名"的说明。

### 5.5 健康查询接入

`ProviderRegistry` 自持一个 `ProviderHealthMonitor`（`Registry::new()` 用
`ProviderHealthConfig::default()`；`with_health_config` 可配）：

- `health_monitor(&self) -> &ProviderHealthMonitor`（宿主对查询侧记录成功/失败）；
- `health_snapshot(&self, id) -> ProviderHealthSnapshot`（health.rs:110-128 快照委托）；
- **注销即重置**（第二轮评审修订）：`unregister_all(id)` 同时调用
  `ProviderHealthMonitor::reset(id)`（health.rs 新增 API，移除该 id 全部记录）。id 已
  完全离开注册面，历史视图(descriptor)与活视图(provider_ids)的两层语义延伸到健康面：
  健康历史属于"活注册面"，重注册从全新 Closed 起步，不继承旧 Open/冷却期；单槽位
  注销不重置（id 在其它槽位仍活着）。测试
  `unregister_all_resets_the_health_history` 钉住 Open 残留与重注册不被旧熔断拦截。

边界说明（诚实记录）：resolve.rs:109-116 流水线内的熔断监视器是**另一个实例**，M1
边界不动；registry 监视器服务"注册表视角"的健康查询，两者不共享状态。

### 5.6 download 死位决策

**决策：停止 OR 并文档化。**`record()` 不再把 `incoming.download` 并入聚合
（删除 registry.rs:51 的 OR 行）。理由：

- download 是引擎侧能力（TRANSPORT_DESIGN.md §6：引擎七变体/自有重试策略，不属于
  provider 声明面）；`grep "download: true"` 全库 0 命中，聚合位今天是恒 false 死位；
- 停止 OR 后，未来 provider 误置 `download: true` 也不会污染聚合描述；若后续出现真正的
  provider 侧下载槽位，再以独立槽位接入；
- `ProviderCapabilities.download` 字段本身保留（provider 对象自持自述）。**可观测性
  边界（第二轮评审修订）：该位在 registry 的全部两个视图里恒为 false**：record() 不并入
  descriptor 聚合，live_capabilities 也不合并它，因此 `providers_with_capability(|c|
  c.download)` 永远返回空集且无报错；"provider 自述不受影响"仅指 provider 对象自身的
  `capabilities()` 调用，在 registry 面没有任何出口。字段 doc comment 与本节均明文标注，
  避免宿主把它当作可查询能力。本切片无可观测行为变化（无 provider 置 true）。测试用
  stub（download:true）钉住"聚合不并入"。

### 5.7 多实例模型（M3 接线点，本切片不实现）

tasks.md §5.4 "同一 Provider 的多个实例（不同账户/地区/Cookie/网络）"与"不要让全局
Registry 直接承担账户状态管理"：完整实例键模型（`BTreeMap<(id, instance_key), Arc<..>>`
槽位、与 §3.1 账号键对齐）推迟到 provider 切片完成后的 M3 接线切片；本切片的
`try_register_*` 冲突语义已保证"同 id 第二实例不会静默覆盖第一实例"，是实例模型的安全
前置。**偏离说明**：ask 的 registry-capabilities 切片文本含"注册时可选实例键"，按最小
修改原则本切片只落地冲突语义 + 本节文档，理由：槽位 x 实例键的 API 面是 11 槽位的
笛卡尔展开，在 provider 迁移（谁会注册多实例、以什么身份键）尚未发生前落地必然返工。

## 6. 兼容性与门禁矩阵

硬约束（全部在本会话实跑验证，命令见 §7）：

| 约束 | 验证 |
|---|---|
| 九 provider feature 全编译 | `cargo check --lib --no-default-features --features source-netease,...,source-fivesing` |
| no-default 边界保持（reqwest-free） | `cargo check --lib --no-default-features` |
| 全特性编译 | `cargo check --lib --all-features` |
| 现有测试不破坏 | `cargo test --lib --all-features`（外层门禁跑全量） |
| 检查严格度不降 | `cargo clippy --lib --all-features` 与 `--no-default-features` |
| 错误/日志不带凭据 | §1.2 类型层面（无自由 message 位）+ ErrorDetail 构造契约 + 新旧脱敏测试 |
| M1 传输层与 retry 分类器复用 | transport.rs / retry.rs 本切片零改动（当前不含 M2 变体构造） |

诚实边界（承评审核验记录）：transport.rs/retry.rs 是未跟踪新文件（无 M1/M2 提交边界），
"零改动"只能验证到"当前不含 M2 变体构造"，git 无法区分净改动归属。

## 7. 本切片测试矩阵

| 测试（位置） | 钉住的语义 |
|---|---|
| `error::tests::error_mapping_matrix_is_total_and_classified` | §1.1 矩阵全变体：category/is_retryable/provider_id/user_hint 非空/Display 片段 |
| `error::tests::new_variant_debug_never_contains_credential_material` | §1.2 脱敏不变量 |
| `error::tests::detail_slot_preserves_underlying_text_per_tasks_md_5_5` | §1.2 detail 槽位：source() 链可达、Display 追加、无凭据材料 |
| `provider::traits::tests::auth_state_transition_table_is_pinned` | §2.2 全组合 7x7 转移表 + transition 守卫 |
| `provider::traits::tests` 其余（转换/默认方法） | §2.3 折算表、§2.4 默认实现语义 |
| `session::tests::multi_account_isolation_under_concurrency` | §3.4 并发隔离 |
| `session::tests::list_accounts_sees_accounts_sorting_above_any_closed_bound` | §3.1 开上界扫描（U+10FFFF 回归） |
| `session::tests` 其余（双键/委托/export/import/Debug） | §3.1/§3.3 |
| `session_file::tests::corrupt_file_is_quarantined_and_store_recovers` | §4.2 JSON 损坏隔离 + 第二次损坏不覆盖首副本 |
| `session_file::tests::transient_decode_failure_does_not_quarantine_or_destroy_the_file` | §4.2 瞬态透传 |
| `session_file::tests::codec_declared_corruption_quarantines_the_file` | §4.2 Serialization 类声明隔离 |
| `session_file::tests` 其余（账号路径/权限/codec/导出） | §4.1/§4.3/§4.4 |
| `registry::tests::try_register_conflict_keeps_the_first_instance` | §5.1 冲突语义 |
| `registry::tests::legacy_register_overwrites_slot_and_or_merges_capabilities` | §5.1 既有行为钉住 |
| `registry::tests::capability_queries_reflect_live_registration_not_history` | §5.2 活注册面 |
| `registry::tests::descriptor_metadata_plumbs_version_priority_auth_and_qualities` | §5.4 descriptor 新字段 |
| `registry::tests::descriptor_auth_upgrade_is_sticky_across_registration_order` | §5.4 仅 Qr 粘住 |
| `registry::tests::aggregate_download_bit_is_never_merged` | §5.6 download 决策 |
| `registry::tests::health_snapshot_reflects_the_registry_monitor` | §5.5 健康接入 |
| `registry::tests::unregister_all_resets_the_health_history` | §5.5 注销即重置健康历史 |
| `download::tests::download_error_category_bridge_covers_all_seven_variants` | §9.7 桥映射全七变体：category/to_error 一致、M2 面可重试性、归因、残差臂 |
| `download::tests::engine_retry_policy_and_m2_face_diverge_only_where_declared` | §9.7 引擎策略族与 M2 面的声明分歧（Io/RangesUnsupported），§6 边界 |
| `download::tests::bridge_preserves_the_underlying_text_in_the_sanitized_slots` | §1.2 底层文本进 detail/Io/Provider 槽位，不静默丢弃 |

## 8. 后续切片索引（不在本 ask 范围）

按 TRANSPORT_DESIGN.md §7 顺序逐个 provider 落地错误迁移与 AuthState 接线（每切片独立
`cargo check --no-default-features --features <f>` + 测试）：

1. netease（301/512 到 `SessionExpired`、405 到 `RateLimited`、881 参数到 `InvalidResponse`；
   底层文本进 detail 槽位；已落地，provider-error-auth-netease：test/clippy 通过
   feature source-netease，无独立 §9 小节）；
2. qqmusic（VIP/风控到 `VipRequired`/`PermissionDenied`、1000/301、QR 403 到
   `AuthenticationFailed`；已落地，provider-error-auth-qqmusic：test/clippy 通过
   feature source-qqmusic，无独立 §9 小节）；
3. kugou（过期码表补齐或记录"无码可辨"、AuthState；已落地，见 §9.2）；
4. kuwo（login_status 真实 web_token 态、`AuthenticationRequired`；已落地，见 §9.3）；
5. bilibili（login_expired 写入点使 Expired 可达、-403/-412 类别化；已落地，见 §9.4）；
6. migu（NotFound 拆分、版权/地区、死 SessionStore 决策；已落地，见 §9.5）；
7. jamendo/apple/fivesing（轻认证三源、fivesing 死 auth impl 处置；已落地，见 §9.6）；
8. download 桥（`DownloadError` 到 `ErrorCategory` 映射 impl，引擎类型不动，无 `From` 强转；已落地，见 §9.7）；
9. lx-js（14 处 catch-all 按宿主契约分类；已落地，见 §9.8）。

每切片对被钉死的消息文本必须**显式核对并记录测试翻新**：M2 变体 Display 不含
"provider {id} failed:" 前缀（§1.2），正文逐字节匹配仅 SessionExpired
（"login required or session expired"）与 AuthenticationRequired（"login required"）成立；
底层 TransportError 文本经 detail 槽位保留（§1.2），不得静默丢弃。

## 9. 评审修订记录（第一轮，2026-11-19）

| # | 评审发现 | 落点 |
|---|---|---|
| 1（高） | 新变体零载荷与 tasks.md §5.5 "保留底层错误信息"冲突且未记录偏离；§8 的"Display 逐字节匹配"说法过头 | §1.2 重写：`ErrorDetail` 受控槽位 + `#[source]` + `detail()` + 构造契约与测试；§1.2/§8 承认 Display 前缀差异、钉死测试须翻新；边界承认：底层错误对象不进 source 链 |
| 2（中） | session_file 任意 decode 错误触发隔离，瞬态故障毁文件 | §4.2/§4.3：隔离只吃 Serialization 类（JSON 解析失败或 codec 显式声明）；其余透传；新增 Flaky/DeclaredCorrupt 两个 codec 测试 |
| 3（中） | descriptor.auth 的 QR 升级被下一次注册冲掉，顺序依赖 | §5.4 + registry.rs `record()`：Qr 粘住（不回写已升级位）；三种顺序测试钉住；descriptor doc 同步 |
| 4（中） | tasks.md §5.4 "查询 Provider 优先级"缺失且无偏离记录 | §5.4 + `ProviderDescriptor::priority` + `MusicProvider::priority()` 默认 0；本轮按最小面实现（不再偏离） |
| 5（中） | providers_with_capability/provider_ids 基于历史聚合，注销后返回非活数据 | §5.2：两查询改活语义（provider_ids = 槽位键并集；谓词作用于活能力），历史视图保留在 descriptor；doc comment + 测试钉住 |
| 6（低） | SessionStore::list_accounts 闭区间上界漏 U+10FFFF 首字符账号 | §3.1 + session.rs：改开上界扫描 + provider 前缀截断；回归测试钉住 |
| 7（低） | tasks.md §10.4 "Session ID"未区分且设计未说明 | §3.1 偏离记录：单会话模型下键即身份，显式 session_id 属多并发会话需求，随 M3 多实例模型评估 |
| 8（低） | AuthCapability 缺 Password/Token（tasks.md §10.2 点名） | §5.4 + traits.rs：补 `Password`/`Token` 变体 |
| 9（低） | 状态机缺 Refreshing→LoggedOut 与 LoggedOut→Expired | §2.2：两边开放并给理由（revoke 可达性/过期会话恢复）；7x7 测试同步 |
| 10（低） | `Unsupported(String)` 载荷是 provider id 却 provider_id()=None | §1.3 记录结论：载荷语义混合（youtube/jamendo/fivesing/kugou 传整句），从载荷推导 id 失真，保持 None；归因落点是 §8 迁移的 `UnsupportedOperation { provider }` typed 位 |

### 9.1 第二轮评审修订（2026-11-19）

| # | 评审发现 | 落点 |
|---|---|---|
| 11（低） | load_for 方法级 doc 仍写"codec failure 即隔离"，与收窄后的实现矛盾 | session_file.rs doc comment 翻新为新契约（仅 Serialization 类/JSON 解析失败隔离，其余透传），与 SessionCodec trait doc、本节 §4.2 三处一致 |
| 12（低） | quarantine 的 POSIX replace 会覆盖既有隔离副本，同文件二次损坏销毁第一副本 | session_file.rs `quarantine_corrupt` 改 rename-if-absent + 编号副本（.corrupt-1/2/...，上限 16，用尽后文件留原地仅报错），不覆盖任何既有副本；§4.2 定稿 + 测试扩展（首副本字节保留、新副本落 .corrupt-1） |
| 13（低） | registry doc 称 auth 为"曾声明的最强能力/顺序无关"，实际仅 Qr 粘住，非 Qr 值是 last-writer-wins | registry.rs 类型 doc 与字段 doc 措辞修正（"仅 Qr 升级粘住，其余 last-writer-wins"）；§5.4 同步；粘住测试描述同步 |
| 14（低） | unregister_all 不清健康状态，同 id 重注册继承旧 Open/冷却 | health.rs 新增 `reset(provider)`；unregister_all 调用之（单槽位注销不重置，id 仍活在其它槽位）；§5.5 注销即重置 + 测试 `unregister_all_resets_the_health_history`（Open 残留清除 + 重注册 try_acquire 不被旧熔断拦截） |
| 15（低） | download 位停止 OR 后在 registry 两个视图（descriptor 聚合/活能力）恒 false，谓词对它永得空集且无报错 | §5.6 补可观测性边界定稿：该位仅 provider 对象自持，registry 一律不呈现，对它写谓词恒 false；`ProviderCapabilities.download` 字段 doc comment 同步明文 |
| 16（低） | ErrorDetail 未抬到 crate root（与其它新类型面不一致） | lib.rs 根导出补 `ErrorDetail`（`pub use error::{Error, ErrorCategory, ErrorDetail, Result}`） |

### 9.2 kugou 切片落地记录（provider-error-auth-kugou，2026-11-19）

§8 切片 3 的落地证据，逐条对应本 ask 范围：

- **过期码表盘点结论：无码可辨，依赖 `login_expired` 标记**。盘点面：上游
  voicefox `source/src/kg/`（只读参考）与仓库内 `tests/fixtures/kugou`。两处均无任何
  带"登录会话过期"语义的 `error_code` 值：kg 模块唯一的码分类是 QR 轮询的
  `data.status` 字段（`-1`/`5`/`6` 指二维码过期而非会话过期，login.rs `status_from_code`）
  与 search 的裸 `error_code != 0` 检查；上游 session.rs 完全没有过期标记或过期码表。
  按 tasks.md §15.4（fixture 通过不宣称 live 已验证），不为无 fixture 佐证的码值
  扩表：`retry::KUGOU_CODES.expired` 保持 `&[]`，理由写进该常量 doc，并由
  `retry::tests::kugou_api_codes_pin_the_no_discernible_expired_code_decision` 钉住
  （ok=0、expired 空、throttled None、非 ok 码恒 Fatal）。`get_json` 的
  `TransportError::SessionExpired` 臂因此是防御位而非可达位，doc 明文。
- **`mark_session_expired` 迁到 M2 类别**（netease/qq 同型）：签名改为
  `(&self, detail: Option<ErrorDetail>) -> Error`，返回
  `Error::SessionExpired { provider: Some("kugou"), detail }`；旧自由文本
  `"{label}: login required or session expired"`（Error::Provider）由变体替代，
  Display 正文 = "login required or session expired"、label 进 detail 槽位（§1.2）。
- **登录态迁到 AuthState**：新增私有 `auth_state()`（无网络 I/O），谓词维持上游
  会话形状：`token` cookie + user_id 缺一即 `LoggedOut`，`login_expired` 标记即
  `Expired`；`login_status` 改为 `auth_state()` 的文档化折算
  （`AuthState::to_login_status`，§2.3），Authenticated 臂保留 user_id/display_name
  独立管道（与迁移前一致）。旧测试 `login_status_requires_token_and_user_id`
  语义与断言原样保持（仅 mark 调用随签名补 `None`）；新增
  `auth_state_derivation_drives_login_status` 钉状态机视图与折算、类别化错误
  （category/provider_id/detail/is_retryable/Display）。
- **QR 接入 AuthState**：`poll_qr_login` 成功臂清除陈旧 `login_expired` 标记。
  理由：该标记是 kugou 唯一的过期信号（过期码表为空），重扫是文档化的恢复路径
  （§2.2 Expired→Authenticated "重登成功"边）；不清除则该边经 QR 永不可达。
  测试 `qr_success_after_expiry_re_arms_authenticated` 钉住。边界承认：netease/qq
  的 QR 成功臂存在同形陈旧标记残留，属各自切片已落地面，本轮不动。
- **registry 描述准确性核对**：`artists: true` 的"搜索回退、无专属端点"注释与实现
  一致（§5.4 核对结果），注释补"id 即歌手名"说明。

### 9.3 kuwo 切片落地记录（provider-error-auth-kuwo，2026-11-19）

§8 切片 4 的落地证据，逐条对应本 ask 范围：

- **`login_status` 迁到真实 web_token 会话态**：新增私有 `auth_state()`（无网络
  I/O），谓词 = 存储会话中 `web_token` 的存在与否（该 token 由 `session_token`
  在每次 web 签名调用前按需铸造并写入 SessionStore，source_kuwo.rs:721-735）：
  有 token 即 `Authenticated`，无 token 即 `LoggedOut`。旧的硬编码
  `LoggedOut`（source_kuwo.rs 原 1956-1961，与写入点矛盾）移除；`login_status`
  改为 `auth_state().to_login_status(None)` 的文档化折算：匿名 web 身份单独
  描述为 `LoggedIn { user_id: None, display_name: None }`（§2.1 匿名已认证态），
  不宣称账号登录，kuwo 无账号登录流程，会话中永无 user_id/display_name。
  `logout` 仍清整个 session（Authenticated→LoggedOut 注销边）；
  `validate_session` 走默认本地推导。
- **`mark_session_expired` 补齐，类别 = `AuthenticationRequired`**：kuwo 无账号
  会话，匿名 web token 被拒 ≠ 账号会话过期，故原 `{label}: login required`
  字符串（source_kuwo.rs 原 236，`TransportError::SessionExpired` 臂）落点为
  `Error::AuthenticationRequired`（Display 正文 = "login required"，与迁移前
  字符串尾段逐字节一致，§1.2 的两个逐字节例外之一），而非
  `Error::SessionExpired`（那会宣称一个 kuwo 从未有过的账号会话）。标记动作 =
  `invalidate_web_token()`：token 丢弃、状态回 `LoggedOut`，下次 web 签名调用
  重铸新 token，即文档化恢复路径（无重登存在），`is_retryable` = false。
  `get_json_with` 的 `TransportError::SessionExpired` 臂与 kugou 同型为防御位：
  `KUWO_CODES.expired` 为空（retry.rs:68-73），引擎今日无法产出该判定。
- **web_get 的非法会话拒绝保持原样**（有意不做，最小修改原则）：可达的
  "request is illegal" 拒绝面（web_get）维持 invalidate + `Error::Provider`
  ("{label}: session rejected, retry with a fresh token")，由既有测试
  `web_get_invalidates_the_web_token_on_illegal_session_without_retry` 钉住；
  ask 只指定 line 236 的迁移，该面属既有行为不扩大范围。
- **capabilities.authentication 保持 false**（匿名 token ≠ 账号认证；注册测试
  `!descriptor.capabilities.authentication` 钉住不动），显式化走
  `auth_capability() -> AuthCapability::Anonymous` 覆写（traits.rs 的
  `Anonymous` 变体文档化用例即 "kuwo web token / migu sid style"）：descriptor
  的 auth facet 呈现 Anonymous，单一 bool 位不动，"匿名身份 ≠ 登录"在两个面
  各自如实表达。
- **InvalidInput 站点逐个核对（构造点 10 处；ask 计数 14 = "Error::InvalidInput"
  全文出现次数，含 4 处测试 `matches!` 断言）**：M2 分类法无 caller-input 变体
  （legacy `Error::InvalidInput` 即 `ErrorCategory::InvalidInput` 的载体，§1.1
  保留不变；netease/qq/kugou/migu 切片同此先例），十处语义核对后均为调用方输入
  错误（空 id/空关键词/外来 track id/无法识别的链接），类别全部正确保持
  `InvalidInput`，消息逐字节不改；由
  `invalid_input_sites_keep_their_category_and_message` 以"类别 + 全句 Display"
  逐站钉住（kw_link_target/search/search_playlists/playlist/playlist_tracks/
  album/album_tracks/chart_tracks/resolve/lyrics），4 处测试 `matches!` 断言
  继续有效。
- **charts 等能力位不动**：capabilities 块与 registration 测试（track_search/
  stream_resolve/lyrics/playlists/albums/charts/link_resolve）全部保持。
- 新测试：`auth_state_derivation_drives_login_status`（token 存在谓词、匿名
  折算、validate_session 本地推导、分类错误全载荷
  category/provider_id/detail/is_retryable/Display、token 丢弃回 LoggedOut、
  logout 注销边）、`auth_capability_declares_the_anonymous_web_token`
  （bool false + facet Anonymous 的 provider 视图与 registry descriptor 视图）、
  `invalid_input_sites_keep_their_category_and_message`（十站点类别 + 消息）。

### 9.4 bilibili 切片落地记录（provider-error-auth-bilibili，2026-11-19）

§8 切片 5 的落地证据，逐条对应本 ask 范围（source_bilibili.rs）：

- **`login_expired` 写入点补齐，Expired 分支可达**。缺口：`login_status`
  检查 `login_expired` 标记（source_bilibili.rs:928）但全仓库无写入点，Expired
  不可达。落点：API 登录失效裁决 `code = -101`（"账号未登录"）在
  `request_json` 的 provider 侧解析后拦截，请求实际携带 SESSDATA 时判定为
  会话失效，写标记（`mark_session_expired`，netease/qq/kugou 同型：签名
  `(&self, detail: Option<ErrorDetail>) -> Error`，返回
  `Error::SessionExpired { provider, detail }`，label 进 detail 槽位）。
  匿名调用方本来就按设计收到 -101（nav/WBI-keys 匿名形状依赖它），无 SESSDATA
  时 body 原样透传（`anonymous_not_logged_in_body_stays_passthrough` 钉住）。
  声明漂移：存量但被拒的会话使原先被容忍的匿名形状 nav 取键改为
  `SessionExpired`，诚实的重登提示而非静默降级匿名
  （`stale_session_turns_the_wbi_keys_nav_into_session_expired` 钉住）。
  未重试 transport 层码表（retry.rs 不加 BILI_CODES）：-101 判定需要"是否
  携带 SESSDATA"这一 provider 侧上下文，引擎层分类器拿不到。
- **登录态迁到 AuthState**：私有 `auth_state()`（无网络 I/O），谓词维持会话
  形状：无会话或缺 `SESSDATA` 即 `LoggedOut`，`login_expired` 标记即
  `Expired`，否则 `Authenticated`；`login_status` 改为
  `auth_state()` 的文档化折算（§2.3），Authenticated 臂保留 user_id/
  display_name 独立管道（kugou §9.2 同型），旧行为逐断言保持
  （`login_status_views_stay_in_step_with_auth_state` 钉住四态）。新增
  `api_not_logged_in_verdict_marks_the_session_expired` 钉住写入点全载荷
  （category/provider_id/detail/is_retryable/Display + 存储标记 + 双视图）。
- **-403/-412 重试穷尽后类别化**（`signed_get`，原 292-296 仅重试 WBI key）：
  键轮换单次重签后仍为门控码时不再落到调用点 Provider 字符串，
  -403（"访问权限不足"）→ `Error::PermissionDenied`（typed code 槽位
  Some(-403)）、-412（"请求被拦截"，B 站限流拦截）→ `Error::RateLimited`
  （配对中唯一 retryable=true；body 无 Retry-After，槽位 None），label 进
  detail（`gate_error`）。其余 body 保持调用点原形；
  `signed_get_refreshes_wbi_keys_after_gating_code`（恢复路径）原样通过。
  `gate_codes_survive_the_key_rotation_as_categorized_errors` 钉住两码的
  category/provider_id/detail/is_retryable/Display 前缀与 4 请求序列。
- **QR 状态映射接入 AuthState**：`data.code → QrLoginStatus` 提取为纯函数
  `qr_status_from_code`（0/86101/86090/86038 四码，未知码照旧报错）；
  成功臂经 `capture_login_session` 清除陈旧 `login_expired` 标记（§2.2
  Expired→Authenticated"重登成功"边，kugou §9.2 同型），
  `qr_success_clears_a_stale_expiry_marker` 钉住（旧标记清除、新 SESSDATA
  落位、双视图 Authenticated/LoggedIn）。折算钉住：
  `qr_statuses_fold_into_the_auth_state_machine`（Success→Authenticated、
  Waiting/Scanned/Expired→PendingQrScan，QR 过期为流程内重出码）。
  **事实核对（ask 中"998→Expired"与协议不符）**：B 站 passport qrcode/poll
  协议无 998 码，上游 voicefox `source/src/bili/mod.rs:239-241` 与本仓库
  fixtures 均只有 0/86101/86090/86038 四码，过期码是 86038；998 按
  未文档化码保持既有报错行为（`qr_poll_undocumented_code_errors_without_
  session_changes` 钉住 "QR poll failed (code=998)" 且会话不动）。
- **5 处 Unsupported 站点映射 `UnsupportedOperation`**（lyrics/playlist/
  categories/search_playlists/user_playlists，§1.3 typed 位落点）：provider
  id 进 typed 槽位、站点原因进 detail 槽位（legacy `Error::Unsupported`
  载荷答不了 `provider_id()`）；error.rs 两个 Display 实现不动（正文
  "unsupported operation" + detail 后缀），能力位保持 false（registration
  测试原样）。`unsupported_sites_map_to_unsupported_operation` 以
  category/provider_id/detail/Display 前缀逐站钉住。
- **有意不做（最小修改原则，netease 先例）**：`playlists()` 对既有标记的
  消费点（原 1164-1166 `perr("login session expired")`）保持 Provider 字符串
  ，netease 切片落地后其 `user_playlists` 同形消费点
  （source_netease.rs:690-692）同样保留，标记消费点不在 ask 范围。
- 新测试合计 9 个（见上），`cargo test --lib --features source-bilibili`
  52 passed 0 failed。

### 9.5 migu 切片落地记录（provider-error-migu，2026-11-19）

§8 切片 6 的落地证据，逐条对应本 ask 范围（source_migu.rs）：

- **NotFound 19 处盘点（ask 计数 19 = `Error::NotFound` 全文出现次数：15 个构造点
  + 4 处测试 `matches!` 断言，全库最多）逐点确认"资源缺失 vs 链路不可用"**。
  14 个构造点保持 `Error::NotFound`，全部是上游明文定义的资源缺失语义，逐点佐证：
  本地 track 记录无 contentId/copyrightId（`resource_id` 落空，url.rs:372
  `ok_or(FetchError::NotFound)` 同形）；link-info GET 的 `resource` 空数组/不可解析行
  （mg/song.rs:114-117）；album 空 songList（mg/parse.rs:125-133）；playlist 空
  首页（mg/playlist.rs:69-71，playlist()、playlist_tracks()、share-link 三面同源）；
  resourceinfo 资源行缺失（url.rs:396-399）；`newRateFormats` 缺失（url.rs:408-411）；
  目标 format/formatType 缺失（url.rs:441-448）；strategy 无 url（url.rs:232-234，
  中间腿，错误被调用方吞掉后回退 listenSong）；302 无 Location 与 preview Location
  （url.rs:272-281）。1 个构造点拆 `Error::Unavailable`：**resource POST 的
  terminal 429/5xx**（原 `Err(TransportError::Status(_)) => NotFound`，source_migu.rs
  原 1360）：这些状态已被 standard ladder 重试到穷尽（retry.rs:80-85），浮现时是
  "链路仍未恢复"而非资源缺失；其余非成功状态（404 等）保持上游映射。实现是三个纯
  函数 `ladder_exhausted_status`/`terminal_status_error`/`status_error_from_text`
  （source_migu.rs:200-231，netease `api_code_error` 纯缝先例），接线在
  resource POST（source_migu.rs:1428）与 listen 面（1623，`send_once` 不分类，直接
  用数值 status）。测试：`terminal_status_error_splits_unavailable_from_not_found`
  （429/500/502/503/504 → Unavailable 全载荷 + 400/403/404/410/418 → NotFound +
  非 "HTTP {code}" 文本防御臂）+ e2e `resource_post_status_error_maps_to_not_found`
  保持（404 穷尽路径，断言文案补准为 "ladder 族之外"）。
- **版权拦截响应映射 `CopyrightRestricted`**：落点是 listenSong 的非 3xx 拒绝体：
  上游明文 "接口用 JSON 里的 code 说明原因（VIP/无版权/参数问题）"（mg/url.rs:285），
  全库唯一 fixture 钉住的拒绝体形状就是 `{"code":"406001","msg":"no rights"}`。
  上游与旧代码把全部原因折叠为 NotFound；本切片以 `listen_refusal_error`
  （source_migu.rs:244）按拒绝文本键控（"right"/"版权"，msg 优先 info 兜底）：权利
  拒绝 → `Error::CopyrightRestricted { provider, detail: "listen url: code 406001,
  no rights" }`；其余（VIP/参数/未知码/非 JSON/空体）保持上游 NotFound 默认。不建
  码表（§9.2 kugou 先例：无 fixture 佐证的码值不扩表），VIP 拒绝不拆
  `VipRequired`（无钉住形状，关键词猜测会误分类平台文案，记录在案）。
  `listen_face_stays_single_shot_and_never_retries` 第一场景的钉死断言按 §1.2
  规则**显式翻新**（NotFound → CopyrightRestricted 全载荷，单发断言不变）并在此
  记录；第二场景 preview 302 → NotFound 原样保持；新增未知原因场景
  （"参数错误" → NotFound）。纯分类器测试
  `listen_refusal_classification_keys_on_the_rights_text` 覆盖英文/中文权利拒绝、
  info 兜底、五类保守回退。
- **RegionRestricted 决策：本切片无构造点**。`sid=USS` 是 search URL 的固定字面量
  （mg/search.rs:21，非会话/地区句柄），且上游与 fixtures 全库不存在任何可键控的
  地区限制响应形状（search 的 code gate 只见 "未登录或无权限"，且该测试在受保护
  区间内逐字节保持 Provider 字符串）。按 tasks.md §15.4 与 §9.2 先例不为无佐证的
  响应形状扩面；`ErrorCategory::RegionRestricted` 留给有真实响应证据的 provider。
- **死 SessionStore 决策：删除**。auth-session 落地后评估原 source_migu.rs:95 字段
  与 139-145 存取器：全文件无任何读写（唯一接触面是两个 pub 方法本身），上游 mg
  无任何 login/auth/session 状态（`sid` 与 deviceId 都是固定字面量）；"接入匿名设备
  sid 语义"只会把静态常量包装成永远为空的假会话状态，违反最小修改的诚实性。
  落地：字段、`with_session_store`/`session_store()` 两个 pub 方法、Debug 脱敏项
  全部移除（模块 doc 明文记录决策与理由，source_migu.rs:10-15）。外部使用面核查：
  examples/、tests/、docs/ 对 migu 存取器 0 命中。**API 变化明示（breaking）**：
  `MiguProvider::with_session_store/session_store` 是本 campaign 首个有意删除的
  provider 公共方法；migu 无 AuthenticationProvider impl、
  `capabilities().authentication` 恒 false（registration 测试原样保持）。
- **兼容策略执行记录**：受保护区间（原 2431-2698）钉死中文消息的测试逐字节保持：
  `search_response_fixture_parses_dedup_totals_and_errors`（"search: 未登录或无权限"）、
  `chart_error_gates_surface_the_info_text`（"找不到资源"×2/"咪咕榜单目录请求失败"）、
  `rank_index_empty_contents_is_an_upstream_error`（"咪咕未返回可用榜单"）、
  `chart_board_missing_contents_is_an_upstream_error`（"咪咕榜单歌曲列表为空"）、
  `album_page_fixture_parses_rows_metadata_and_traps`（空 data → NotFound）、
  `link_target_fixtures_follow_upstream_parse`（InvalidInput），8 项过滤实跑全绿；
  registration 测试（原 2200）原样保持。search/chart 的 code gate 保持 `perr`
  （catch-all 迁移不在本 ask 范围）。新增测试 3 个 + 显式翻新 1 个。
- **门禁证据（本轮实跑）**：`cargo test --locked --features source-migu --lib`
  138 passed 0 failed；4 个证据测试逐个过滤实跑全绿（上列 3 新 + 1 翻新）；
  `cargo clippy --lib --features source-migu -- -D warnings` 与
  `--no-default-features` 均 0 warning；`cargo check --lib --all-features` 与
  `--no-default-features` 均 Finished；`cargo fmt --all` 已跑且
  `cargo fmt --all -- --check` exit=0。

### 9.6 轻认证三源落地记录（provider-error-light，2026-11-19）

§8 切片 7 的落地证据，逐条对应本 ask 范围（source_jamendo.rs /
source_apple.rs / source_fivesing.rs）：

- **jamendo 9 处 Unsupported 按语义二分**（bilibili §9.4 的 typed 落点形状）：
  - **5 个 container 面 → `Unavailable`**（album / album_tracks / playlist /
    playlist_tracks / resolve_link，经 `container_unavailable` 纯缝）：这些面
    是**已实现**的，拒绝原因是 `/api/albums`、`/api/playlists`、`/api/tracks`
    自 2026-11-17 实测起匿名挂起；模块注释原文 "until Jamendo restores
    them" 明文这是暂时性上游条件，不是 build 未实现，故落 `Unavailable`
    （retryable=true，恢复 = 上游恢复后的既定重试阶梯）而非
    `UnsupportedOperation`；迁移前拒绝文本
    "jamendo container endpoints are unavailable anonymously" 原样保留进
    detail 槽位（§1.2）。capability 位（albums/playlists/link_resolve）
    保持 false 不动，查能力的调用方照旧到不了这些面。
  - **4 个平台无端点面 → `UnsupportedOperation`**（categories /
    playlists 目录 / user_playlists / lyrics，经 `unsupported_operation`
    纯缝，§1.3 typed 位落点）：provider id 进 typed 槽位、站点原因进
    detail 槽位（legacy `Error::Unsupported` 载荷答不了 `provider_id()`）：
    "no playlist category endpoint exists upstream" /
    "no playlist directory endpoint exists upstream; search covers
    discovery"（原注释照搬）/ "no account system; upstream exposes no user
    playlist endpoint" / "the CC catalog exposes no lyrics"。
  - resolve_link 的非法链接解析守卫（`InvalidInput`）保持在 container 拒绝
    之前。`unsupported_sites_split_into_unavailable_and_unsupported_operation`
    以 category/provider_id/detail/Display/is_retryable 逐站钉住两面（5+4），
    并钉住链接解析守卫次序。
- **apple preview-only 语义：选文档化，不用 `UnsupportedQuality`**（ask 给的
  二选一）：
  - 落点 1：`capabilities()` 的 stream_resolve 注释补全为完整契约：
    resolve() 无视请求档位恒服务 30 秒预览（`StreamQuality::Low`）、达成档
    位经 `StreamInfo.quality` 呈现、每条 track 的 `available_qualities` 恒
    `[Low]`。
  - 落点 2：`supported_qualities()` 覆写为 `vec![StreamQuality::Low]`，
    registry descriptor 的 §5.4 质量档位 facet 携带同一真相（空 = 未声明会
    低估模块 doc 首段明文写死的契约）。
  - `UnsupportedQuality` 被拒的理由（记录在案）：上游移植对任意请求档位都
    服务预览（钉死的 resolve 测试即以 `ResolveOptions::default()`（=High）
    解析成功），>Low 即拒会破坏钉死的迁移契约；达成档位已对宿主可见。
    `preview_only_contract_is_declared_on_the_quality_facet` 钉住双 facet。
- **apple / fivesing login_status 恒 LoggedOut 显式映射**：两处均改为
  `Ok(AuthState::LoggedOut.to_login_status(None))`（§2.3 文档化折算），
  替换手写 `LoginStatus::LoggedOut` 字面量；logout 仍为空操作注销边；
  validate_session 走默认本地推导（LoggedOut）。
  `login_status_is_the_explicit_logged_out_state_fold`（apple）与
  `auth_slot_is_registered_with_an_explicit_none_facet`（fivesing）钉住。
- **fivesing `code != 1000` 门按语义二分**（`stream_url_gate_error` 纯缝，
  netease `api_code_error` / migu `listen_refusal_error` 先例）：无任何拒绝
  码表（上游 api.rs:272-274 把全部拒绝折叠为一个字符串；fixtures 唯一实测
  拒绝形状是 2026-11-18 `{"code":1001,"message":"缺少songid","success":false}`），
  故按信封自身的拒绝声明键控：
  - `"success": false`（端点声明性拒绝本次 stream-url 授权）→
    `AuthenticationRequired`（不可重试：重发不解除平台拒绝，恢复所需身份是
    这个无登录 provider 永不执行的平台登录；code 与平台 message 进 detail
    槽位，如 "stream url: code 1001, 缺少songid"）；
  - 其余 `code != 1000` 形状（无声明性拒绝，脱契约信封）→ `Unavailable`
    （retryable，标准阶梯重发仍是合理恢复）。该臂今日为防御位（kugou §9.2
    先例：doc 明文），无任何 capture 佐证其形状，由合成信封在
    `stream_url_gate_splits_declared_refusal_from_off_contract_envelope`
    钉住。
  - **消息钉死点翻新记录（ask 点名 fivesing.rs:935-936 核对）**：
    `fixture_get_song_url_gate_rejects_non_1000` 按设计 §8 规则显式翻新：
    旧钉死 `Error::Provider` 全文 "provider fivesing failed: stream url
    request failed" 被 M2 变体替代，Display 正文按 §1.2 两个逐字节例外之一
    收敛为 "login required"，detail 保留 code/message；新钉死全载荷
    （category/provider_id/detail/is_retryable/Display）。
    `resolve_stream_url_gate_refusal_is_categorized` 端到端钉住 200 信封
    携带 1001 拒绝形状时 resolve 呈现分类错误且单发不重试。
    song_info 的 SN 门（NotFound）与 playlist_info 的 data 门
    （"5sing playlist missing or inaccessible"，fixture 钉死）不在 ask
    范围，保持原样。
- **fivesing 死 AuthenticationProvider impl 处置：选"补 register+capability"**
  （ask 二选一）：`register()` 增 `registry.register_authentication(provider)`
  （qq/netease/bilibili/kugou 注册形状），`auth_capability()` 显式覆写
  `AuthCapability::None`（kuwo §9.3 explicit-facet 先例的 None 用例），
  `capabilities.authentication` bool 保持 false（注册测试原样钉住）。选此不
  选"注销 impl"的理由：(1) ask 的 login_status 显式映射项同时点名
  apple/fivesing，注销后 fivesing 无可映射面；(2) 注销将连带孤儿化
  `with_session_store/session_store` 构造面，属 ask 未授权的 API 破坏
  （migu §9.5 删除先例有对应 ask 指令，此处无）；(3) 注册后恒 LoggedOut
  视图经 registry auth 槽位可达，宿主统一查询得到诚实回答而非缺槽。
  `auth_slot_is_registered_with_an_explicit_none_facet` 钉住 facet/bool/
  login_status/logout/validate_session 全链。
- **apple 错误站点迁移补记（slice m2-apple-error-categories-unwired，
  2026-11-20）**：本节首版只给 apple 落了 preview 双 facet 与 login_status
  显式映射；apple 的全部 perr 站点（src/source_apple.rs 的 `perr` 构造点，
  共 10 个）当时仍落 legacy `Error::Provider` 全捕获，IMPLEMENTATION_STATUS.md
  M2 记录的 "jamendo/apple/fivesing (categorized errors)" 对 apple 失实。
  本轮按 jamendo §9.6 形状补齐迁移（provider id 进 typed 槽位、迁移前文本
  逐字节进 detail 槽位），`perr` 零调用后删除（`Error::Provider` catch-all
  对 apple 归零；NotFound/InvalidInput 等旧变体站点不动）。10 站三类：
  - **2 个 terminal Status 臂（raw_text / amp_get 的 `TransportError::Status`
    分支）→ `terminal_status_error` 纯缝按 ladder 语义二分**（migu §9.5
    同名纯缝先例）：429/5xx = 标准阶梯已重试到穷尽（retry.rs:80-85）→
    `Unavailable`（retryable，重发是既定恢复）；其余非成功态 = 码表外未映射
    状态 → `InvalidResponse`（不可重试：过期 token 的 401 重发亦复现，恢复
    是守卫窗口后的下一次 crawl）。apple 无上游状态表可继承（其迁移前折叠
    就是 `Error::Provider` catch-all 本身），故无 migu 的 NotFound
    carve-out（kugou §9.2 不造表规则）；`code` 槽恒 `None`（HTTP 状态不进
    平台 API 码命名空间，joox/qianqian 先例），状态文本随 detail 槽位走
    （"{label}: HTTP {code}"，与 transport 迁移钉死文本逐字节一致）。
  - **2 个 send 臂（ladder 穷尽后的发送失败）→ `Network`**（retryable，
    §1.1 连接/DNS/TLS 层），detail 保留 "{label}: {error}"（joox
    `send_get` 先例；防御位：该臂也接 `Build`，其文本随同一槽位保留，
    apple 的 URL 全部由常量 + urlencoding 构造，现实臂体是发送失败）。
  - **1 个解析失败臂 → `InvalidResponse`**（§1.1 不可解析类），detail 保留
    "AMP response parse failed: {serde_json message}"（公共目录 JSON，无
    凭据材料）。
  - **2 个 token 站 → `Unavailable`**（retryable）：每分钟轮换守卫
    （"Apple Music token unavailable; retry shortly"，守卫文案本身即
    暂时性条件）与 JWT crawl 穷尽（"no usable Apple Music token found"，
    恢复 = 守卫窗口后的下一次 crawl）。
  - **3 个契约违反站 → `InvalidResponse`**：homepage 无已知 script 标记
    （"homepage script not found (page layout changed)"）、script 无 JWT
    候选（"script contains no token candidates"）、song detail 无 previews
    节点（"no preview available for this track"；模块契约明文 preview 是
    唯一可得流，缺字段属 §1.1 InvalidResponse，不是缺资源的 NotFound）。
  - **测试证据**：新增 3 个：`terminal_status_error_splits_ladder_
    exhaustion_from_unmapped_status`（纯缝钉 429/500/503→Unavailable
    retryable、401/403/404→InvalidResponse 不可重试、防御臂保文），
    `raw_text_send_failure_lands_in_network_with_today_text`（脚本 `Build`
    单发钉 Network 臂，引擎不重试 Build（transport.rs:203-205），无需
    真实 backoff sleep），`resolve_without_previews_lands_in_invalid_
    response`（无 previews 的 detail 端到端钉 InvalidResponse）；
    按 §1.2 翻新规则显式翻新 5 个（Display 全句 "provider applemusic
    failed: …" 换 M2 形态，transport 迁移文本逐字节保留进 detail 槽位）：
    `raw_text_fatal_status_keeps_today_error_text`（404→"invalid response:
    homepage: HTTP 404"）、`amp_get_fatal_status_keeps_today_error_text`
    （401→"invalid response: AMP request: HTTP 401"）、
    `amp_get_parse_failure_keeps_label_text`（前缀换 "invalid response:
    AMP response parse failed: "）、`token_probe_stays_single_shot_when_
    every_candidate_fails`（→"provider unavailable: no usable Apple Music
    token found"）、`token_rotation_guard_blocks_immediate_recrawl`（两臂
    分别换 InvalidResponse/Unavailable 形态）。
  - **本轮门禁证据（2026-11-20 实跑）**：`cargo test --locked --features
    source-apple` 158 lib + 1 integration passed 0 failed（apple 过滤 31 =
    原 28 + 新 3）；`cargo clippy --lib --features source-apple
    -- -D warnings` 0 warning；三源回归 `cargo test --locked --features
    source-jamendo,source-apple,source-fivesing --lib` 203 passed
    0 failed；`cargo fmt --all` 已跑。
- **兼容策略执行记录（首版）**：三源 registration 测试（jamendo/apple/
  fivesing 的 `registration_exposes_implemented_capabilities`）零改动全绿；
  fivesing playlist_info 钉死文本（"5sing playlist missing or
  inaccessible"）、jamendo/apple 的 transport 迁移钉死文本（"api request:
  HTTP {status}" 等）在首版全部未触碰（apple 的同文本于 2026-11-20 补记
  轮按 §1.2 显式翻新进 detail 槽位，见上）。首版新测试合计 7 个（上列），
  翻新 1 个（fivesing 门钉死）。
- **门禁证据（本轮实跑）**：`cargo test --locked --features
  source-jamendo,source-apple,source-fivesing` 170 lib + 1 integration
  passed 0 failed；7 个证据测试逐个过滤实跑全绿（上列 6 新 + 1 翻新）；
  3 个 registration 测试过滤实跑全绿；`cargo clippy --lib --features
  source-jamendo,source-apple,source-fivesing -- -D warnings` 0 warning；
  `cargo check --lib --all-features` / `--no-default-features` 均 Finished，
  `cargo clippy --lib --all-features` / `--no-default-features`
  （-D warnings）均 0 warning；`cargo fmt --all` 已跑且
  `cargo fmt --all -- --check` exit=0。

### 9.7 download 桥落地记录（download-error-bridge，2026-11-19）

§8 切片 8 的落地证据，全部改动在 `src/download.rs`（`download` feature 域内），
`error.rs`/`retry.rs`/`transport.rs` 零改动：

- **引擎七变体原样保留**：`DownloadError`（download.rs:107-123）与
  `DownloadError::is_retryable`（download.rs:126-138）逐字节未动。引擎内部
  循环（`download_to` 的单连接回退、`download_part` 的逐片重试）依赖它的
  策略族语义（TRANSPORT_DESIGN.md §6：两族策略不合并，`RangesUnsupported`
  是"切策略"不是"重发"，408 入引擎重试集但 `classify_http` 不重试）。引擎
  测试零改动：`download::` 过滤 27 通过 = 原 24 个 + 新增 3 个桥测试。
- **映射 impl（ask 钉死七行 + 两个声明残差臂）**：新增独立
  `impl DownloadError`，`category() -> ErrorCategory`
  纯函数：`Network→Network`、`Http{429}→RateLimited`、`Http{5xx/408}→
  Unavailable`（5xx/408 判定收敛为私有 `is_unavailable_status`，与引擎
  `*status >= 500 || *status == 408` 同源，两面不漂移）、
  `Cancelled→Cancelled`、`Integrity→ProviderInternal`、`Io→Io`、
  `RangesUnsupported→UnsupportedOperation`。残差为声明决策而非疏漏：
  `Http` 钉死集外状态（403/404/…）与 `Other` catch-all 落
  `ErrorCategory::Provider`（§1.1 的未分类兜底），不为 ask 未钉死的状态
  发明语义；需要更细 HTTP 分类的宿主直接 match 公开的 `DownloadError`。
- **桥函数按需引入、无 `From` 强转**：`to_error(&self, provider: &str) ->
  Error`（形状对齐 M1 面 `TransportError::into_provider_error` 先例），
  宿主在边界显式调用；`category()` 供只要分类不要值的宿主。文本保留
  （§1.2 "不得静默丢弃"）：engine 载荷全部到达 M2 面，`Network`/
  `Integrity`/`RangesUnsupported`/已分类 `Http` 状态走 `detail` 槽位，
  `Io` 走 legacy `Error::Io(io::Error)` 载荷，残差 `Http`/`Other` 走
  `Error::Provider.message`（"download: HTTP {status}" 形状）。`retry_after`
  恒 `None`（`DownloadError::Http` 不携带服务端时长）；`Cancelled` 桥到
  无载荷 `Error::Cancelled`，`Io` 桥到无 provider 槽位的 legacy `Error::Io`，
  两行 `provider_id()` 恒 `None`（映射的固有属性，测试明文）。引擎只产出
  错误字符串（不产出 header/cookie），与 `ErrorDetail` 构造方契约的暴露
  级别一致。
- **is_retryable 语义对齐**：M2 面（`to_error(...).is_retryable()`）完全
  遵循 `Error::is_retryable`（`Network|RateLimited|Unavailable` 为 true，
  其余 false）；引擎面保持原策略。两个声明分歧点（Io：引擎重试瞬态磁盘
  错误、M2 面 `Io` 不可盲重发；RangesUnsupported：引擎"可重试"= 策略切换、
  M2 面 `UnsupportedOperation` 不可盲重发）由
  `engine_retry_policy_and_m2_face_diverge_only_where_declared` 双向钉住，
  且每个 M2 可重试行都同时是引擎可重试行（无反向矛盾）。
- **§6 边界保持**：download.rs 对 `crate::transport` 零引用（grep 全文件
  仅命中方法 doc 注释里的措辞，无 `use`/路径引用）；无
  `impl From<DownloadError>`。
- **门禁证据（本轮实跑）**：`cargo test --locked --features download
  download::` 27 passed 0 failed（原 24 + 新 3，既有测试零改动）；3 个新
  测试逐个过滤实跑全绿；`cargo clippy --locked --features download --lib
  --all-targets -- -D warnings` 0 warning；`cargo check --locked
  --no-default-features --features download` / `--lib --all-features` /
  `--lib --no-default-features` 均 Finished exit 0；`cargo fmt --all` 已跑
  且 `cargo fmt --all -- --check` exit=0。外层脚本门禁
  （`cargo test --locked --features download` 全量与
  `clippy --features download`）由外层负责。

### 9.8 lx-js 桥落地记录（lx-js-error-mapping，2026-11-19）

§8 切片 9 的落地证据。改动面：`src/lx_js_engine.rs`（14 处 catch-all 分类 + 3 个分类
构造器 + 2 个可测纯函数 + 测试）、`src/lx_js_provider.rs`（类别层测试钉）、
`docs/LX_JS_BRIDGE.md`（Results 行同步）。`error.rs`/`lx_js.rs`/`retry.rs` 零改动。

- **14 处 catch-all → 三类分类落点（ask 锚点：JS 沙箱错误→ProviderInternal、
  契约缺失→UnsupportedOperation，其余 JS 侧违反响应契约→InvalidResponse）**：
  - `ProviderInternal` ×10：spawn 面 5 处（data dir 创建 lx_js_engine.rs:75、wrapper
    写入 :78、engine spawn :91、stdin/stdout 取管 :95/:99）、管道面 3 处（request
    serialize :118、engine write :124、engine read :133 与 "engine closed the stream"
    :136）、`handler_failure`（round_trip ok:false，:196），ok:false 是**契约内**响应
    （包裹层无违约），失败原因是沙箱内 JS handler 自身（自由文本，不解析发明语义，
    §9.7 残差先例），结构上唯一诚实的落点是"provider 自身内部错误"；含 request
    serialize（本地桥层内部失败，label 文本经 detail 保留）。
  - `InvalidResponse` ×2：响应行不可解析（`parse_response_line` parse 失败）与
    响应 id 失序（有序响应契约违约），设计 §1.1 "缺字段/码表外/不可解析" 的桥版。
  - `UnsupportedOperation` ×2：ok:true 但契约强制结果产物缺失（"engine returned no
    url"/"engine returned no lyric"），ask 锚点"契约缺失"的唯二落点；与
    InvalidResponse 的分界：包裹层违约（id/可解析性）→ InvalidResponse，操作产物
    缺失（url/lyric）→ UnsupportedOperation。
  - 兼容不变量：旧 `Error::Provider` 与三个新落点全部不可盲重发（retry 语义零变化）；
    provider_id() 等价迁移（旧 `Error::Provider { provider: "lx-js" }` 已带 id，新面
    `provider: Some("lx-js")` 同值）；消息文本逐字节搬入 `detail` 槽位（§1.2
    "不得静默丢弃"；Display 前缀从 "provider lx-js failed: X" 变为类别正文，
    仓库内无钉死旧前缀的测试：grep "lx-js failed" 全仓库零命中，实核）。
- **可测性最小重构**：`read_response` 的 parse+id 校验抽为纯函数
  `parse_response_line(line, expected_id)`（无子进程即可驱动两类契约违约）；
  ok:false 分支抽为 `handler_failure(label, error)`；三个分类构造器
  `sandbox_error`/`response_contract_error`/`missing_result_error` 收敛 14 处
  构造。行为与消息文本不变（仅构造点换分类）。
- **`lx_js.rs` seam 声明残差（ask 命名范围外，最小修改原则保留）**：
  `response_value`（lx_js.rs:62-79）的 2 处 `Error::Provider` catch-all 本切片
  **不迁移**，ask 范围钉死 "lx_js_engine.rs 14 处 + lx_js_provider.rs 5+3 处"；
  seam 是任意宿主引擎的响应翻译层，其 `response_errors_become_provider_errors`
  测试钉住现行为。宿主若用 NodeJsEngine 直连（round_trip 面）即得分类值；
  经 `JsBridgeEngine` + `response_value` 面则保持 legacy catch-all。两者分歧由
  本条明文记录，LX_JS_BRIDGE.md Results 行同步注明。
- **lx_js_provider.rs InvalidInput/NotFound（5+3 处）映射类别**：M2 矩阵
  （§1.1）无 `InvalidInput`/`NotFound` 的 typed 等价变体（两者本就是 legacy
  变体行），构造点零改动；类别层经 `Error::category()` 钉住：构造点
  lx_js_provider.rs:84/279/352（InvalidInput）与 :324/:326（NotFound）由三个
  既有测试扩展断言 + 新测试 `bridge_input_and_not_found_faces_carry_stable_categories`
  全覆盖（category + provider_id()=None（§1.3 legacy 无 provider 槽位，声明决策）
  + is_retryable()=false + user_hint 非空）。计数：InvalidInput 3 构造 + 2 既有
  断言 = 5，NotFound 2 构造 + 1 既有断言 = 3，与 ask "5+3 处" 一致。
- **兼容策略逐条核对**：契约测试 `capabilities_match_the_contract` 零改动保持
  （lx_js 过滤实跑绿）；JS 引擎行为（WRAPPER_SCRIPT 逐字节未动，wrapper 测试零改动）
  与消息文本不变（`bridge_errors_classify_per_the_host_contract` 逐字节断言
  detail 文本）；仅增加类别层。
- **门禁证据（本轮实跑）**：`cargo test --locked --features lx-js-bridge --lib lx_js`
  22 passed 0 failed（原 19 + 新 3：`bridge_errors_classify_per_the_host_contract`、
  `envelope_violations_classify_as_invalid_response`、
  `bridge_input_and_not_found_faces_carry_stable_categories`；既有测试零改动）。
- **门禁证据（本轮实跑）**：`cargo fmt --all` 已跑且 `cargo fmt --all -- --check`
  exit=0；`cargo test --locked --features lx-js-bridge` 全量：lib 91 passed
  0 failed + 集成 qr_terminal_png 1 passed 0 failed（合计 92 passed 0 failed）；
  `cargo clippy --locked --features lx-js-bridge --lib --all-targets` 0 warning
  （初轮 1 个 `unused import: ErrorCategory`，移入 tests 模块后复跑归零）；
  7 个证据测试逐个 `--exact` 过滤实跑全绿（上列 3 个新测试 + 既有
  `capabilities_match_the_contract` 与三个类别断言扩展测试）。
