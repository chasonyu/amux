# Provider SPI 化与 OMP 无行为变化迁移方案

Date: 2026-08-14
Status: implemented（verification complete）
Scope: 仅建立编译期 Provider SPI，并把现有 OMP 链路完整迁入 SPI；不接入 Codex，不改变用户配置、快捷键、会话文件或 UI 行为。

## 目标

把当前散落在 `SessionSupervisor`、`shell::App` 和 `provider::*` 中的 OMP 专用逻辑收口为一个内部、编译期注册的 `AgentProvider` SPI。迁移完成后，新增 Provider 只需要实现 SPI、注册实现和增加 Provider 选择 UI，不得再向 `SessionSupervisor`、PTY 或通用 Shell 流程写 Provider 名称分支。

本步骤的首要约束是 **OMP 行为等价**，不是顺手重构：

- `omp` 命令、参数顺序、环境变量和值完全不变。
- `~/.amux/config.json`、`workspaces.json`、lock 文件和 `~/.omp` 数据格式完全不变。
- 新建、恢复、切换、后台保活、关闭、删除、重命名、标题刷新、fork/branch 重绑定、busy/unread、transcript、modified-files、主题、鼠标和输入转发行为完全不变。
- 本步骤只注册 `omp`；不新增 Provider 选择入口，不改变默认 Provider。
- 不引入动态库、Wasm、外部 Provider RPC 协议或 command-template Provider。

## 不变量与完成标准

1. `SessionSupervisor` 不再 import `OmpProvider`、`OmpDiskSession` 或 OMP JSONL 辅助函数。
2. `shell::App` 不再直接调用 `write_session_title`、`delete_session_with_artifacts`、`refresh_disk_session`、`session_dir_for_cwd`、`list_omp_sessions` 等 OMP 函数。
3. `PtySession` 仍由 `SessionSupervisor` 创建、持有、resize 和终止；Provider 只返回 `SpawnSpec`，不得持有 PTY。
4. 所有 live map、锁和 UI 选择使用 `SessionKey { provider, session_id }`，不再只用裸 session ID。
5. OMP 原有解析器和算法原样保留并从 `OmpProvider` 调用；不重写 cwd 编码、title slot、fork 匹配、turn status、transcript 或 diff 算法。
6. 现有 147 个非 ignored 测试在清除外部 `COLORFGBG` 后全部通过；新增 SPI 回归测试全部通过。
7. 真实 OMP 交互回归矩阵全部通过后，才允许合并。

---

## 1. 要修改的模块或文件

### 1.1 新增 `src/provider/api.rs`

职责：只定义 Provider 中立契约和数据模型，不依赖 OMP 模块。

新增类型：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProviderId(&'static str);

impl ProviderId {
    pub const OMP: Self = Self("omp");
    pub const fn new(id: &'static str) -> Self;
    pub const fn as_str(self) -> &'static str;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub provider: ProviderId,
    pub session_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleSource {
    Official,
    Provisional,
    Fallback,
}

#[derive(Debug, Clone)]
pub struct ProviderSession {
    pub key: SessionKey,
    pub title: String,
    pub title_source: TitleSource,
    pub parent_ref: Option<String>,
    pub path: Option<PathBuf>,
    pub cwd: PathBuf,
    pub modified_at: DateTime<Utc>,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderCapabilities {
    pub rename: bool,
    pub delete: bool,
    pub transcript: bool,
    pub modified_files: bool,
    pub live_rebind: bool,
}

pub enum LiveRenameAction {
    WritePty(Vec<u8>),
    Persisted,
}

pub enum ProviderChange {
    Upsert(ProviderSession),
    Removed(SessionKey),
    Rescan,
}
```

`AgentProvider` 使用同步、对象安全接口；amux 当前 event loop 和 Provider 文件访问都是同步模型，不为未来 Provider 提前引入 async runtime：

```rust
pub trait AgentProvider {
    fn id(&self) -> ProviderId;
    fn display_name(&self) -> &'static str;
    fn capabilities(&self) -> ProviderCapabilities;

    fn available(&self) -> Result<()>;
    fn list_sessions(&mut self, cwd: &Path) -> Result<Vec<ProviderSession>>;
    fn spawn_new(&self, cwd: &Path) -> Result<SpawnSpec>;
    fn spawn_resume(&self, cwd: &Path, session_id: &str) -> Result<SpawnSpec>;

    fn check_external_occupant(&self, session_id: &str) -> Result<()>;
    fn parent_refers_to(&self, parent_ref: &str, session_id: &str) -> bool;
    fn session_busy(
        &mut self,
        session: &ProviderSession,
        live: bool,
        pty_active: bool,
    ) -> bool;
    fn forget_session(&mut self, key: &SessionKey);
    fn normalize_title(&self, draft: &str) -> Result<String>;

    fn rename_live(&mut self, session: &ProviderSession, title: &str)
        -> Result<LiveRenameAction>;
    fn rename_stored(&mut self, session: &ProviderSession, title: &str) -> Result<()>;
    fn delete_stored(&mut self, session: &ProviderSession) -> Result<()>;

    fn select_workspace(&mut self, cwd: Option<&Path>) -> Result<()>;
    fn poll_changes(&mut self, now: Instant) -> Result<Vec<ProviderChange>>;
    fn next_deadline(&self) -> Option<Instant>;

    fn load_transcript(&mut self, session: &ProviderSession)
        -> Result<Vec<TranscriptBlock>>;
    fn modified_files_scanner(
        &mut self,
        session: &ProviderSession,
    ) -> Result<Option<Box<dyn ModifiedFilesScanner>>>;
}
```

约束：

- `ProviderId` 只接受实现中定义的 `'static` ID；配置字符串通过 registry 查找，不在每个 session 上重复分配 Provider 名称。
- `SpawnSpec` 刻意沿用 `PtySession::spawn` 当前的 UTF-8 `String` 契约；OMP 实现继续使用现有 `to_string_lossy`，本步骤不借 SPI 重构改变非 UTF-8 路径行为。
- `ProviderSession.path` 是可选的缓存/展示元数据，不作为 SPI 身份；身份始终是 `SessionKey`。
- 不给 SPI 方法提供默认 no-op。Provider 必须明确实现；不支持的 UI 操作由 `ProviderCapabilities` 隐藏，直接调用则返回明确错误。

### 1.2 新增 `src/provider/registry.rs`

职责：构造、注册和查找编译期 Provider；不负责 PTY 或 UI。

```rust
pub struct ProviderRegistry {
    default: ProviderId,
    providers: HashMap<ProviderId, Box<dyn AgentProvider>>,
}
```

首版只实现：

```text
ProviderRegistry::from_config(&AmuxConfig)
  └── OmpProvider::from_config(config)
```

必须提供：

- `default_id()`：本步骤恒为 `ProviderId::OMP`。
- `get()` / `get_mut()`：未知 ID 返回包含 ID 的错误。
- `register()`：拒绝重复 ID，防止后注册实现静默覆盖。
- `ids()`：供未来 Provider 选择 UI 使用，本步骤不接 UI。

### 1.3 修改 `src/provider/mod.rs`

- 导出 `api`、`registry` 和中立类型。
- `OmpProvider` 保持公开，但 Shell 和 SessionSupervisor 不再直接依赖它。
- OMP 文件函数仅供 `provider::omp` 内部或其单元测试使用，不再从 `provider/mod.rs` 作为 Shell API 批量 re-export。
- `TranscriptBlock`、`ModifiedFile`、`DiffLine` 等中立展示类型继续从 transcript 模块导出。

### 1.4 修改 `src/provider/omp.rs`

`OmpProvider` 实现 `AgentProvider`，但保留以下现有算法和文件协议：

- `sessions_root()` / `encode_cwd_key()` / legacy cwd bucket 合并。
- `list_omp_sessions()` 和 `read_disk_session()`。
- 256-byte title slot 的读取、覆盖和 legacy prepend。
- 第一条 user message 的 provisional title。
- `parent_refers_to()` 对 UUID/path 的匹配。
- JSONL + sibling artifacts 删除及删除后重试。
- profile 和 session-dir 参数。

映射要求：

| 现有 OMP 行为 | SPI 映射 |
|---|---|
| `omp --cwd <cwd>` | `SpawnSpec { program: omp_bin, args: ["--cwd", cwd, common...], ... }` |
| `omp --cwd <cwd> --resume <id>` | `spawn_resume`，参数顺序完全不变 |
| `PI_*` pins | `SpawnSpec.env`，名称和值完全不变 |
| `TitleKind` | `TitleSource` 一一映射 |
| `OmpDiskSession` | `ProviderSession` |
| live `/rename` | `LiveRenameAction::WritePty(b"\x15/rename {title}\r")` |
| disk rename | `rename_stored` 调现有 `write_session_title` |
| delete | `delete_stored` 调现有 `delete_session_with_artifacts` |
| external resume 检测 | `check_external_occupant` 调 OMP 专用 pgrep 逻辑 |

`OmpDiskSession` 可以保留为 OMP 私有解析模型，但离开 `OmpProvider` 前必须转换为 `ProviderSession`。

### 1.5 修改 `src/provider/watch.rs`

把当前只暴露文件路径的 watcher 封装成 OMP Provider 的变更源，同时保持原参数：

- 当前 workspace 目录。
- 约 80ms debounce。
- 3s fallback poll。
- CREATE/DELETE/overflow 触发 full rescan。
- 已知 JSONL MODIFY 优先转成单 session `Upsert`。

允许保留底层 `SessionDirWatcher` 类型，但它只能被 `OmpProvider` 或 OMP monitor 持有。`shell::App` 不再持有 `SessionDirWatcher`、dirty path、title debounce 或 fallback poll 状态。

为保持事件循环响应速度，`OmpProvider::next_deadline()` 必须暴露 debounce deadline；`App::next_timeout()` 通过 `SessionSupervisor` 读取该 deadline。

### 1.6 修改 `src/provider/turn_status.rs`

- 保留所有 OMP JSONL 状态推导算法和 `SessionActivityTracker`。
- 不把 `DiskTurnStatus` 提升为公共 Provider 状态。
- `SessionActivityTracker` 移入 `OmpProvider`，由 `session_busy(session, live, pty_active)` 调用现有 `busy` 算法；`SessionSupervisor` 不判断 OMP JSONL。
- close、delete 和 rebind 时，`SessionSupervisor` 调用 `forget_session` 清理 Provider 活动状态；OMP 实现继续按裸 session ID 清理现有 tracker。
- 未来 Provider 可以从自己的状态源计算 busy，不必伪装成 OMP JSONL。

### 1.7 修改 `src/provider/transcript/mod.rs` 与 `src/provider/transcript/omp.rs`

- `TranscriptBlock`、渲染和 markdown 继续保持 Provider 中立。
- `load(provider: &str, path)` 的字符串分发改为通过 `AgentProvider::load_transcript` 调用；OMP 实现仍调用现有 `transcript::omp::load(path)`。
- 把 `ModifiedFilesScan` 的 Shell 持有方式收窄成对象安全的 `ModifiedFilesScanner`：

```rust
pub trait ModifiedFilesScanner {
    fn advance(&mut self, session: &ProviderSession) -> Result<bool>;
    fn version(&self) -> u64;
    fn files(&self) -> &[ModifiedFile];
    fn render_diff(&self, file_index: usize) -> Vec<DiffLine>;
}
```

OMP scanner 适配现有增量扫描器，不重写 diff 聚合和预算限制。未来不支持 modified-files 的 Provider 返回 `Ok(None)`。

### 1.8 修改 `src/session/mod.rs`

`SessionSupervisor` 从：

```text
OmpProvider + AmuxConfig + HashMap<String, LiveEntry>
```

迁移为：

```text
ProviderRegistry + HashMap<SessionKey, LiveEntry>
```

Supervisor 对 Shell 暴露中立增量协议：

```rust
pub enum SessionSummaryChange {
    Upsert {
        summary: SessionSummary,
        became_idle: bool,
    },
    ReplaceAll {
        sessions: Vec<SessionSummary>,
        rebinds: Vec<(SessionKey, SessionKey)>,
        became_idle: Vec<SessionKey>,
    },
}

impl SessionSupervisor {
    pub fn select_provider_workspace(
        &mut self,
        provider: ProviderId,
        workspace_id: &str,
        cwd: Option<&Path>,
    ) -> Result<Vec<SessionSummary>>;

    pub fn poll_provider_changes(
        &mut self,
        now: Instant,
    ) -> Result<Vec<SessionSummaryChange>>;

    pub fn next_provider_deadline(&self) -> Option<Instant>;
}
```

调用和消费顺序固定如下：

1. startup、add/remove/move workspace 或 selected workspace 改变时，Shell 先调用 `select_provider_workspace`，再替换 `session_list`。
2. `select_provider_workspace` 先让旧 Provider change source 停止并丢弃旧 workspace 队列，再调用 `provider.select_workspace(cwd)`，最后执行初次 `list_sessions`；`cwd=None` 清空监听和列表。
3. 已知 session 的 `ProviderChange::Upsert` 由 Supervisor 合并 live title 优先级、计算 busy、识别 busy→idle，输出 `SessionSummaryChange::Upsert`。
4. 未知 session 的 Upsert、Removed 或 Rescan 执行现有 full reconcile，包含 synthetic adopt、fork/branch rebind、title merge 和 busy→idle 对比，输出 `ReplaceAll`；这与当前 CREATE/DELETE/overflow 路径一致，不把已知 MODIFY 降级为全量扫描。
5. Shell 只根据 `became_idle` 和当前 `focused_session` 判断 unread，并用 `rebinds` 更新 focused/selected key；不得接触 `ProviderSession`。
6. `poll_provider_changes` 丢弃 provider/cwd 与当前选择不一致的迟到事件，防止 workspace 切换后旧 title 泄漏。

具体修改：


- `SessionSummary.id` 替换为 `SessionSummary.key: SessionKey`；需要显示裸 ID 时使用 `key.session_id`。
- 删除独立的 `SessionSummary.provider` 字段，Provider 身份只从 `SessionSummary.key.provider` 读取，避免两份状态不一致。
- `LiveEntry` 增加 `key: SessionKey`，锁文件使用 provider-qualified lock key。
- `list_for_workspace` 显式接收 `ProviderId`；本步骤所有调用传 `registry.default_id()`，所以 UI 结果不变。
- synthetic ID 仍为 `new-N`，但完整 key 是 `(omp, new-N)`。
- synthetic 与新 JSONL 的 mtime 匹配、排除 fork、500ms skew、oldest-first 配对规则保持不变。
- fork/branch rebind 只在 `provider.capabilities().live_rebind` 为 true 时调用 `provider.parent_refers_to()`；OMP 返回 true，仍使用现有 UUID/path 算法。false Provider 即使返回 `parent_ref` 也不得迁移 live、selection、unread 或 lock。
- `capabilities.live_rebind == false` 时不调用 `parent_refers_to`，但正常的 synthetic `new-N` → 持久 ID adopt 仍执行；两类重绑定必须保持独立。
- `attach_new` 保持原顺序：`available()` → 分配 synthetic ID → amux flock → 构造 `SpawnSpec` → PTY spawn。
- `attach_resume` 保持原顺序：`provider.check_external_occupant()` → amux flock → `available()` → 构造 `SpawnSpec` → PTY spawn；不得交换错误优先级。
- 从 Provider 取得 `SpawnSpec` 后调用原 `PtySession::spawn`；program/args/env 已与现有签名同为 `String`，PTY 参数、rows/cols、kitty、host surface 不变。
- busy/unread 更新调用 `provider.session_busy()`；close、delete 和 rebind 调用 `provider.forget_session()`，Supervisor 不 import `SessionActivityTracker`。
- rename 的唯一入口先调用 `provider.normalize_title()`，再按现有顺序判断 live/exited/readiness 并执行 live/stored rename；OMP 复用现有首行、控制字符剔除、trim、空值拒绝算法。
- rename/delete/transcript/modified-files 操作由 Supervisor 转发给对应 Provider，Shell 不取得 `&mut dyn AgentProvider`。
- `drain_rebinds()` 返回 `(SessionKey, SessionKey)`。
- `set_host_surface`、`poll_exits`、kill ladder、join、shutdown 顺序不变。

`SessionLock::try_acquire` 接收 `&SessionKey`，但 OMP 的落盘名继续保持：

```text
<sanitized-session-id>.lock
```

未来非 OMP Provider 使用 `<provider>--<sanitized-session-id>.lock`。这样同裸 ID 不碰撞，同时新版 OMP 与旧版 amux 仍竞争同一个 flock，不改变现有占用语义。

### 1.9 修改 `src/lock.rs`

- `check_occupiable` 中的 OMP pgrep 逻辑移入 `OmpProvider::check_external_occupant`。
- `SessionLock` 只负责 amux 自身 flock，不再知道 `omp` 命令。
- lock path 生成改为接收 `SessionKey`：OMP 保持 legacy 裸 ID 文件名，非 OMP Provider 使用 provider-qualified 文件名。
- instance lock 行为、SIGTERM → SIGKILL 替换流程不变。

### 1.10 修改 `src/shell/mod.rs`

Shell 只消费 `SessionSummary` 和 Supervisor 高层操作：

- `focused_session_id: Option<String>` 改为 `focused_session: Option<SessionKey>`。
- 选择恢复、unread、rebind、cache key 全部改用 `SessionKey`。
- 移除 Shell 中的 `SessionDirWatcher`、`dirty_session_paths`、`title_debounce_until`、`title_need_rescan`、`last_title_poll`；workspace 变化时调用 `sessions.select_provider_workspace(...)`，event loop 调用 `sessions.poll_provider_changes(now)`，Shell 只消费 `SessionSummaryChange`。
- `next_timeout()` 合并 `sessions.next_provider_deadline()`。
- live/stored rename：Shell 只提交 draft 并展示 Supervisor 返回结果；title 规范化、readiness、Provider action 和错误时机由 Supervisor 按原顺序处理。OMP 最终写入 PTY 的字节必须仍为 `Ctrl-U + /rename + title + CR`。
- stored delete：只调用 Supervisor，不直接接触 JSONL/title slot/artifacts。
- transcript cache 从 `(path, mtime, size)` 改为 `(SessionKey, modified_at, size)`，缓存失效条件不变。
- modified-files cache 使用 `Box<dyn ModifiedFilesScanner>`，版本和 file-index 缓存规则不变。
- 所有现有 OMP 文案和快捷键在本步骤保持不变，包括 `new omp session`、退出确认、startup hint 和帮助内容；中立化文案留到真正提供 Provider 选择 UI 时处理，避免本次重构产生可见变化。

### 1.11 修改 `src/config/mod.rs`

仅允许内部构造调整：

- `AmuxConfig` 字段、serde 名称、默认值完全不变。
- `omp_command()` 和 `effective_pi_pins()` 可以移到 `OmpProvider` 私有构造逻辑，也可以保留为过渡期内部方法；本步骤结束时不得新增用户可见配置。
- 不新增 `provider`、`providers` 或 `default_provider` 字段。

### 1.12 修改 `src/lib.rs`

继续导出 `provider` 模块；不从 crate root 暴露 SPI 细节。amux 当前是单 binary 主导项目，SPI 是 crate 内部扩展点，不承诺第三方 ABI 稳定性。

### 1.13 不修改的文件

- `src/pty/mod.rs`：Provider 不得改变 PTY 生命周期和 VT 行为。
- `src/raw_input.rs`、`src/escape.rs`、`src/mouse.rs`：输入与鼠标不属于 Provider。
- `src/appearance.rs`、`src/theme.rs`：主题行为不变。
- `src/workspace/mod.rs`：workspace 持久化格式不变。
- `README.md`：当前已经描述 Provider registry 和 OMP-first；本步骤没有用户可见能力变化，不追加尚不可用的 Provider 文档。
- `Cargo.toml`：SPI 化不需要新依赖。

---

## 2. 行为变化

### 2.1 用户可见行为

**预期为零。**

| 场景 | 重构前 | 重构后 |
|---|---|---|
| 启动命令 | `omp --cwd <workspace>` | 完全相同 |
| 恢复命令 | `omp --cwd <workspace> --resume <id>` | 完全相同，参数顺序不变 |
| profile/session-dir | 来自现有 config | 完全相同 |
| `PI_*` 环境变量 | 现有四个默认 pin 或用户覆盖 | 完全相同 |
| 新会话临时名称 | `New session` / `new-N` | 完全相同 |
| title 优先级 | official → provisional → fallback | 完全相同 |
| title 刷新 | notify + debounce + fallback poll | 完全相同时间边界 |
| live rename | 注入 `Ctrl-U` + `/rename` | 完全相同字节 |
| disk rename | 写 256-byte slot/legacy prepend | 完全相同 |
| delete | JSONL + sibling artifacts | 完全相同 |
| fork/branch | live PTY 重绑定到 child ID | 完全相同 |
| busy/unread | OMP JSONL tail + activity tracker | 完全相同 |
| transcript/files | OMP JSONL 解析与缓存 | 完全相同 |
| PTY/鼠标/主题 | 现有实现 | 不修改 |
| config/workspaces | 现有 JSON | 不迁移 |

### 2.2 内部结构变化

live map、focused/selected key、cache 和 lock API 改为 `SessionKey`，但 OMP 的 lock 文件路径继续保持：

```text
~/.amux/locks/<session-id>.lock
```

因此没有 OMP 磁盘路径或占用语义变化。未来新增的非 OMP Provider 才使用 `<provider>--<session-id>.lock`。

### 2.3 明确禁止的行为变化

- 不把 OMP 文案提前改成通用 Agent 文案。
- 不改变 session 排序、selection restore 或 unread 清除时机。
- 不把增量 title refresh 简化成每次全量扫描。
- 不改变 `Ctrl+N`、`Ctrl+\` 或任何 AgentMode 截获键。
- 不改变 PTY readiness、startup drop、20s hint、kill ladder。
- 不改变 OMP session JSONL 的任何字节，rename/delete 用户操作除外。
- 不保留旧 API alias 或双路径 Provider 调用；迁移完成后删除 Shell/Supervisor 的 OMP 直连路径。

---

## 3. 为什么这样设计

### 3.1 Provider 描述“差异”，Supervisor 管理“生命周期”

PTY 是 amux 已验证的公共能力。把 PTY 下放到 Provider 会导致 resize、主题、输入、退出清理和错误恢复逐 Provider 分叉。`SpawnSpec` 是最窄边界：Provider 决定程序、参数、环境和 cwd，Supervisor 统一执行。

### 3.2 编译期 SPI，不做动态插件 ABI

Rust 没有稳定 ABI。当前需求是让仓库内后续增加 Codex、Claude Code 等实现更容易，不是让第三方安装二进制插件。`Box<dyn AgentProvider>` 提供运行时注册和隔离，但所有实现与 amux 一起编译、测试和发布，避免动态库版本、安全和跨平台问题。

### 3.3 SPI 同时受 OMP 与已知 Codex 差异约束

只从 OMP 代码机械抽接口会固化 `--cwd`、`--resume`、按 cwd 分桶 JSONL、title slot 等假设。因此 SPI 使用 `SpawnSpec`、`ProviderSession`、capabilities 和 Provider-owned change source；OMP 文件细节留在 OMP 实现内。这样 Codex 后续可用 `codex -C`、`codex resume` 和 App Server，而不用修改 Supervisor。

### 3.4 强类型 `SessionKey`

裸 session ID 已经贯穿 live map、focused session、selection、unread、cache 和 lock。新增 Provider 后继续使用裸 ID 会产生碰撞，并诱导后续到处拼字符串。一次性迁移为 `(ProviderId, session_id)` 能让编译器暴露遗漏调用点。

### 3.5 capability 只控制 UI，不提供假实现

Provider 不支持 rename/transcript 时，UI 应隐藏操作。SPI 方法不提供默认成功或空数据，避免新 Provider 看似工作、实际丢失用户操作。误调用必须返回明确错误。

### 3.6 Provider 自己拥有 change source

OMP 使用目录 watcher，Codex 可能使用 App Server 通知或轮询。Shell 如果继续持有 `SessionDirWatcher`，新增 Provider 仍要修改 Shell。Provider 输出中立的 `ProviderChange`，Shell/Supervisor 只处理 upsert/remove/rescan。

### 3.7 保留 OMP 算法，不在抽象迁移时重写

本步骤的风险来自调用路径变化，不应叠加算法变化。cwd hash、title slot、fork 匹配、busy 状态、transcript 和 diff 均保留现有实现；SPI 只做输入输出转换。这样现有 fixture 和测试仍然是有效的回归保护。

### 3.8 不修改持久配置

当前只有 OMP，实现 Provider 选择配置没有用户价值，却会引入迁移和回滚成本。Registry 内部默认 OMP；等第二个 Provider 实现完成，再单独设计配置和 UI。

---

## 4. 需要补充的测试

### 4.1 保留并继续运行的现有 OMP 测试

以下现有测试不得删除、改弱断言或改成只验证新类型：

- cwd bucket：`encode_home_relative`、`encode_matches_omp_v17_known_hash`、`list_merges_legacy_session_dir`。
- title：`resolve_official_from_slot`、`resolve_provisional_from_first_user_message`、两个 `write_session_title_*`。
- fork：`parent_refers_to_uuid_and_path`、`synthetic_match_skips_fork_and_pairs_oldest`、`synthetic_match_allows_small_mtime_skew`。
- 删除：`delete_session_removes_jsonl_and_artifacts`、`list_skips_orphan_artifacts_dir_without_jsonl`。
- busy/status：pending、complete、async result、tool result、aborted/error、unknown、oversized tail、tracker truncation 全部现有测试。
- transcript/markdown/render、mouse、raw input、PTY、theme、workspace 和 lock 的全部现有测试。

### 4.2 `provider/api.rs` 新增测试

1. `session_key_distinguishes_same_id_across_providers`
   - 构造 `omp/x` 和测试 Provider `fake/x`。
   - 断言不相等且 HashMap 可同时保存两项。
2. `provider_id_display_is_stable`
   - 断言 `ProviderId::OMP.as_str() == "omp"`。
3. `spawn_spec_keeps_existing_utf8_contract`
   - 断言 program/args/env 使用与 `PtySession::spawn` 相同的 `String` 类型，OMP 的 cwd 参数仍由现有 `to_string_lossy` 产生。

### 4.3 `provider/registry.rs` 新增测试

1. `registry_defaults_to_omp`
   - 默认 ID 为 OMP，且能取得同一实例。
2. `registry_rejects_duplicate_provider_id`
   - 第二次注册相同 ID 返回错误，原实现未被覆盖。
3. `registry_reports_unknown_provider`
   - 错误包含请求的 Provider ID。

测试使用最小 `FakeProvider`，只返回固定数据；不启动 PTY，不访问用户 HOME。

### 4.4 `provider/omp.rs` 新增 SPI contract 测试

1. `omp_new_spawn_spec_matches_legacy_argv`
   - 精确断言 program、`--cwd`、profile、`--session-dir` 和顺序。
2. `omp_resume_spawn_spec_matches_legacy_argv`
   - 精确断言 `--cwd <cwd> --resume <id>`，common args 仍在尾部。
3. `omp_spawn_spec_preserves_default_pi_pins`
   - 精确断言四个默认 `PI_*` 名称和值。
4. `omp_spawn_spec_preserves_configured_pi_pins`
   - 用户覆盖时不混入默认 pin。
5. `omp_maps_disk_session_without_losing_metadata`
   - `id/title/title source/parent/path/cwd/mtime/size` 与原 `OmpDiskSession` 及传入 `list_sessions(cwd)` 的 cwd 一致；resume 和 modified-files scanner 消费同一个映射结果。
6. `omp_live_rename_action_preserves_exact_bytes`
   - 精确断言 `b"\x15/rename new title\r"`。
7. `omp_stored_rename_preserves_title_slot_length`
   - 复用现有 fixture，rename 前后固定 slot 文件长度不变。
8. `omp_delete_preserves_artifact_cleanup`
   - SPI 入口执行后 JSONL 和 sibling 目录均不存在。
9. `omp_external_occupant_uses_resume_prefix_matching`
   - 把 process-list 解析拆成纯函数，用 fixture 覆盖 `--resume`、`-r`、非匹配和 amux 自身行；测试不调用真实 `pgrep`。
10. `omp_normalize_title_preserves_existing_rules`
   - 覆盖只取首行、剔除控制字符、trim、全空白拒绝；live/stored 入口必须得到相同结果。

### 4.5 `provider/watch.rs` 新增测试

1. `omp_watch_modify_emits_upsert_after_debounce`。
2. `omp_watch_create_delete_or_overflow_emits_rescan`。
3. `omp_watch_switch_workspace_drops_old_workspace_events`。

增加跨层测试 `omp_watch_change_flows_through_supervisor_summary`：从 tempfile JSONL MODIFY 触发 OMP Upsert，经 Supervisor 完成 title/mtime/size/busy 合并，并验证 busy→idle 只输出一次 `became_idle`。
4. `omp_watch_fallback_poll_detects_changed_fingerprint`。
5. `omp_watch_next_deadline_matches_existing_debounce`。

测试使用 `tempfile`，等待以条件/通道事件为准，不用固定长 sleep；保留当前约 80ms debounce 和 3s fallback 常量的行为断言。

### 4.6 `session/mod.rs` 新增测试

1. `supervisor_uses_provider_qualified_live_keys`
   - fake Provider 返回与 OMP 相同裸 ID，两个 key 不覆盖。
2. `supervisor_preserves_omp_synthetic_reconcile`
   - `omp/new-1` 只与非 fork、最早合格的 OMP disk session 配对。
3. `supervisor_rebind_preserves_provider_id`
   - fork rebind 只能在同 Provider 内发生。
4. `supervisor_builds_pty_from_spawn_spec`
   - 把构造参数转换提取为纯函数，断言 program/args/env/cwd/rows/cols/kitty/surface 与现有 `PtySession::spawn` 入参完全一致；不增加第二套 PTY 抽象。
5. `supervisor_preserves_resume_guard_order`
   - fake Provider 和 fake lock seam 记录完整顺序，断言 external occupant → flock → available → spawn spec → PTY spawn；双重占用时仍优先返回 external OMP 错误。
6. `supervisor_disables_parent_rebind_when_capability_is_false`
   - fake Provider 返回 `live_rebind=false` 且 session 带 `parent_ref`，断言不调用 `parent_refers_to`、不迁移 live/selection/unread/lock；synthetic adopt 仍可独立执行。
7. `supervisor_normalizes_title_before_live_state_checks`
   - 覆盖多行、控制字符、全空白、live/stored 相同规范化结果；规范化错误发生在 readiness 判断前，并保持 modal 可重试。
8. `supervisor_workspace_switch_discards_old_provider_events`
   - A workspace 排队 change 后切到 B，断言先切换 change source、丢弃 A 队列、再 list B，Shell 只收到 B 的 summary。
9. `supervisor_provider_upsert_preserves_title_busy_unread_pipeline`
   - 已知 MODIFY 走 Upsert 而非 full rescan；验证 live title merge、mtime/size、busy→idle 和 `became_idle`。
10. `supervisor_live_rename_writes_provider_action_bytes`
   - 使用可观察 writer seam，断言只写一次且字节不变。
11. `supervisor_delete_closes_live_before_provider_delete`
   - fake Provider 记录顺序，断言 kill/join 完成后才调用删除。
12. `supervisor_rebind_preserves_unread_and_selection_keys`
   - old key 到 new key 的状态迁移不丢失。

只为需要观测的边界提取小型纯函数或测试 seam；不得引入通用 DI framework、mock crate 或第二套 SessionSupervisor。

### 4.7 `lock.rs` 新增测试

1. `omp_lock_path_preserves_legacy_filename`
   - 精确断言 OMP key 仍生成 `<sanitized-id>.lock`。
2. `same_session_id_uses_distinct_provider_lock_paths`
   - `omp/x` 使用 `x.lock`，`fake/x` 使用 `fake--x.lock`，两者不相等。
3. `session_lock_still_rejects_second_holder`
   - 同一 `SessionKey` 的第二次非阻塞 flock 仍返回 occupied，释放首个 lock 后可重新获取。

### 4.8 `shell/mod.rs` 缓存和选择测试

把以下无终端依赖逻辑提取成纯函数并测试：

1. `restore_selection_uses_full_session_key`。
2. `apply_rebind_updates_focused_and_selected_keys`。
3. `transcript_cache_invalidates_on_key_or_revision_change`。
4. `modified_files_cache_invalidates_on_session_key_change`。
5. `busy_to_idle_marks_unread_only_when_not_watching_same_key`。

不对 ratatui 绘制像素写脆弱快照；真实 TUI 使用第 6 节的交互回归矩阵验证。

### 4.9 测试基线注意事项

当前工作站设置了 `COLORFGBG=0;15`。现有三个 appearance 测试的语义是“没有 COLORFGBG 时 fallback dark”，但测试本身未隔离继承环境：直接运行 `cargo test --all-targets` 会得到 144 passed、3 failed、2 ignored；执行：

```bash
env -u COLORFGBG cargo test --all-targets
```

当前基线为 147 passed、2 ignored。该问题是重构前已存在的测试环境敏感性，不在 SPI 重构中顺手修复；验证命令必须显式清除该变量，避免把既有环境问题误判为 SPI 回归。

---

## 5. 风险及回滚方式

### 5.1 风险：对象安全或借用关系迫使 Shell 绕过 SPI

表现：为了同时访问 Provider、live PTY 和 session list，在 Shell 中重新出现 `if provider == "omp"` 或直接调用 OMP 函数。

控制：Registry 只由 SessionSupervisor 持有；rename/delete/transcript/watch 都暴露为 Supervisor 高层方法。Code review 搜索 `OmpProvider` 和 OMP helper 的调用范围，Shell/Supervisor 之外不得新增直连。

回滚：回退 Registry/Supervisor/Shell 三个迁移提交；新 `api.rs` 不被使用时一并删除。没有数据迁移。

### 5.2 风险：live map 改用 `SessionKey` 时漏掉裸 ID 比较

影响：选中项丢失、错误 session 被标 unread、fork 后 pane 指向父 session、cache 串 session。

控制：一次性迁移 `focused_session_id`、selection、rebind、unread、cache 和 lock；禁止保留 `String` alias。依赖编译错误暴露调用点，并补第 4.6/4.7 节测试。

回滚：代码回退即可；SessionKey 不持久化。

### 5.3 风险：watcher 所有权移动导致标题刷新变慢或漏事件

影响：official/provisional title、busy/unread、fork discovery 延迟。

控制：保留 80ms debounce、3s fallback、CREATE/DELETE/overflow full rescan 和 workspace switch 清空规则；新增 watcher contract 测试；真实 OMP 验证 title 与 fork。

回滚：若 watcher 测试或真实矩阵失败，停止合并并回退 watcher + Shell event plumbing，不采用“全部改成定时全量扫描”的降级实现。

### 5.4 风险：`SpawnSpec` 改变命令参数或环境

影响：profile/session-dir 不生效、OMP TUI 协议回退、主题/图片渲染异常。

控制：对 new/resume argv 和所有 `PI_*` 写精确 golden 断言；`PtySession::spawn` 签名和调用顺序不变。

回滚：回退 SpawnSpec 接线，恢复 `OmpProvider::spawn_*_args` 直传；不修改用户配置。

### 5.5 风险：live rename/delete 的操作顺序改变

影响：rename 覆盖编辑器输入；删除后 dying OMP 重建 JSONL；lock 提前释放导致并发恢复。

控制：live rename 字节精确测试；delete 顺序必须保持 `leave Agent if focused → close_session_blocking → join kills → provider delete → refresh`。

回滚：回退 rename/delete routing；JSONL 格式没有迁移。若删除真实验证失败，使用备份的 disposable fixture 重跑，不对用户历史 session 验证删除。

### 5.6 风险：SessionKey 迁移错误改变 OMP flock

影响：若实现者无条件给 OMP lock 加 provider 前缀，旧版与新版可能同时认为同一 OMP session 可占用。

控制：OMP 明确保留 legacy `<id>.lock`；只有非 OMP Provider 加前缀。精确 lock path 测试和双 holder flock 测试必须通过。

回滚：退出新版并回退二进制即可；本方案不迁移或重命名任何现有 lock 文件。

### 5.7 风险：SPI 变成过大的万能接口

影响：新增 Provider 被迫实现 OMP 文件语义，或出现大量无意义 no-op。

控制：本步骤只纳入当前 OMP 已使用且 Codex 已确认存在差异的能力；不加入模型、权限、MCP、远程控制等未被 amux 使用的接口；capability 负责 UI 可用性。

回滚：在第二个 Provider 实现前可无数据成本调整 SPI；不承诺外部 crate ABI。

### 5.8 风险：借重构顺手清理既有 warning 或 appearance 测试

影响：扩大 diff，难以确认 OMP 回归来源。

控制：当前 `cargo run -- --smoke` 有 18 个既有 warning，测试还受 `COLORFGBG` 环境影响；两者记录为基线，不在本步骤修复。新增代码不得增加 warning。

### 5.9 Git 回滚策略

实施放在独立 feature branch，按以下可独立验证的提交切分：

1. `refactor: add provider SPI types and registry`
2. `refactor: implement provider SPI for omp`
3. `refactor: route session supervisor through provider SPI`
4. `refactor: route shell operations through session supervisor`
5. `test: cover omp provider behavior parity`

每个提交都必须通过对应单元测试；第 5 个提交和真实 OMP 矩阵通过后才合并。出现 Critical/Important 回归时，不追加 compatibility shim，直接：

```bash
git revert <first-spi-commit>^..<last-spi-commit>
```

若已 squash merge，则：

```bash
git revert <squash-merge-commit>
```

本步骤无 config/session 数据迁移，因此回滚只涉及代码和短生命周期 lock 文件。

---

## 6. 完成该步骤的验证命令

### 6.1 重构前记录基线

```bash
cd /path/to/amux
codex --version  # 仅记录环境，不属于 OMP 验证
omp --version
env -u COLORFGBG cargo test --all-targets
cargo run -- --smoke
```

当前已记录基线：

```text
codex-cli 0.146.0
omp 17.3.3
cargo test: 147 passed, 2 ignored（清除 COLORFGBG 后）
cargo run -- --smoke: amux smoke ok
```

README 要求 OMP 17.2.5+；本次真实回归使用 OMP 17.3.3。

### 6.2 静态和单元验证

```bash
cargo fmt --check
env -u COLORFGBG cargo test --all-targets
cargo test provider::api::tests --lib
cargo test provider::registry::tests --lib
cargo test provider::omp::tests --lib
cargo test provider::turn_status::tests --lib
cargo test provider::transcript:: --lib
cargo test session::tests --lib
cargo build --release
cargo run -- --smoke
```

通过标准：

- 所有非 ignored 测试通过。
- 新增 SPI 测试全部执行，不允许通过 filter 误得到 0 tests。
- 不增加 compiler warning；当前 18 个既有 smoke warning另行治理。
- release build 成功。
- smoke 输出 `amux smoke ok`。

### 6.3 架构边界检查

```bash
# 使用仓库代码搜索工具确认；以下是需要满足的查询语义：
# 1. OmpProvider 只允许出现在 provider/omp.rs、provider/registry.rs 和对应测试。
# 2. write_session_title/delete_session_with_artifacts/refresh_disk_session
#    不允许被 shell/mod.rs 或 session/mod.rs 直接调用。
# 3. shell/mod.rs 不再持有 SessionDirWatcher。
# 4. SessionSupervisor 的 live map key 必须是 SessionKey。
```

该项在实施时使用代码搜索工具执行，不使用 shell `grep`。任一越界调用都视为 SPI 未完成。

### 6.4 真实 OMP TUI 回归命令

使用 disposable workspace 和 disposable OMP sessions，避免删除用户历史数据：

```bash
cd /path/to/amux
AMUX_LOG=1 cargo run --release
```

在实际 TUI 中依次验证：

1. 添加 disposable workspace，退出重开后仍存在。
2. `n` 新建 OMP session；确认真实 OMP TUI、主题和输入正常。
3. 提交一条会产生工具调用的消息；确认 busy spinner、完成后 unread 规则和 transcript preview。
4. 再建一个 session，在两者间切换；后台 session 不退出。
5. `Ctrl+\` 往返 Nav/Agent；双击 attach、键盘 attach、resize、鼠标滚动和 OSC 52 剪贴板不退化。
6. live rename；确认写入 OMP、sidebar official title 更新且重启后仍保留。
7. disk session rename；确认固定 title slot 未破坏 JSONL。
8. 执行 OMP `/fork`；确认同一 PTY 重绑定 child，selection/header/live dot 跟随 child。
9. 执行 OMP `/branch` 并切换到历史节点；确认当前 OMP 的同文件树分支继续写入同一 JSONL，且 amux 不误判为跨文件 rebind。
10. 打开 transcript 和 `Ctrl+N` modified-files panel；确认 tool、markdown、diff 和 cache 更新正常。
11. 关闭一个 live session；确认另一个仍运行。
12. 删除 disposable disk session；确认 JSONL 和 sibling artifacts 都删除，且 dying OMP 不会重建文件。
13. `q`、`y` 退出；确认全部 child process 终止、终端 raw/alternate-screen/mouse 模式恢复。
14. 重启 amux，恢复保留的 disposable session；确认 `omp --resume` 成功。

同时观察：

```bash
cat ~/.amux/amux.log
```

通过标准：无 panic、无 spawn/resume 参数错误、无重复 session、无错误 rebind、无卡死 child、无终端模式残留。

### 6.5 合并前最终命令

```bash
cargo fmt --check \
  && env -u COLORFGBG cargo test --all-targets \
  && cargo build --release \
  && cargo run -- --smoke
```

只有自动验证全部通过且第 6.4 节真实 OMP 矩阵逐项确认后，才能声明 SPI 化完成和 OMP 行为等价。

### 6.6 2026-08-14 实施结果

- 变更模块的非递归 `rustfmt --check` 通过。仓库级 `cargo fmt --check` 仍会报告未触及文件的既有格式差异，因此未用全仓格式化覆盖这些文件。
- `env -u COLORFGBG cargo test --all-targets`：220 passed，2 ignored。
- `cargo build --release`：成功；仍为 18 个既有 warning，本次未新增 warning。
- `cargo run -- --smoke`：输出 `amux smoke ok`。
- 架构边界搜索通过：Shell、Session、Lock 生产路径均不再直连 OMP 实现；唯一生产调用链为 `Shell → SessionSupervisor → ProviderRegistry → AgentProvider`。
- 使用 release binary + OMP 17.3.3 完成 disposable TUI 回归：新建/恢复、双 live session 后台保活与切换、tool call、transcript/modified-files、live/stored rename、close/delete、`/fork` child 重绑定、`/branch` 同文件树分支、重启恢复和正常退出均通过；测试 JSONL、workspace 与进程已清理。
- 实施后独立代码终审先发现 provider 锁键碰撞、modified-files 路径缓存失效和 stale busy 状态 3 项问题；均补 RED/GREEN 回归并修复。定向复核最终结论：**APPROVED，无 Critical/Important/Minor finding**。

---
## Review 结论与二次确认

评审方式：独立 `reviewer` 子代理对照本规格与 `provider/session/shell/lock/config/pty` 现有实现做只读评审；未修改文件，未运行 formatter、lint、build 或测试。

首次结论为“修订后可执行”，发现 2 个 Critical 和 4 个 Important：

1. `SpawnSpec<OsString>` 与现有 `PtySession::spawn<String>` 矛盾。
2. watcher change 缺少 Supervisor 消费协议和 workspace 切换顺序。
3. resume external occupant 与 flock 顺序写反。
4. title 规范化缺少 SPI seam。
5. `live_rebind=false` 没有消费规则。
6. cwd 映射和 provider-aware lock 缺少精确测试。

本规格已逐项修订：

- `SpawnSpec` 改回现有 `String` 契约，明确保持 OMP `to_string_lossy` 行为。
- 增加 `SessionSummaryChange`、`select_provider_workspace`、`poll_provider_changes`、迟到事件丢弃和 Upsert/ReplaceAll 规则。
- 恢复 `external occupant → flock → available → spawn spec → PTY` 顺序。
- 增加 `normalize_title`，复用现有 OMP 规范化和错误时机。
- 明确 `live_rebind` capability gating，且不影响 synthetic adopt。
- 补 cwd、watcher→Supervisor、legacy OMP lock path、跨 Provider 同 ID 和双 holder 测试。
- OMP lock path继续使用 legacy `<id>.lock`，消除原方案的唯一磁盘路径变化。

同一 reviewer 对修订稿进行了定向二次复核，确认原 2 个 Critical 与 4 个 Important 均关闭，无遗留 finding。最终 verdict：**可执行，但只有第 6 节自动验证和真实 OMP 矩阵全部通过后，才能认定行为等价。**

---


## 实施顺序

1. 先增加中立类型、SPI、Registry 和纯单元测试，不改变现有调用路径。
2. 让 `OmpProvider` 实现 SPI，精确对比 argv/env/session 映射。
3. 把 `SessionSupervisor` 迁到 Registry 和 `SessionKey`，保持 Shell 暂时可编译。
4. 把 watcher、rename、delete、transcript、modified-files 调用从 Shell 收口到 Supervisor/Provider。
5. 删除旧 re-export 和 OMP 直连路径，禁止双路径兼容层。
6. 执行全部自动验证、架构边界检查和真实 OMP 矩阵。
7. 完成后再单独设计 CodexProvider；不得在本次重构中混入 Codex 行为。

