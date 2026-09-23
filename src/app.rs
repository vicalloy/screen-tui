//! 应用状态机与主事件循环（tech-design §2.1 / §2.2）。
//!
//! `Mode` 之间只通过显式事件转换；`Esc` 统一回退上一层，`q` 在 `List` 才退出。
//! 渲染层只读 `App`；`App` 的按键处理是纯状态变更（除显式标注的动作外不碰进程环境），
//! 因此可脱离终端做单测。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};

use crate::config::Config;
use crate::screen::caps::Caps;
use crate::screen::cmd::{self, AttachKind};
use crate::screen::parse::{self, Enumeration, SessionRecord, Status};
use crate::screen::probe;
use crate::ui;
use crate::util::time::local_label;

/// 刷新间隔（FR-19：默认 3 秒，介于 spv 的 1s 与 screen-manager 的 5s 之间）。
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(3);

/// 事件轮询的最大等待。同时封顶「响应外部信号」的延迟。
const POLL_CAP: Duration = Duration::from_millis(200);

/// 会话名上限。screen 的 socket 文件名是 `<pid>.<name>`，80 字符留足余量（FR-02 验收 2）。
pub const NAME_MAX: usize = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 主列表（默认轮询）。
    List,
    /// `?` 帮助弹层。
    Help,
    /// `i` 详情弹层（窄屏的主要信息入口，FR-05）。
    Detail,
    /// `n` 新建会话三步向导（FR-02）。
    NewSession,
    /// attached 会话的冲突选择框（共享 / 接管 / 取消，FR-03）。
    AttachChoice,
    /// 危险操作二次确认（K / D / W，FR-13/FR-12/FR-20）。
    Confirm,
    /// `r` 重命名输入（FR-14）。
    Rename,
    /// `/` 过滤输入（FR-16）。查询词存在 `App::filter`，离开输入态后仍生效。
    Filter,
    /// `p` 预览快照（FR-15）。
    Preview,
    /// 别名/描述单行编辑（FR-24）。
    MetaEdit,
}

/// 向导步骤：名 → 目录 → 命令，每步回车即接受默认值（FR-02 验收 1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewStep {
    Name,
    Dir,
    Command,
}

impl NewStep {
    pub fn index(self) -> usize {
        match self {
            NewStep::Name => 0,
            NewStep::Dir => 1,
            NewStep::Command => 2,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            NewStep::Name => "Name",
            NewStep::Dir => "Directory",
            NewStep::Command => "Command",
        }
    }
}

/// 新建向导的草稿状态。`error` 是阻断性错误（停在当前步），`note` 是非阻断提示
/// （如重名提示 —— FR-02 验收 2 允许重名创建，但提示寻址方式）。
///
/// 不派生 `Default`：`NewStep` 没有合理初值，向导一律经 `open_new_session()` 显式构造。
#[derive(Debug, Clone)]
pub struct NewDraft {
    pub step: NewStep,
    pub name: String,
    pub dir: String,
    pub command: String,
    pub error: Option<String>,
    pub note: Option<String>,
}

/// 会话名校验（纯函数）：空名 / 前导 `-`（会被 screen 当选项）/ 空白与控制字符 /
/// 超长即时报错；重名不在此拦 —— 见 `NewDraft::note`。
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name must not be empty".into());
    }
    if name.starts_with('-') {
        return Err("name must not start with '-' (screen would read it as an option)".into());
    }
    if let Some(bad) = name.chars().find(|c| c.is_control() || c.is_whitespace()) {
        return Err(format!(
            "name must not contain whitespace or control characters (found {bad:?})"
        ));
    }
    if name.chars().count() > NAME_MAX {
        return Err(format!("name is longer than {NAME_MAX} characters"));
    }
    Ok(())
}

/// 默认会话名：`<当前目录名>-<MMDD-HHMM>`（FR-02 验收 1）。
///
/// 目录名先过一遍与 `validate_name` 同口径的清洗（空白 → `-`），保证默认值必过校验。
pub fn default_session_name(dir: &std::path::Path, now: std::time::SystemTime) -> String {
    let base = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "session".into());
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .collect();
    let label = local_label(now);
    if label.is_empty() {
        cleaned
    } else {
        format!("{cleaned}-{label}")
    }
}

/// 默认命令：`$SHELL`（非空时），否则 `/bin/sh`。
pub fn default_command() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/bin/sh".into())
}

/// 展开 `~` 前缀（仅 `~` 与 `~/` 两种形态；其余 `~user` 不支持，原样保留交给报错）。
pub fn expand_tilde(path: &str) -> String {
    if path == "~" {
        return std::env::var("HOME").unwrap_or_else(|_| path.into());
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var("HOME").ok().filter(|h| !h.is_empty())
    {
        return format!("{home}/{rest}");
    }
    path.to_string()
}

/// attached 冲突选择框的挂起状态（1.5b）。
#[derive(Debug, Clone)]
pub struct AttachChoice {
    pub name: String,
    /// Multi 会话的尺寸风险提示（FR-03）。
    pub note: Option<String>,
}

// ------------------------------------------------------------- 会话动作（T2.4）

/// 动作种类（对应 `cmd::SessionAction`；App 层单独建模以携带展示信息）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    /// 远程断开（FR-12）。
    Detach,
    /// 终止会话（FR-13）。
    Kill,
    /// 清理 dead 会话（FR-20）。
    Wipe,
    /// 清理已消失会话的本工具元数据（T2.6，不碰 screen 资源）。
    Cleanup,
}

impl ActionKind {
    pub fn label(self) -> &'static str {
        match self {
            ActionKind::Detach => "detach",
            ActionKind::Kill => "kill",
            ActionKind::Wipe => "wipe",
            ActionKind::Cleanup => "cleanup",
        }
    }

    /// 确认框的动词描述（危险操作要把后果说清楚）。
    pub fn consequence(self) -> &'static str {
        match self {
            ActionKind::Detach => "detach the attached client (it keeps running)",
            ActionKind::Kill => "TERMINATE the session and all its windows",
            ActionKind::Wipe => "remove all dead session sockets",
            ActionKind::Cleanup => "forget metadata of sessions that no longer exist",
        }
    }

    fn to_cmd(self) -> Option<cmd::SessionAction> {
        match self {
            ActionKind::Detach => Some(cmd::SessionAction::Detach),
            ActionKind::Kill => Some(cmd::SessionAction::Kill),
            ActionKind::Wipe => Some(cmd::SessionAction::Wipe),
            ActionKind::Cleanup => None, // 纯配置操作，不经 screen。
        }
    }
}

/// 二次确认框的状态（FR-13：默认焦点在**取消**，防小屏误触）。
#[derive(Debug, Clone)]
pub struct ConfirmAction {
    pub kind: ActionKind,
    /// `<pid>.<name>` 全名（Wipe 为空串）。
    pub target: String,
    /// 确认框里显示的会话名。
    pub display: String,
    /// 探测到的运行命令（FR-13 验收 2：让用户确认杀对了对象）。
    pub command: Option<String>,
    /// 当前焦点：`true` = 确认键。**初始恒为 false（取消）**。
    pub focus_yes: bool,
}

/// 重命名输入的状态（FR-14）。
#[derive(Debug, Clone)]
pub struct RenameDraft {
    /// `<pid>.<name>` 全名。
    pub target: String,
    /// 可编辑的新名字（预填当前名）。
    pub name: String,
    pub error: Option<String>,
}

/// 已确认的重命名请求：由 `App` 产出、事件循环消费。
#[derive(Debug, Clone)]
pub struct RenameRequest {
    pub target: String,
    pub new_name: String,
}

// ------------------------------------------------------------- 预览（T2.3）

/// 一次成功抓取的预览视图（FR-15）。只在成功时存在 —— 失败不 produce 视图，
/// 绝不用旧视图顶替（FR-15 验收 3）。
#[derive(Debug, Clone)]
pub struct PreviewView {
    pub full: String,
    pub name: String,
    /// 抓取时间标签（本地时间）。
    pub fetched: String,
    pub lines: Vec<String>,
}

/// 预览抓取请求：`App` 产出、事件循环执行（保持 App 可脱离终端单测）。
#[derive(Debug, Clone)]
pub struct PreviewRequest {
    pub full: String,
    pub name: String,
    /// `true` = 用户按了 `p`（失败要给可读提示）；`false` = 宽屏自动跟随（失败静默降级）。
    pub manual: bool,
}

/// managed 会话重启请求（FR-24）：用记录的 command+cwd 重建。
#[derive(Debug, Clone)]
pub struct RestartRequest {
    pub name: String,
    pub dir: PathBuf,
    pub command: String,
}

/// 元数据字段编辑（别名 / 描述）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaField {
    Alias,
    Note,
}

impl MetaField {
    pub fn key(self) -> &'static str {
        match self {
            MetaField::Alias => "alias",
            MetaField::Note => "note",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MetaEdit {
    /// 元数据键 = 会话名。
    pub session: String,
    pub field: MetaField,
    pub value: String,
    pub error: Option<String>,
}

/// `.screenrc` 的 `escape` 行解析（FR-18，纯函数）。
///
/// 认两种形态：`escape ^Aa`（单 token：控制字符 + 命令字符）与 `escape ^A a`
/// （两 token）。返回给用户看的前缀描述，如 `Ctrl-A`；字面前缀字符原样返回。
pub fn parse_screenrc_escape(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let mut tokens = line.split_whitespace();
        // 空 token（空行/纯注释行）只跳过本行，绝不能 `?` 提前退出整个函数。
        let Some(keyword) = tokens.next() else {
            continue;
        };
        if keyword != "escape" {
            continue;
        }
        let Some(first) = tokens.next() else {
            continue;
        };
        let first = first.trim_start_matches('"');
        let describe = |token: &str| -> Option<String> {
            let raw = token.strip_prefix('^').unwrap_or(token);
            let c = raw.chars().next()?;
            if token.starts_with('^') {
                Some(format!("Ctrl-{}", c.to_ascii_uppercase()))
            } else {
                Some(c.to_string())
            }
        };
        return describe(first);
    }
    None
}

/// 探测用户实际配置的 escape 前缀（FR-18）：`$SCREENRC` > `~/.screenrc`。
pub fn detect_escape_prefix() -> Option<String> {
    let path = match std::env::var_os("SCREENRC").filter(|v| !v.is_empty()) {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let home = std::env::var_os("HOME").filter(|v| !v.is_empty())?;
            std::path::PathBuf::from(home).join(".screenrc")
        }
    };
    let text = std::fs::read_to_string(path).ok()?;
    parse_screenrc_escape(&text)
}

/// detach 提示（FR-18 验收）：探测到实际前缀时按它提示，否则回退默认并注明。
pub fn detach_hint_text(prefix: Option<&str>) -> String {
    match prefix {
        Some(p) => format!("Tip: detach with {p} d"),
        None => {
            "Tip: detach with Ctrl-A D (default prefix - use your own prefix + d if you changed it)"
                .into()
        }
    }
}

/// 已确认的连接请求：由 `App` 产出，事件循环消费（保持 App 可脱离终端单测）。
#[derive(Debug, Clone)]
pub struct AttachRequest {
    pub kind: AttachKind,
    pub target: String,
}

/// 歧义名回退（1.5e / FR-03 验收 4）：同名会话多于一个时用 `<pid>.<name>` 全名寻址。
pub fn unambiguous_target(sessions: &[SessionRecord], name: &str) -> String {
    let matches: Vec<&SessionRecord> = sessions.iter().filter(|s| s.name == name).collect();
    match matches.len() {
        1 => name.to_string(),
        _ => matches
            .first()
            .map(|s| s.full.clone())
            .unwrap_or_else(|| name.to_string()),
    }
}

/// `-Q windows` 输出 → 窗口数（FR-17，纯函数）。
///
/// 输出每行一个窗口（`0$ bash` / `1$* zsh` 形态），数非空行即可；
/// 4.00.03 的 usage dump 每行也有内容，但那条路径根本到不了这里
/// —— 调用方只在 `caps.query == Yes` 时查询。
pub fn parse_window_count(text: &str) -> usize {
    text.lines().filter(|l| !l.trim().is_empty()).count()
}

pub struct App {
    pub caps: Caps,
    /// 用户配置（T2.1）：刷新间隔、布局阈值、escape 前缀覆盖等。
    pub config: Config,
    pub mode: Mode,
    /// 最近一次 `-ls` 枚举结果；首次刷新失败时为 `None`（正文给空态，不闪退）。
    pub enumeration: Option<Enumeration>,
    /// 选中行下标，永远 clamp 在 `0..=len-1`。
    pub selected: usize,
    /// 页脚瞬态消息（刷新失败、动作结果等），不弹窗打扰。
    pub status: Option<String>,
    /// 新建向导草稿；仅在 `Mode::NewSession` 期间非空。
    pub draft: Option<NewDraft>,
    /// 元数据探测缓存（T2.2）：详情/过滤按 pid 取，refresh 后选中项强制重探。
    pub meta_cache: probe::MetaCache,
    /// 选中会话的元数据（cwd / command，取不到为 `None` → UI 隐藏字段，C-5）。
    pub meta: Option<probe::Meta>,
    /// attached 冲突选择框状态；仅在 `Mode::AttachChoice` 期间非空。
    pub attach: Option<AttachChoice>,
    /// 危险操作确认框状态；仅在 `Mode::Confirm` 期间非空。
    pub confirm: Option<ConfirmAction>,
    /// 重命名输入状态；仅在 `Mode::Rename` 期间非空。
    pub rename: Option<RenameDraft>,
    /// 探测到的 escape 前缀（FR-18）；`None` = 未探测到，提示回退默认。
    pub escape_prefix: Option<String>,
    /// 过滤查询词（FR-16）。空串 = 不过滤；refresh 不重置它。
    pub filter: String,
    /// 选中会话的窗口数（`-Q windows`，FR-17）。能力不可用/未知时恒为 `None` → UI 隐藏。
    pub window_count: Option<usize>,
    /// 最近一次成功抓取的预览（T2.3）。仅宽屏右栏与 `p` 弹层消费。
    pub preview: Option<PreviewView>,
    /// 宽屏自动预览的「已抓取目标」—— 同一会话不重复抓，选中项变化才再抓。
    last_preview_target: Option<String>,
    /// 待事件循环执行的预览抓取请求。
    preview_request: Option<PreviewRequest>,
    /// 待事件循环执行的重启请求（T2.6：managed 会话重建）。
    restart_request: Option<RestartRequest>,
    /// 元数据字段编辑状态；仅在 `Mode::MetaEdit` 期间非空。
    pub meta_edit: Option<MetaEdit>,
    /// 配置落盘路径覆盖（测试注入）；`None` = 标准位置。
    pub config_path_override: Option<PathBuf>,
    /// 加载时被告知的只读状态（配置版本比本工具新）。
    pub config_read_only: bool,
    /// 会话枚举器（NFR-10 可测性）：生产用 [`parse::enumerate`]，
    /// 测试注入替身 —— 与 M1 的 `plan_connect` 注入风格一致，但覆盖所有调用点。
    enumerate: fn() -> crate::screen::Result<Enumeration>,
    /// 待事件循环消费的**动作**请求（已过确认框）。
    action_request: Option<ConfirmAction>,
    /// 待事件循环消费的重命名请求。
    rename_request: Option<RenameRequest>,
    /// 待事件循环消费的连接请求（`take_attach_request` 取走后执行前台连接）。
    attach_request: Option<AttachRequest>,
    pub should_quit: bool,
    pub refresh_interval: Duration,
    last_refresh: Option<Instant>,
}

impl App {
    pub fn new(caps: Caps) -> Self {
        Self::with_config(caps, Config::default())
    }

    /// 带配置构造（T2.1）：刷新间隔来自配置（FR-19 可配置项），下限 500ms 防忙轮询。
    pub fn with_config(caps: Caps, config: Config) -> Self {
        let refresh_interval = Duration::from_millis(config.ui.refresh_ms.max(500));
        Self {
            caps,
            config,
            mode: Mode::List,
            enumeration: None,
            selected: 0,
            status: None,
            draft: None,
            meta_cache: probe::MetaCache::default(),
            meta: None,
            attach: None,
            confirm: None,
            rename: None,
            escape_prefix: None,
            filter: String::new(),
            window_count: None,
            preview: None,
            last_preview_target: None,
            preview_request: None,
            restart_request: None,
            meta_edit: None,
            config_path_override: None,
            config_read_only: false,
            enumerate: parse::enumerate,
            action_request: None,
            rename_request: None,
            attach_request: None,
            should_quit: false,
            refresh_interval,
            last_refresh: None,
        }
    }

    /// 可见会话（渲染层与选中语义统一走这里；FR-16 过滤生效后的子集）。
    ///
    /// 过滤匹配（大小写不敏感的子串）：会话名 / PID 恒参与；
    /// 运行命令与工作目录在探测缓存里有就参与（进入过滤模式时一次性补齐缓存，
    /// 之后按缓存匹配 —— 不为过滤在每次 refresh 里对全部会话各起一个 lsof）。
    pub fn sessions(&self) -> Vec<SessionRecord> {
        self.all_sessions()
            .iter()
            .filter(|s| self.matches_filter(s))
            .cloned()
            .collect()
    }

    /// 全量会话（不过滤）。
    pub fn all_sessions(&self) -> &[SessionRecord] {
        self.enumeration
            .as_ref()
            .map(|e| e.list.sessions.as_slice())
            .unwrap_or(&[])
    }

    /// 过滤匹配（纯读：meta 只查缓存，不触发探测）。
    fn matches_filter(&self, session: &SessionRecord) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        let q = self.filter.to_lowercase();
        if session.name.to_lowercase().contains(&q)
            || session
                .pid
                .map(|p| p.to_string().contains(&q))
                .unwrap_or(false)
        {
            return true;
        }
        if let Some(pid) = session.pid
            && let Ok(pid) = u32::try_from(pid)
            && let Some(meta) = self.meta_cache.peek(pid)
            && (meta
                .command
                .as_deref()
                .map(|c| c.to_lowercase().contains(&q))
                .unwrap_or(false)
                || meta
                    .cwd
                    .as_deref()
                    .map(|c| c.to_lowercase().contains(&q))
                    .unwrap_or(false))
        {
            return true;
        }
        false
    }

    /// `/` 进入过滤：先把全部会话的元数据补进缓存（一次性，输入即筛的前提）。
    fn open_filter(&mut self) {
        let pids: Vec<u32> = self
            .all_sessions()
            .iter()
            .filter_map(|s| s.pid.and_then(|p| u32::try_from(p).ok()))
            .collect();
        for pid in pids {
            self.meta_cache.get(pid);
        }
        self.mode = Mode::Filter;
    }

    fn on_key_filter(&mut self, code: KeyCode) {
        match code {
            // Esc 清空并退出（FR-16 验收）；Enter 保留查询词回列表。
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::List;
            }
            KeyCode::Enter => self.mode = Mode::List,
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char(c) if !c.is_control() => self.filter.push(c),
            _ => {}
        }
        self.clamp_selection();
    }

    /// 存入一次枚举结果：不闪屏、不丢选中项、不 reset 其它状态（FR-19 验收 2）。
    pub fn apply_enumeration(&mut self, enumeration: Enumeration) {
        self.enumeration = Some(enumeration);
        self.clamp_selection();
    }

    /// 重新枚举（screen -ls，退出码只作快路径 —— FR-19 验收 1 的 M0 修订版）。
    ///
    /// 失败**保留旧列表**并在页脚给出原因：刷新失败不该把上一帧的真相擦掉。
    pub fn refresh(&mut self) {
        match (self.enumerate)() {
            Ok(enumeration) => {
                self.apply_enumeration(enumeration);
                self.status = None;
            }
            Err(err) => {
                self.status = Some(format!("refresh failed: {err}"));
            }
        }
        self.last_refresh = Some(Instant::now());
        self.refresh_meta();
    }

    /// 重新探测选中会话的元数据（T2.2）。探测失败只影响展示字段，不影响主流程。
    fn refresh_meta(&mut self) {
        self.meta = None;
        self.window_count = None;
        // 窗口数（FR-17）：仅当 `-Q` 实测可用（Support::Yes）才查询；
        // Unknown / No 一律隐藏 —— 显示「0」就是编造（C-5）。
        if self.caps.query.usable()
            && let Some(session) = self.sessions().get(self.selected)
        {
            let full = session.full.clone();
            if let Ok(run) = cmd::run(["-S", &full, "-Q", "windows"]) {
                self.window_count = Some(parse_window_count(&run.text()));
            }
        }
        if let Some(session) = self.sessions().get(self.selected)
            && let Some(pid) = session.pid
            && let Ok(pid) = u32::try_from(pid)
        {
            self.meta_cache.invalidate(pid);
            self.meta = self.meta_cache.get(pid).cloned();
        }
    }

    /// 显式共享连接（FR-10 / `x` 键）：重新校验后直接以 `-x` 进入，
    /// 不弹冲突选择框 —— 共享不踢人，任何可连接状态都安全。
    fn start_share(&mut self) {
        if self.sessions().is_empty() {
            return;
        }
        match (self.enumerate)() {
            Ok(fresh) => {
                let Some(name) = self.sessions().get(self.selected).map(|s| s.name.clone()) else {
                    return;
                };
                let status = fresh
                    .list
                    .sessions
                    .iter()
                    .find(|s| s.name == name)
                    .map(|s| s.status.clone());
                self.apply_enumeration(fresh);
                match status {
                    Some(Status::Dead | Status::Unreachable) => {
                        self.status = Some(format!("'{name}' is not connectable; cannot share"));
                    }
                    Some(Status::Unknown(raw)) => {
                        self.status = Some(format!(
                            "'{name}' reports unknown state '{raw}'; refusing to connect"
                        ));
                    }
                    Some(_) => self.request_attach(AttachKind::Share, name),
                    None => {
                        self.status = Some(format!("session '{name}' is gone; list refreshed"));
                    }
                }
            }
            Err(err) => {
                self.status = Some(format!("cannot verify sessions before connecting: {err}"));
            }
        }
    }

    fn clamp_selection(&mut self) {
        let count = self.sessions().len();
        if count == 0 {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(count - 1);
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.sessions().len();
        if count == 0 {
            return;
        }
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, count as isize - 1) as usize;
    }

    /// 距下次自动刷新的剩余时间；尚未刷新过时返回 0（下一轮立即刷新）。
    pub fn next_tick_in(&self) -> Duration {
        match self.last_refresh {
            None => Duration::ZERO,
            Some(at) => self.refresh_interval.saturating_sub(at.elapsed()),
        }
    }

    /// 是否到达自动刷新点。轮询间隔已到且未被手动刷新重置。
    pub fn tick_due(&self) -> bool {
        self.next_tick_in() == Duration::ZERO && self.last_refresh.is_some()
    }

    /// socket 目录（`-ls` 尾行提取；未枚举或解析不出时 `None`）。
    pub fn socket_dir(&self) -> Option<&str> {
        self.enumeration
            .as_ref()
            .and_then(|e| e.list.socket_dir.as_deref())
    }

    /// 按键分发。循环层已过滤非 Press 事件，这里再挡一次（双保险，tech-design §2.2 要点 1）。
    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        match self.mode {
            Mode::List => self.on_key_list(key.code),
            Mode::Help | Mode::Preview => self.on_key_overlay(key.code),
            Mode::Detail => self.on_key_detail(key.code),
            Mode::NewSession => self.on_key_new(key.code),
            Mode::AttachChoice => self.on_key_attach_choice(key.code),
            Mode::Confirm => self.on_key_confirm(key.code),
            Mode::Rename => self.on_key_rename(key.code),
            Mode::Filter => self.on_key_filter(key.code),
            Mode::MetaEdit => self.on_key_meta_edit(key.code),
        }
    }

    fn on_key_list(&mut self, code: KeyCode) {
        match code {
            // q 只在 List 退出（tech-design §2.1）。
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            // 手动刷新重置自动轮询计时（refresh() 内统一更新 last_refresh）。
            KeyCode::Char('R') => self.refresh(),
            // 过滤（FR-16）：输入即筛，Esc 清空。
            KeyCode::Char('/') => self.open_filter(),
            // 详情弹层：仅在有会话时可开（无会话保持列表空态）。
            KeyCode::Char('i') => {
                if !self.sessions().is_empty() {
                    self.mode = Mode::Detail;
                }
            }
            // 危险 / 低频操作（§6.4 设计规则：大写键留给危险或低频动作）。
            KeyCode::Char('D') => self.open_confirm(ActionKind::Detach),
            KeyCode::Char('K') => self.open_confirm(ActionKind::Kill),
            KeyCode::Char('W') => self.open_confirm(ActionKind::Wipe),
            KeyCode::Char('r') => self.open_rename(),
            // 显式共享连接（FR-10）：任何可连接会话都可以 `-x` 进入。
            KeyCode::Char('x') => self.start_share(),
            // 预览快照（FR-15）。
            KeyCode::Char('p') => self.open_preview(),
            // `s` 重启（T2.6）/ `X` 手动元数据清理（GC = 手动）在 List 层的入口。
            KeyCode::Char('s') => self.restart_selected(),
            KeyCode::Char('X') => self.open_cleanup(),
            KeyCode::Char('n') => self.open_new_session(),
            // 数字键 1–9 直连对应序号（§6.4 核心键）。
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                let index = (c as u8 - b'1') as usize;
                if index < self.sessions().len() {
                    self.selected = index;
                    self.start_connect();
                }
            }
            // 连接选中会话：连接前重校验（1.5a / NFR-08），不以列表旧状态为准。
            KeyCode::Enter => self.start_connect(),
            _ => {}
        }
    }

    // ------------------------------------------------------------- 连接闭环（T1.5）

    /// 连接入口：重新 `-ls` 拿新鲜状态再决策（FR-03 验收 3：列表不可信，必须重验）。
    fn start_connect(&mut self) {
        if self.sessions().is_empty() {
            return;
        }
        match (self.enumerate)() {
            Ok(fresh) => self.plan_connect(fresh),
            Err(err) => {
                self.status = Some(format!("cannot verify sessions before connecting: {err}"));
            }
        }
    }

    /// 连接决策（纯逻辑，吃注入的新鲜枚举结果）。
    pub fn plan_connect(&mut self, fresh: Enumeration) {
        let Some(name) = self.sessions().get(self.selected).map(|s| s.name.clone()) else {
            return;
        };
        // 先取走需要的信息再消费 fresh，避免借用冲突。
        let fresh_status = fresh
            .list
            .sessions
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.status.clone());

        match fresh_status {
            None => {
                // 会话已消失：明确提示 + 刷新，不卡死（FR-03 验收 3）。
                self.status = Some(format!("session '{name}' is gone; list refreshed"));
                self.apply_enumeration(fresh);
            }
            Some(status) => {
                self.apply_enumeration(fresh);
                match status {
                    Status::Detached => self.request_attach(AttachKind::Resume, name),
                    Status::Attached => {
                        self.attach = Some(AttachChoice { name, note: None });
                        self.mode = Mode::AttachChoice;
                    }
                    Status::Multi => {
                        self.attach = Some(AttachChoice {
                            name,
                            note: Some(
                                "multi-display session: terminals may resize each other".into(),
                            ),
                        });
                        self.mode = Mode::AttachChoice;
                    }
                    // dead / unreachable 拒连（FR-03 表）。
                    Status::Dead => {
                        self.status = Some(format!(
                            "'{name}' is dead; wipe it before connecting (wipe lands in M2)"
                        ));
                    }
                    Status::Unreachable => {
                        self.status =
                            Some(format!("'{name}' is unreachable; check the socket dir"));
                    }
                    Status::Unknown(raw) => {
                        self.status = Some(format!(
                            "'{name}' reports unknown state '{raw}'; refusing to connect"
                        ));
                    }
                }
            }
        }
    }

    fn on_key_attach_choice(&mut self, code: KeyCode) {
        let Some(choice) = self.attach.take() else {
            self.mode = Mode::List;
            return;
        };
        match code {
            // 1 共享 / 2 接管（-d -r，绝不用 -D -r）/ Esc 取消（1.5b）。
            KeyCode::Char('1') => {
                self.mode = Mode::List;
                self.request_attach(AttachKind::Share, choice.name);
            }
            KeyCode::Char('2') => {
                self.mode = Mode::List;
                self.request_attach(AttachKind::Takeover, choice.name);
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                self.mode = Mode::List;
            }
            _ => {
                // 其它按键不消费选择框状态。
                self.attach = Some(choice);
            }
        }
    }

    /// 产出连接请求（歧义名在此时解析为 full name）。
    fn request_attach(&mut self, kind: AttachKind, name: String) {
        let sessions: Vec<SessionRecord> = self.sessions();
        let target = unambiguous_target(&sessions, &name);
        self.attach_request = Some(AttachRequest { kind, target });
    }

    /// 事件循环取走连接请求；`None` 表示无待执行连接。
    pub fn take_attach_request(&mut self) -> Option<AttachRequest> {
        self.attach_request.take()
    }

    /// 连接结果落账（FR-03 验收 2：无论子进程退出码如何，都回到列表，不退出 TUI）。
    pub fn note_attach_outcome(&mut self, request: &AttachRequest, run: &crate::screen::cmd::Run) {
        self.mode = Mode::List;
        // 先刷新再落账：refresh() 成功时会清掉瞬态消息，结果消息必须留在最后。
        self.refresh();
        // 连接过的会话留观察记录（FR-24）：managed 条目只更新 last_seen。
        let name = request
            .target
            .split_once('.')
            .map(|(_, n)| n.to_string())
            .unwrap_or_else(|| request.target.clone());
        self.record_seen(&name);
        self.save_config();
        self.status = Some(if run.success() {
            format!("detached from '{}'", request.target)
        } else {
            format!(
                "screen exited with code {} ({})",
                run.code,
                request.kind.label()
            )
        });
    }

    // ------------------------------------------------------------- 预览（T2.3 / FR-15）

    /// `p` 预览入口：能力/状态门槛在这里拦（给可读原因），抓取请求交事件循环。
    fn open_preview(&mut self) {
        let visible = self.sessions();
        let Some(session) = visible.get(self.selected) else {
            return;
        };
        if matches!(session.status, Status::Dead | Status::Unreachable) {
            self.status = Some(format!(
                "'{}' is not running; there is nothing to preview",
                session.name
            ));
            return;
        }
        if !self.caps.hardcopy.usable() {
            self.status = Some(format!(
                "preview unavailable: hardcopy support is {} on this screen build \
                 (run `stui doctor` for details)",
                self.caps.hardcopy.label()
            ));
            return;
        }
        self.last_preview_target = Some(session.full.clone());
        self.preview_request = Some(PreviewRequest {
            full: session.full.clone(),
            name: session.name.clone(),
            manual: true,
        });
    }

    /// 宽屏右栏（FR-15 验收 5）：预览常驻并随选中项更新 —— 选中项变化时产一次抓取请求。
    /// 失败由事件循环静默降级（pane 显示提示，不用 status 刷屏）。
    pub fn wide_preview_due(&mut self, is_wide: bool) -> Option<PreviewRequest> {
        if !is_wide || self.mode != Mode::List || !self.caps.hardcopy.usable() {
            return None;
        }
        if self.preview_request.is_some() {
            return None;
        }
        let visible = self.sessions();
        let session = visible.get(self.selected)?;
        let full = session.full.clone();
        if self.last_preview_target.as_deref() == Some(full.as_str()) {
            return None;
        }
        let name = session.name.clone();
        self.last_preview_target = Some(full.clone());
        Some(PreviewRequest {
            full,
            name,
            manual: false,
        })
    }

    /// 事件循环取走预览请求。
    pub fn take_preview_request(&mut self) -> Option<PreviewRequest> {
        self.preview_request.take()
    }

    /// 抓取成功落账：视图带抓取时间；手动请求进入弹层。
    pub fn note_preview(&mut self, request: &PreviewRequest, lines: Vec<String>) {
        self.preview = Some(PreviewView {
            full: request.full.clone(),
            name: request.name.clone(),
            fetched: crate::util::time::local_datetime(std::time::SystemTime::now()),
            lines,
        });
        if request.manual {
            self.mode = Mode::Preview;
        }
    }

    /// 抓取失败落账：手动给 status（可读原因），自动只清视图（pane 回落提示），
    /// 绝不让旧视图冒充新会话的画面（FR-15 验收 3）。
    pub fn note_preview_failed(&mut self, request: &PreviewRequest, reason: String) {
        if self
            .preview
            .as_ref()
            .map(|v| v.full == request.full)
            .unwrap_or(false)
        {
            self.preview = None;
        }
        if self.last_preview_target.as_deref() == Some(request.full.as_str()) {
            self.last_preview_target = None;
        }
        if request.manual {
            self.status = Some(reason);
        }
    }

    // ------------------------------------------------------------- 元数据持久化（T2.6 / FR-24）

    /// 配置落盘。返回 `false` = 没写成（无路径 / 只读保护），调用方给用户提示。
    fn save_config(&self) -> bool {
        if self.config_read_only {
            return false;
        }
        let path = self
            .config_path_override
            .clone()
            .or_else(crate::config::config_path);
        matches!(
            crate::config::save_to(&self.config, path.as_deref()),
            Ok(true)
        )
    }

    /// 记录「见过这个会话」：managed 条目保留业务字段，外部会话只留观察记录。
    fn record_seen(&mut self, name: &str) {
        let now = crate::util::time::local_datetime(std::time::SystemTime::now());
        let entry = self.config.sessions.entry(name.to_string()).or_default();
        entry.last_seen = Some(now);
    }

    /// 记录本工具创建的会话（managed，可重启）。
    fn record_managed(&mut self, name: &str, dir: &str, command: &str) {
        let now = crate::util::time::local_datetime(std::time::SystemTime::now());
        let entry = self.config.sessions.entry(name.to_string()).or_default();
        entry.managed = true;
        entry.command = Some(command.to_string());
        entry.cwd = Some(dir.to_string());
        entry.last_seen = Some(now);
    }

    /// `s` 重启入口：只对**本工具创建**（managed）且已 dead 的会话开放；
    /// unmanaged 明确拒绝（FR-24 验收 1：绝不假设有权重启别人的会话）。
    fn restart_selected(&mut self) {
        let Some(session) = self.sessions().get(self.selected).cloned() else {
            return;
        };
        if session.status != Status::Dead {
            self.status = Some(format!(
                "'{}' is still running; restart applies to dead sessions",
                session.name
            ));
            return;
        }
        let Some(meta) = self.config.sessions.get(&session.name).cloned() else {
            self.status = Some(format!(
                "'{}' was not created by stui; restart is unavailable",
                session.name
            ));
            return;
        };
        if !meta.managed {
            self.status = Some(format!(
                "'{}' is unmanaged; restart is only available for sessions created by stui",
                session.name
            ));
            return;
        }
        let (Some(command), Some(cwd)) = (meta.command.clone(), meta.cwd.clone()) else {
            self.status = Some(format!(
                "'{}' has no recorded command/cwd; cannot restart",
                session.name
            ));
            return;
        };
        self.restart_request = Some(RestartRequest {
            name: session.name.clone(),
            dir: PathBuf::from(cwd),
            command,
        });
    }

    /// 重启结果落账。
    pub fn note_restart_outcome(&mut self, request: &RestartRequest, run: &cmd::Run) {
        self.refresh();
        if run.success() {
            self.record_managed(
                &request.name,
                &request.dir.display().to_string(),
                &request.command,
            );
            self.save_config();
            self.status = Some(format!(
                "restarted '{}' with its recorded command",
                request.name
            ));
        } else {
            self.status = Some(format!(
                "restart of '{}' failed (exit {}): {}",
                request.name,
                run.code,
                run.text().trim()
            ));
        }
    }

    /// 事件循环取走重启请求。
    pub fn take_restart_request(&mut self) -> Option<RestartRequest> {
        self.restart_request.take()
    }

    /// `X` 手动元数据清理（GC 策略 = 手动，requirements §14）：删除已消失会话的元数据。
    /// 返回 None = 没有可清理项（status 已给提示）。
    fn open_cleanup(&mut self) {
        let stale = self.stale_metadata_names();
        if stale.is_empty() {
            self.status = Some("no stale session metadata to clean".into());
            return;
        }
        let count = stale.len();
        self.confirm = Some(ConfirmAction {
            kind: ActionKind::Cleanup,
            target: String::new(),
            display: format!("{count} stale metadata entrie(s)"),
            command: Some(stale.join(", ")),
            focus_yes: false,
        });
        self.mode = Mode::Confirm;
    }

    /// 已消失会话的元数据键列表。
    fn stale_metadata_names(&self) -> Vec<String> {
        self.config
            .sessions
            .keys()
            .filter(|name| !self.all_sessions().iter().any(|s| &s.name == *name))
            .cloned()
            .collect()
    }

    /// 执行清理（确认框确认后）：只删元数据，不碰任何 screen 资源。
    fn run_cleanup(&mut self) {
        let stale = self.stale_metadata_names();
        let count = stale.len();
        for name in &stale {
            self.config.sessions.remove(name);
        }
        let saved = self.save_config();
        self.status = Some(if saved {
            format!("removed {count} stale metadata entrie(s)")
        } else {
            format!(
                "removed {count} stale metadata entrie(s) for this session only (no config file written)"
            )
        });
    }

    /// 打开元数据字段编辑（详情弹层 `a` / `t`）。
    fn open_meta_edit(&mut self, field: MetaField) {
        let visible = self.sessions();
        let Some(session) = visible.get(self.selected) else {
            return;
        };
        let name = session.name.clone();
        let current = self
            .config
            .sessions
            .get(&name)
            .and_then(|m| match field {
                MetaField::Alias => m.alias.clone(),
                MetaField::Note => m.note.clone(),
            })
            .unwrap_or_default();
        self.meta_edit = Some(MetaEdit {
            session: name,
            field,
            value: current,
            error: None,
        });
        self.mode = Mode::MetaEdit;
    }

    fn on_key_meta_edit(&mut self, code: KeyCode) {
        let Some(mut edit) = self.meta_edit.take() else {
            self.mode = Mode::List;
            return;
        };
        match code {
            KeyCode::Esc => self.mode = Mode::List,
            KeyCode::Backspace => {
                edit.value.pop();
                self.meta_edit = Some(edit);
            }
            KeyCode::Char(c) if !c.is_control() => {
                edit.value.push(c);
                self.meta_edit = Some(edit);
            }
            KeyCode::Enter => {
                let value = edit.value.trim().to_string();
                let entry = self
                    .config
                    .sessions
                    .entry(edit.session.clone())
                    .or_default();
                match edit.field {
                    MetaField::Alias => entry.alias = (!value.is_empty()).then_some(value),
                    MetaField::Note => entry.note = (!value.is_empty()).then_some(value),
                }
                let field_label = edit.field.key().to_string();
                let session = edit.session;
                let saved = self.save_config();
                self.meta_edit = None;
                self.mode = Mode::List;
                self.status = Some(if saved {
                    format!("'{session}' {field_label} updated")
                } else {
                    format!(
                        "'{session}' {field_label} kept for this session only (no config file written)"
                    )
                });
            }
            _ => self.meta_edit = Some(edit),
        }
    }

    // ------------------------------------------------------------- 会话动作（T2.4）

    /// 打开危险操作确认框（FR-13）。入口即校验（NFR-08 的第一道），
    /// 但**执行前**事件循环还会拿新鲜枚举再验一次 —— 中间只隔确认框，仍可能变化。
    fn open_confirm(&mut self, kind: ActionKind) {
        match kind {
            // Cleanup 有自己的入口（open_cleanup），不经这里。
            ActionKind::Cleanup => return,
            ActionKind::Wipe => {
                if !self.sessions().iter().any(|s| s.status == Status::Dead) {
                    self.status = Some("no dead sessions; nothing to wipe".into());
                    return;
                }
                self.confirm = Some(ConfirmAction {
                    kind,
                    target: String::new(),
                    display: "dead sessions".into(),
                    command: None,
                    focus_yes: false, // 默认焦点在取消（FR-13 验收 1）。
                });
            }
            ActionKind::Detach | ActionKind::Kill => {
                let visible = self.sessions();
                let Some(session) = visible.get(self.selected) else {
                    return;
                };
                if kind == ActionKind::Detach
                    && !matches!(session.status, Status::Attached | Status::Multi)
                {
                    self.status = Some(format!(
                        "'{}' is not attached; nothing to detach (use Enter to connect)",
                        session.name
                    ));
                    return;
                }
                self.confirm = Some(ConfirmAction {
                    kind,
                    target: session.full.clone(),
                    display: session.name.clone(),
                    command: self.meta.as_ref().and_then(|m| m.command.clone()),
                    focus_yes: false,
                });
            }
        }
        self.mode = Mode::Confirm;
    }

    fn on_key_confirm(&mut self, code: KeyCode) {
        let Some(mut confirm) = self.confirm.take() else {
            self.mode = Mode::List;
            return;
        };
        match code {
            // ←/→/Tab 切焦点；Enter 执行焦点项；y 显式确认；n/Esc/q 取消。
            KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                confirm.focus_yes = !confirm.focus_yes;
                self.confirm = Some(confirm);
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                confirm.focus_yes = true;
                self.settle_confirm(confirm);
            }
            KeyCode::Enter => {
                self.settle_confirm(confirm);
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::List;
            }
            _ => {
                self.confirm = Some(confirm);
            }
        }
    }

    /// 确认框落定：Cleanup 纯配置操作直接执行；其余产出动作请求交事件循环。
    fn settle_confirm(&mut self, confirm: ConfirmAction) {
        self.mode = Mode::List;
        if !confirm.focus_yes {
            return; // 焦点在取消：只关闭。
        }
        match confirm.kind {
            ActionKind::Cleanup => self.run_cleanup(),
            _ => self.action_request = Some(confirm),
        }
    }

    /// 事件循环取走动作请求；`None` = 无待执行动作。
    pub fn take_action(&mut self) -> Option<ConfirmAction> {
        self.action_request.take()
    }

    /// 动作执行前的重校验（NFR-08）：吃注入的新鲜枚举，不合格一律拒绝并说明。
    /// 新鲜枚举会更新到列表（与会话消失的提示保持一致，FR-03 验收 3 同款体验）。
    pub fn validate_action(
        &mut self,
        fresh: Enumeration,
        action: &ConfirmAction,
    ) -> Result<(), String> {
        let dead_count = fresh
            .list
            .sessions
            .iter()
            .filter(|s| s.status == Status::Dead)
            .count();
        let found = fresh
            .list
            .sessions
            .iter()
            .find(|s| s.full == action.target)
            .map(|s| s.status.clone());
        self.apply_enumeration(fresh);

        match action.kind {
            ActionKind::Wipe => {
                if dead_count == 0 {
                    Err("no dead sessions left; nothing to wipe".into())
                } else {
                    Ok(())
                }
            }
            ActionKind::Kill => match found {
                Some(_) => Ok(()),
                None => Err(format!("'{}' is gone; nothing to kill", action.display)),
            },
            ActionKind::Detach => match found {
                Some(Status::Attached | Status::Multi) => Ok(()),
                Some(other) => Err(format!(
                    "'{}' is no longer attached (now {}); nothing to detach",
                    action.display,
                    other.label()
                )),
                None => Err(format!("'{}' is gone; nothing to detach", action.display)),
            },
            // Cleanup 只动本工具的配置，无 screen 语义可校验；确认框内直接执行，
            // 正常不会走到这里（防御性放行）。
            ActionKind::Cleanup => Ok(()),
        }
    }

    /// 动作结果落账：先刷新再给结论（refresh 会清瞬态消息，顺序不能反）。
    pub fn note_action_outcome(&mut self, action: &ConfirmAction, run: &cmd::Run) {
        self.refresh();
        self.status = Some(if run.success() {
            match action.kind {
                ActionKind::Wipe => "dead sessions wiped".to_string(),
                _ => format!("'{}' {} done", action.display, action.kind.label()),
            }
        } else {
            let detail = run.text();
            let detail = detail.trim();
            format!(
                "{} '{}' failed (exit {}){}",
                action.kind.label(),
                action.display,
                run.code,
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            )
        });
    }

    // ------------------------------------------------------------- 重命名（T2.4 / FR-14）

    fn open_rename(&mut self) {
        let visible = self.sessions();
        let Some(session) = visible.get(self.selected) else {
            return;
        };
        self.rename = Some(RenameDraft {
            target: session.full.clone(),
            name: session.name.clone(),
            error: None,
        });
        self.mode = Mode::Rename;
    }

    fn on_key_rename(&mut self, code: KeyCode) {
        let Some(mut draft) = self.rename.take() else {
            self.mode = Mode::List;
            return;
        };
        match code {
            KeyCode::Esc => {
                self.mode = Mode::List;
            }
            KeyCode::Backspace => {
                draft.error = None;
                draft.name.pop();
                self.rename = Some(draft);
            }
            KeyCode::Char(c) if !c.is_control() => {
                draft.error = None;
                draft.name.push(c);
                self.rename = Some(draft);
            }
            KeyCode::Enter => {
                let name = draft.name.trim().to_string();
                if let Err(err) = validate_name(&name) {
                    draft.error = Some(err);
                    self.rename = Some(draft);
                    return;
                }
                self.mode = Mode::List;
                self.rename_request = Some(RenameRequest {
                    target: draft.target,
                    new_name: name,
                });
            }
            _ => {
                self.rename = Some(draft);
            }
        }
    }

    /// 事件循环取走重命名请求。
    pub fn take_rename_request(&mut self) -> Option<RenameRequest> {
        self.rename_request.take()
    }

    /// 重命名结果落账（FR-14 验收：列表立即按新名显示）。
    pub fn note_rename_outcome(&mut self, request: &RenameRequest, run: &cmd::Run) {
        self.refresh();
        self.status = Some(if run.success() {
            format!("session renamed to '{}'", request.new_name)
        } else {
            format!("rename failed (exit {}): {}", run.code, run.text().trim())
        });
    }

    fn on_key_overlay(&mut self, code: KeyCode) {
        match code {
            // 任何界面下 q / Esc 回上一层（requirements §9）；`i` 再次按下同样关闭。
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('i') | KeyCode::Char('?') => {
                self.mode = Mode::List;
            }
            _ => {}
        }
    }

    /// 详情弹层（T2.6）：`a` 别名、`t` 描述（FR-24），Esc/i/q 关闭。
    fn on_key_detail(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('a') => self.open_meta_edit(MetaField::Alias),
            KeyCode::Char('t') => self.open_meta_edit(MetaField::Note),
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('i') => self.mode = Mode::List,
            _ => {}
        }
    }

    // ------------------------------------------------------------- 新建会话（T1.4）

    /// 打开三步向导：预填当前目录、默认名、`$SHELL`（FR-02 验收 1）。
    pub fn open_new_session(&mut self) {
        let dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        self.draft = Some(NewDraft {
            step: NewStep::Name,
            name: default_session_name(&dir, std::time::SystemTime::now()),
            dir: dir.display().to_string(),
            command: default_command(),
            error: None,
            note: None,
        });
        self.mode = Mode::NewSession;
    }

    fn on_key_new(&mut self, code: KeyCode) {
        let Some(draft) = &mut self.draft else {
            // 草稿丢失（不应发生）：回到列表而不是卡死。
            self.mode = Mode::List;
            return;
        };
        match code {
            KeyCode::Esc => {
                self.draft = None;
                self.mode = Mode::List;
            }
            KeyCode::Enter => self.advance_new(),
            KeyCode::Backspace => {
                draft.error = None;
                let field = current_field_mut(draft);
                field.pop();
            }
            KeyCode::Char(c) if !c.is_control() => {
                draft.error = None;
                let field = current_field_mut(draft);
                field.push(c);
            }
            _ => {}
        }
    }

    /// 回车推进：当前步校验通过后进入下一步；最后一步触发真实创建。
    fn advance_new(&mut self) {
        let Some(mut draft) = self.draft.take() else {
            self.mode = Mode::List;
            return;
        };

        match draft.step {
            NewStep::Name => {
                draft.name = draft.name.trim().to_string();
                if let Err(err) = validate_name(&draft.name) {
                    draft.error = Some(err);
                    self.draft = Some(draft);
                    return;
                }
                draft.note = self.duplicate_note(&draft.name);
                draft.error = None;
                draft.step = NewStep::Dir;
                self.draft = Some(draft);
            }
            NewStep::Dir => {
                draft.dir = expand_tilde(draft.dir.trim());
                if draft.dir.is_empty() {
                    draft.error = Some("directory must not be empty".into());
                    self.draft = Some(draft);
                    return;
                }
                if !std::path::Path::new(&draft.dir).is_dir() {
                    draft.error = Some(format!("not a directory: {}", draft.dir));
                    self.draft = Some(draft);
                    return;
                }
                draft.error = None;
                draft.step = NewStep::Command;
                self.draft = Some(draft);
            }
            NewStep::Command => {
                draft.command = draft.command.trim().to_string();
                if draft.command.is_empty() {
                    draft.error = Some("command must not be empty".into());
                    self.draft = Some(draft);
                    return;
                }
                let name = draft.name.clone();
                let dir = draft.dir.clone();
                let command = draft.command.clone();
                let note = draft.note.take();

                match self.create_session(&name, &dir, &command) {
                    Ok(selected) => {
                        // 成功：草稿丢弃，回列表，选中并确认新会话（FR-02 验收 5）。
                        self.draft = None;
                        self.mode = Mode::List;
                        self.selected = selected;
                        self.status = Some(match note {
                            Some(hint) => format!("created '{name}' ({hint})"),
                            None => format!("created '{name}'"),
                        });
                        self.refresh();
                    }
                    Err(message) => {
                        // 失败：草稿保留在最后一步，可行动报错，不静默（FR-02 验收 4）。
                        draft.error = Some(message);
                        self.draft = Some(draft);
                    }
                }
            }
        }
    }

    /// 重名提示（非阻断）：FR-02 验收 2 —— 允许创建，提示「将以 `<pid>.<name>` 寻址」。
    fn duplicate_note(&self, name: &str) -> Option<String> {
        self.sessions()
            .iter()
            .any(|s| s.name == name)
            .then(|| format!("a session named '{name}' already exists; address it as <pid>.{name}"))
    }

    /// 执行创建并刷新列表。成功返回新会话在（刷新后）列表中的下标。
    fn create_session(&mut self, name: &str, dir: &str, command: &str) -> Result<usize, String> {
        let path = std::path::PathBuf::from(dir);
        let run =
            cmd::create(name, &path, command).map_err(|err| format!("create failed: {err}"))?;

        if !run.success() {
            let detail = run.text();
            let detail = detail.trim();
            return Err(format!(
                "screen refused to create '{name}' (exit {}):\n  {} ran in {dir}\n  {}",
                run.code,
                run.command,
                if detail.is_empty() {
                    "screen produced no diagnostic output; check the name and directory"
                } else {
                    detail
                }
            ));
        }

        // 创建成功：记录 managed 元数据（FR-24 验收 1：stui 创建 = 可重启）。
        self.record_managed(name, dir, command);
        self.save_config();

        // 创建成功后立刻重枚举，把选中项对准新会话（FR-02 验收 5）。
        match parse::enumerate() {
            Ok(enumeration) => {
                let index = enumeration
                    .list
                    .sessions
                    .iter()
                    .position(|s| s.name == name);
                self.apply_enumeration(enumeration);
                // 绕过了 refresh()，这里补计时起点，避免下一轮立即重复枚举。
                self.last_refresh = Some(Instant::now());
                Ok(index.unwrap_or(0))
            }
            Err(_) => Ok(0), // 列表刷新失败不回滚创建本身；下个轮询周期自会补上。
        }
    }
}

/// 当前草稿步对应的可编辑字段。
fn current_field_mut(draft: &mut NewDraft) -> &mut String {
    match draft.step {
        NewStep::Name => &mut draft.name,
        NewStep::Dir => &mut draft.dir,
        NewStep::Command => &mut draft.command,
    }
}

/// TUI 入口：环境检查 → 探测 → 守护终端 → 事件循环。返回进程退出码。
pub fn run() -> u8 {
    use std::io::IsTerminal;

    // TUI 需要 stdin/stdout 都是 TTY；非交互场景明确指路 `stui ls`（NFR-09）。
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!("stui: no interactive terminal attached; use `stui ls` for non-interactive use");
        return crate::cli::EXIT_ENV;
    }

    // 配置先行（T2.1）：加载失败/损坏已在本层降级，warnings 随首帧给用户。
    let loaded = crate::config::load();

    let caps = match Caps::detect(None) {
        Ok(caps) => caps,
        Err(err) => {
            eprintln!("stui: {err}");
            return crate::cli::EXIT_ENV;
        }
    };

    ui::install_hooks();

    let mut guard = match ui::TuiGuard::enter() {
        Ok(guard) => guard,
        Err(err) => {
            eprintln!("stui: cannot take over the terminal: {err}");
            return crate::cli::EXIT_ENV;
        }
    };
    let mut terminal = match ui::new_terminal() {
        Ok(terminal) => terminal,
        Err(err) => {
            drop(guard);
            eprintln!("stui: cannot initialize rendering: {err}");
            return crate::cli::EXIT_ENV;
        }
    };

    // escape 前缀（FR-18）：配置显式覆盖 > `.screenrc` 探测 > 默认（None 回退）。
    let explicit_prefix = loaded.config.defaults.escape_prefix.clone();
    let mut app = App::with_config(caps, loaded.config);
    app.escape_prefix = match explicit_prefix {
        Some(explicit) => Some(explicit),
        None => detect_escape_prefix(),
    };
    // $STY 非空 = 已经在一个 screen 会话里（FR-03 验收 5）：警告一次，不阻塞。
    if std::env::var("STY").map(|v| !v.is_empty()).unwrap_or(false) {
        app.status = Some(
            "already inside a screen session ($STY); nested connections can be confusing".into(),
        );
    }
    app.refresh();
    // 配置警告在首帧后给出（refresh 会清瞬态消息，这条必须在它之后落）。
    if !loaded.warnings.is_empty() {
        app.status = Some(loaded.warnings.join("; "));
    }

    let outcome = event_loop(&mut terminal, &mut guard, &mut app);

    // 显式 drop 顺序：先终端后守护，避免后端在已还原的终端上再写一笔。
    drop(terminal);
    drop(guard);

    match outcome {
        Ok(()) => crate::cli::EXIT_OK,
        Err(err) => {
            eprintln!("stui: {err}");
            crate::cli::EXIT_FAILURE
        }
    }
}

fn event_loop(
    terminal: &mut ui::TuiTerminal,
    guard: &mut ui::TuiGuard,
    app: &mut App,
) -> std::io::Result<()> {
    while !app.should_quit {
        terminal.draw(|f| ui::render(f, app))?;

        // 封顶 200ms：信号标志最长延迟 200ms 被看到；其余时间阻塞在 poll，非忙等（NFR-03）。
        let timeout = app.next_tick_in().min(POLL_CAP);
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
                // resize 后下一次 draw 自动按新尺寸重排（FR-04 验收 1）。
                Event::Resize(_, _) => {}
                _ => {}
            }
        }

        if ui::shutdown_requested() {
            // 外部 SIGINT/SIGTERM：走正常退出路径，RAII 负责还原终端。
            break;
        }

        if app.tick_due() {
            app.refresh();
        }

        // 连接请求：suspend → 前台 screen → resume → 强制重绘（1.5d）。
        if let Some(request) = app.take_attach_request() {
            let hint = detach_hint_text(app.escape_prefix.as_deref());
            match attach_foreground(terminal, guard, &request, &hint) {
                Ok(run) => app.note_attach_outcome(&request, &run),
                Err(err) => {
                    app.mode = Mode::List;
                    app.status = Some(format!("attach failed: {err}"));
                }
            }
            // 子进程画过屏幕：清掉 ratatui 的 diff 基线，强制整屏重绘。
            terminal.clear()?;
        }

        // 危险动作（T2.4）：确认框通过后，**执行前**拿新鲜枚举重校验（NFR-08）。
        if let Some(action) = app.take_action() {
            let validation = match (app.enumerate)() {
                Ok(fresh) => app.validate_action(fresh, &action),
                Err(err) => Err(format!(
                    "cannot verify sessions before {}: {err}",
                    action.kind.label()
                )),
            };
            match validation {
                Ok(()) => match action.kind.to_cmd() {
                    Some(session_action) => match cmd::action(session_action, &action.target) {
                        Ok(run) => app.note_action_outcome(&action, &run),
                        Err(err) => {
                            app.refresh();
                            app.status = Some(format!("{} failed: {err}", action.kind.label()));
                        }
                    },
                    // Cleanup 在确认框内直接执行，不产动作请求（防御性兜底）。
                    None => app.status = Some("nothing to do".into()),
                },
                Err(message) => {
                    app.refresh();
                    app.status = Some(message);
                }
            }
        }

        // 重命名（T2.4 / FR-14）：`-X sessionname` 是亚秒级动作，直接在循环里执行。
        if let Some(request) = app.take_rename_request() {
            match cmd::rename(&request.target, &request.new_name) {
                Ok(run) => app.note_rename_outcome(&request, &run),
                Err(err) => {
                    app.refresh();
                    app.status = Some(format!("rename failed: {err}"));
                }
            }
        }

        // 预览抓取（T2.3）：用户按 p 的手动请求。
        if let Some(request) = app.take_preview_request() {
            execute_preview(app, &request);
        }

        // 重启（T2.6）：managed 会话用记录的 command+cwd 重建。
        if let Some(request) = app.take_restart_request() {
            match cmd::create(&request.name, &request.dir, &request.command) {
                Ok(run) => app.note_restart_outcome(&request, &run),
                Err(err) => {
                    app.refresh();
                    app.status = Some(format!("restart failed: {err}"));
                }
            }
        }

        // 宽屏右栏自动预览（FR-15 验收 5）：选中项变化才抓，失败静默降级。
        let size = terminal.size()?;
        let is_wide = ui::layout::Tier::from_size_with(
            size.width,
            size.height,
            app.config.ui.narrow_cols,
            app.config.ui.wide_cols,
        ) == ui::layout::Tier::Wide;
        if let Some(request) = app.wide_preview_due(is_wide) {
            execute_preview(app, &request);
        }
    }
    Ok(())
}

/// 执行一次预览抓取并落账（事件循环侧；请求是否手动决定失败时的告知方式）。
fn execute_preview(app: &mut App, request: &PreviewRequest) {
    match crate::screen::preview::preview_with(&app.caps.program, &request.full) {
        Ok(lines) => app.note_preview(request, lines),
        Err(err) => app.note_preview_failed(request, err.to_string()),
    }
}

/// 前台执行连接（1.5d）：spawn 而非 exec —— exec 会替换进程，detach 后无法回到 TUI。
fn attach_foreground(
    terminal: &mut ui::TuiTerminal,
    guard: &mut ui::TuiGuard,
    request: &AttachRequest,
    hint: &str,
) -> crate::screen::Result<crate::screen::cmd::Run> {
    use std::io::Write;

    // 离开备用屏前把缓冲刷掉，然后还原终端给 screen。
    terminal.flush()?;
    guard.suspend()?;

    // 1.5c：detach 提示打印到真实终端（留在滚动缓冲里，不进 TUI 画面）。
    // FR-18：前缀按探测/配置结果给出。
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "{hint}");
    let _ = stdout.flush();

    let run = cmd::attach(request.kind, &request.target);

    // 子进程退出（无论退出码是什么）→ 恢复 TUI（FR-03 验收 2 头号契约）。
    guard.resume()?;
    run
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::parse::Outlook;

    /// 用解析器真实产物构造枚举结果（不跑 screen）。
    fn enumeration(text: &str) -> Enumeration {
        Enumeration {
            outlook: Outlook::Inconclusive(8),
            list: parse::parse_list_output(text).unwrap_or_default(),
            list_error: None,
        }
    }

    const FOUR: &str = "There are screens on:\n\t12345.work\t(09/23/2026 10:00:00 AM)\t(Detached)\n\t12346.llm\t(09/23/2026 10:01:00 AM)\t(Attached)\n\t12347.dep\t(09/23/2026 10:02:00 AM)\t(Detached)\n\t12348.legacy\t(09/23/2026 10:03:00 AM)\t(Dead ???)\n4 Sockets in /tmp/.screen.\n";

    fn app_with(text: &str) -> App {
        let mut app = App::new(Caps::default());
        app.apply_enumeration(enumeration(text));
        app
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, crossterm::event::KeyModifiers::empty())
    }

    #[test]
    fn selection_moves_and_clamps() {
        let mut app = app_with(FOUR);
        assert_eq!(app.selected, 0);
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.selected, 1);
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.selected, 0);
        // 连按 10 次 down，最多到最后一行，不越界。
        for _ in 0..10 {
            app.on_key(key(KeyCode::Down));
        }
        assert_eq!(app.selected, 3);
        for _ in 0..10 {
            app.on_key(key(KeyCode::Char('k')));
        }
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn refresh_keeps_selection_and_clamps() {
        let mut app = app_with(FOUR);
        app.selected = 3;
        // 会话变少：选中项被 clamp，不越界。
        app.apply_enumeration(enumeration(
            "There is a screen on:\n\t12345.work\t(09/23/2026 10:00:00 AM)\t(Detached)\n1 Socket in /tmp/.screen.\n",
        ));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn apply_enumeration_does_not_reset_selection() {
        let mut app = app_with(FOUR);
        app.selected = 2;
        app.apply_enumeration(enumeration(FOUR));
        assert_eq!(app.selected, 2);
    }

    #[test]
    fn mode_transitions_follow_the_keymap() {
        let mut app = app_with(FOUR);
        app.on_key(key(KeyCode::Char('?')));
        assert_eq!(app.mode, Mode::Help);
        // Help 层 q / Esc 只回 List，不退出。
        app.on_key(key(KeyCode::Char('q')));
        assert_eq!(app.mode, Mode::List);
        assert!(!app.should_quit);
        app.on_key(key(KeyCode::Char('?')));
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::List);
        // List 层 q 才退出。
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.should_quit);
    }

    #[test]
    fn release_events_are_ignored() {
        let mut app = app_with(FOUR);
        let mut release = key(KeyCode::Char('q'));
        release.kind = KeyEventKind::Release;
        app.on_key(release);
        assert!(!app.should_quit);
        let mut repeat = key(KeyCode::Char('j'));
        repeat.kind = KeyEventKind::Repeat;
        app.on_key(repeat);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn tick_scheduling_respects_manual_refresh() {
        let mut app = app_with(FOUR);
        // 尚未刷新过：下一轮立即刷新。
        assert_eq!(app.next_tick_in(), Duration::ZERO);
        assert!(!app.tick_due());

        app.refresh_interval = Duration::from_millis(1);
        app.refresh();
        // 刚刷完：不该立刻 tick。
        assert!(!app.tick_due());
        std::thread::sleep(Duration::from_millis(5));
        assert!(app.tick_due());

        // 手动刷新把计时器重置。
        app.refresh();
        assert!(!app.tick_due());
    }

    #[test]
    fn empty_list_selects_nothing_and_stays_safe() {
        let mut app = app_with("No Sockets found in /tmp/.screen.\n");
        assert!(app.sessions().is_empty());
        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Char('k')));
        assert_eq!(app.selected, 0);
    }

    // ------------------------------------------------------------- T1.4 新建向导

    #[test]
    fn name_validation_rejects_the_documented_cases() {
        assert!(validate_name("work").is_ok());
        assert!(validate_name("a.b_c-d").is_ok());
        // 空名。
        assert!(validate_name("").is_err());
        // 前导 `-` 会被 screen 当选项。
        assert!(validate_name("-rf").is_err());
        // 空白与控制字符。
        assert!(validate_name("a b").is_err());
        assert!(validate_name("a\nb").is_err());
        // 超长。
        assert!(validate_name(&"a".repeat(NAME_MAX)).is_ok());
        assert!(validate_name(&"a".repeat(NAME_MAX + 1)).is_err());
    }

    #[test]
    fn default_name_is_dirname_plus_timestamp() {
        let now = std::time::SystemTime::now();
        let name = default_session_name(std::path::Path::new("/Users/x/my proj"), now);
        // 空格被清洗成 `-`，保证默认值必过校验（FR-02 验收 1）。
        assert!(name.starts_with("my-proj-"), "{name}");
        assert!(validate_name(&name).is_ok(), "{name}");
        // `MMDD-HHMM` 尾巴。
        let tail = &name["my-proj-".len()..];
        assert_eq!(tail.len(), 9, "{name}");
        assert_eq!(tail.as_bytes()[4], b'-');
    }

    #[test]
    fn tilde_expands_only_known_forms() {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            return; // 无 HOME 的环境跳过（分支行为已由调用方兜底）。
        }
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/work"), format!("{home}/work"));
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
        // `~user` 不支持，原样保留（交给目录存在性检查报错）。
        assert_eq!(expand_tilde("~root/x"), "~root/x");
    }

    #[test]
    fn wizard_opens_with_prefilled_defaults() {
        let mut app = app_with(FOUR);
        app.on_key(key(KeyCode::Char('n')));
        assert_eq!(app.mode, Mode::NewSession);
        let draft = app.draft.as_ref().expect("draft created");
        assert_eq!(draft.step, NewStep::Name);
        assert!(!draft.name.is_empty());
        assert!(
            validate_name(&draft.name).is_ok(),
            "default must pass: {}",
            draft.name
        );
        assert_eq!(draft.command, default_command());
        // 再按 n 不叠加草稿。
        app.on_key(key(KeyCode::Char('n')));
        assert_eq!(app.draft.as_ref().unwrap().step, NewStep::Name);
    }

    #[test]
    fn wizard_esc_cancels_and_returns_to_list() {
        let mut app = app_with(FOUR);
        app.on_key(key(KeyCode::Char('n')));
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::List);
        assert!(app.draft.is_none());
    }

    #[test]
    fn wizard_input_edits_only_the_current_step() {
        let mut app = app_with(FOUR);
        app.open_new_session();
        let original_name = app.draft.as_ref().unwrap().name.clone();

        app.on_key(key(KeyCode::Char('x')));
        assert_eq!(
            app.draft.as_ref().unwrap().name,
            format!("{original_name}x")
        );

        app.on_key(key(KeyCode::Backspace));
        assert_eq!(app.draft.as_ref().unwrap().name, original_name);

        // 目录步里输入不会误改名字。
        app.draft.as_mut().unwrap().step = NewStep::Dir;
        app.on_key(key(KeyCode::Char('/')));
        assert!(app.draft.as_ref().unwrap().dir.ends_with('/'));
        assert_eq!(app.draft.as_ref().unwrap().name, original_name);
    }

    #[test]
    fn wizard_rejects_invalid_name_and_stays_on_step() {
        let mut app = app_with(FOUR);
        app.open_new_session();
        app.draft.as_mut().unwrap().name = "-bad".into();

        app.on_key(key(KeyCode::Enter));
        let draft = app.draft.as_ref().unwrap();
        assert_eq!(draft.step, NewStep::Name, "stays on the name step");
        assert!(draft.error.is_some(), "reports the reason");
    }

    #[test]
    fn wizard_advances_through_valid_steps() {
        let mut app = app_with(FOUR);
        app.open_new_session();

        app.on_key(key(KeyCode::Enter)); // 默认名合法 → Dir
        assert_eq!(app.draft.as_ref().unwrap().step, NewStep::Dir);

        app.on_key(key(KeyCode::Enter)); // 默认目录合法 → Command
        assert_eq!(app.draft.as_ref().unwrap().step, NewStep::Command);
        // 不真实创建：到此为止，Esc 退出。
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::List);
    }

    #[test]
    fn duplicate_name_gets_a_note_but_is_allowed() {
        let mut app = app_with(FOUR); // 含 work / llm / dep / legacy
        app.open_new_session();
        app.draft.as_mut().unwrap().name = "work".into();

        app.on_key(key(KeyCode::Enter));
        let draft = app.draft.as_ref().unwrap();
        assert_eq!(draft.step, NewStep::Dir, "duplicate does not block");
        assert!(draft.error.is_none());
        let note = draft.note.as_deref().expect("duplicate note set");
        assert!(note.contains("<pid>.work"), "{note}");
    }

    #[test]
    fn nonexistent_dir_is_rejected_before_creation() {
        let mut app = app_with(FOUR);
        app.open_new_session();
        let draft = app.draft.as_mut().unwrap();
        draft.step = NewStep::Dir;
        draft.dir = "/no/such/dir/stui-test".into();

        app.on_key(key(KeyCode::Enter));
        let draft = app.draft.as_ref().unwrap();
        assert_eq!(draft.step, NewStep::Dir);
        assert!(
            draft
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("not a directory")
        );
    }

    // ------------------------------------------------------------- T1.5 连接闭环

    #[test]
    fn connect_is_refused_when_session_is_gone() {
        let mut app = app_with(FOUR);
        app.selected = 0; // dep（解析后按名称排序）
        // 新鲜枚举里 dep 已不存在。
        let fresh = enumeration(
            "There is a screen on:\n\t99999.other\t(09/23/2026 11:00:00 AM)\t(Detached)\n1 Socket in /tmp/.screen.\n",
        );
        app.plan_connect(fresh);

        assert!(app.take_attach_request().is_none(), "must not attach");
        assert!(
            app.status
                .as_deref()
                .unwrap_or_default()
                .contains("'dep' is gone"),
            "{}",
            app.status.as_deref().unwrap_or_default()
        );
        // 列表已被刷新（只剩 1 条）。
        assert_eq!(app.sessions().len(), 1);
    }

    #[test]
    fn detached_session_connects_directly() {
        let mut app = app_with(FOUR);
        app.selected = 0; // dep (Detached)
        app.plan_connect(enumeration(FOUR));

        let request = app.take_attach_request().expect("attach request produced");
        assert_eq!(request.kind, AttachKind::Resume);
        assert_eq!(request.target, "dep");
        // 取走后不重复。
        assert!(app.take_attach_request().is_none());
    }

    #[test]
    fn attached_session_opens_the_conflict_choice() {
        let mut app = app_with(FOUR);
        app.selected = 2; // llm (Attached)
        app.plan_connect(enumeration(FOUR));

        assert_eq!(app.mode, Mode::AttachChoice);
        assert!(
            app.take_attach_request().is_none(),
            "waiting for user choice"
        );
        assert_eq!(app.attach.as_ref().unwrap().name, "llm");

        // 2 → 接管（-d -r）。
        app.on_key(key(KeyCode::Char('2')));
        let request = app.take_attach_request().unwrap();
        assert_eq!(request.kind, AttachKind::Takeover);
        assert_eq!(app.mode, Mode::List);
    }

    #[test]
    fn conflict_choice_sharing_and_cancelling() {
        let mut app = app_with(FOUR);
        app.selected = 2; // llm (Attached)
        app.plan_connect(enumeration(FOUR));

        // 1 → 共享。
        app.on_key(key(KeyCode::Char('1')));
        let request = app.take_attach_request().unwrap();
        assert_eq!(request.kind, AttachKind::Share);

        // Esc → 取消，无请求。
        app.selected = 1;
        app.plan_connect(enumeration(FOUR));
        app.on_key(key(KeyCode::Esc));
        assert!(app.take_attach_request().is_none());
        assert_eq!(app.mode, Mode::List);
    }

    #[test]
    fn multi_session_choice_carries_a_size_warning() {
        let text = "There are screens on:\n\t12345.share\t(09/23/2026 10:00:00 AM)\t(Multi)\n1 Socket in /tmp/.screen.\n";
        let mut app = app_with(text);
        app.plan_connect(enumeration(text));

        assert_eq!(app.mode, Mode::AttachChoice);
        let note = app
            .attach
            .as_ref()
            .unwrap()
            .note
            .as_deref()
            .expect("size note");
        assert!(note.contains("resize"), "{note}");
    }

    #[test]
    fn dead_and_unknown_sessions_are_refused() {
        let mut app = app_with(FOUR);
        app.selected = 1; // legacy (Dead)
        app.plan_connect(enumeration(FOUR));
        assert!(app.take_attach_request().is_none(), "dead must be refused");
        assert!(app.status.as_deref().unwrap_or_default().contains("dead"));

        // 未知状态同样拒连（C-5：不猜）。
        let text = "There is a screen on:\n\t12345.weird\t(09/23/2026 10:00:00 AM)\t(???)\n1 Socket in /tmp/.screen.\n";
        let mut app = app_with(text);
        app.plan_connect(enumeration(text));
        assert!(app.take_attach_request().is_none());
        assert!(
            app.status
                .as_deref()
                .unwrap_or_default()
                .contains("unknown state")
        );
    }

    #[test]
    fn ambiguous_names_fall_back_to_full_address() {
        let text = "There are screens on:\n\t111.dup\t(09/23/2026 10:00:00 AM)\t(Detached)\n\t222.dup\t(09/23/2026 10:01:00 AM)\t(Detached)\n2 Sockets in /tmp/.screen.\n";
        let sessions: Vec<SessionRecord> = parse::parse_list_output(text).unwrap().sessions;

        // 同名两个 → 回退到首个匹配的 full（111.dup）。
        let target = unambiguous_target(&sessions, "dup");
        assert_eq!(target, "111.dup", "{target}");
    }

    #[test]
    fn attach_outcome_always_returns_to_the_list() {
        let mut app = app_with(FOUR);
        app.mode = Mode::AttachChoice;

        let request = AttachRequest {
            kind: AttachKind::Resume,
            target: "work".into(),
        };

        // 成功。
        let ok_run = crate::screen::cmd::Run {
            command: "screen -U -r work".into(),
            code: 0,
            stdout: String::new(),
            stderr: String::new(),
        };
        app.note_attach_outcome(&request, &ok_run);
        assert_eq!(app.mode, Mode::List);
        assert!(!app.should_quit);
        assert!(
            app.status
                .as_deref()
                .unwrap_or_default()
                .contains("detached")
        );

        // 非零退出码：回列表并如实报告，绝不吞掉（1.5d 替身契约）。
        let fail_run = crate::screen::cmd::Run {
            command: "screen -U -r work".into(),
            code: 7,
            stdout: String::new(),
            stderr: String::new(),
        };
        app.note_attach_outcome(&request, &fail_run);
        assert_eq!(app.mode, Mode::List);
        assert!(app.status.as_deref().unwrap_or_default().contains("7"));
    }

    #[test]
    fn detach_hint_text_covers_probed_and_default_paths() {
        // 探测到前缀：按实际前缀提示（FR-18 验收）。
        assert_eq!(
            detach_hint_text(Some("Ctrl-]")),
            "Tip: detach with Ctrl-] d"
        );
        // 探测不到：回退默认并注明可自定义。
        let fallback = detach_hint_text(None);
        assert!(fallback.contains("Ctrl-A D"), "{fallback}");
        assert!(fallback.contains("your own prefix"), "{fallback}");
    }

    // ------------------------------------------------------------- T2.4 会话操作

    /// 构造一个确认动作（测试辅助）。
    fn confirm_action(kind: ActionKind, full: &str) -> ConfirmAction {
        ConfirmAction {
            kind,
            target: full.into(),
            display: full.split('.').nth(1).unwrap_or(full).into(),
            command: None,
            focus_yes: false,
        }
    }

    #[test]
    fn confirm_defaults_to_cancel_and_y_explicitly_confirms() {
        let mut app = app_with(FOUR);
        app.selected = 2; // llm（attached）

        // D 打开 detach 确认。
        app.on_key(key(KeyCode::Char('D')));
        assert_eq!(app.mode, Mode::Confirm);
        let confirm = app.confirm.as_ref().unwrap();
        assert_eq!(confirm.kind, ActionKind::Detach);
        assert_eq!(confirm.target, "12346.llm");
        assert!(!confirm.focus_yes, "default focus must be cancel (FR-13)");

        // Enter（焦点在取消）→ 只关闭，不执行。
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.mode, Mode::List);
        assert!(app.take_action().is_none());

        // y 显式确认 → 产出动作请求。
        app.on_key(key(KeyCode::Char('D')));
        app.on_key(key(KeyCode::Char('y')));
        let action = app.take_action().expect("y must confirm");
        assert_eq!(action.kind, ActionKind::Detach);
        assert!(app.take_action().is_none(), "request consumed once");
    }

    #[test]
    fn confirm_focus_toggle_redirects_enter() {
        let mut app = app_with(FOUR);
        app.selected = 0; // dep（detached）→ K 终止确认
        app.on_key(key(KeyCode::Char('K')));
        assert_eq!(app.mode, Mode::Confirm);
        assert_eq!(app.confirm.as_ref().unwrap().kind, ActionKind::Kill);

        // Tab 切到「确认」，Enter 执行焦点项。
        app.on_key(key(KeyCode::Tab));
        assert!(app.confirm.as_ref().unwrap().focus_yes);
        app.on_key(key(KeyCode::Enter));
        assert!(app.take_action().is_some());

        // n / Esc 任何焦点下都取消。
        app.on_key(key(KeyCode::Char('K')));
        app.on_key(key(KeyCode::Char('n')));
        assert_eq!(app.mode, Mode::List);
        assert!(app.take_action().is_none());
    }

    #[test]
    fn kill_confirm_carries_probed_command() {
        let mut app = app_with(FOUR);
        app.selected = 0;
        app.meta = Some(probe::Meta {
            cwd: Some("/srv/app".into()),
            command: Some("/bin/zsh -l".into()),
        });
        app.on_key(key(KeyCode::Char('K')));
        let confirm = app.confirm.as_ref().unwrap();
        // FR-13 验收 2：确认框里显示会话名 + 运行命令。
        assert_eq!(confirm.display, "dep");
        assert_eq!(confirm.command.as_deref(), Some("/bin/zsh -l"));
    }

    #[test]
    fn detach_entry_is_restricted_to_attached_sessions() {
        let mut app = app_with(FOUR);
        app.selected = 0; // dep（detached）
        app.on_key(key(KeyCode::Char('D')));
        assert_eq!(
            app.mode,
            Mode::List,
            "detached target must not open confirm"
        );
        assert!(
            app.status
                .as_deref()
                .unwrap_or_default()
                .contains("not attached")
        );
    }

    #[test]
    fn wipe_entry_lists_dead_sessions_and_wipe_without_dead_is_refused() {
        let mut app = app_with(FOUR); // legacy 是 dead
        app.on_key(key(KeyCode::Char('W')));
        assert_eq!(app.mode, Mode::Confirm);
        assert_eq!(app.confirm.as_ref().unwrap().kind, ActionKind::Wipe);
        app.on_key(key(KeyCode::Esc));

        // 无 dead 会话时 W 只给提示，不弹确认框。
        let mut clean = app_with(
            "There is a screen on:\n\t12345.work\t(09/23/2026 10:00:00 AM)\t(Detached)\n1 Socket in /tmp/.screen.\n",
        );
        clean.on_key(key(KeyCode::Char('W')));
        assert_eq!(clean.mode, Mode::List);
        assert!(
            clean
                .status
                .as_deref()
                .unwrap_or_default()
                .contains("nothing to wipe")
        );
    }

    #[test]
    fn validate_action_rechecks_with_fresh_enumeration() {
        let mut app = app_with(FOUR);

        // 目标已消失 → 拒绝。
        let mut gone = confirm_action(ActionKind::Kill, "99999.gone");
        gone.display = "gone".into();
        let fresh = enumeration(
            "There is a screen on:\n\t12345.work\t(09/23/2026 10:00:00 AM)\t(Detached)\n1 Socket in /tmp/.screen.\n",
        );
        assert!(app.validate_action(fresh, &gone).is_err());

        // detach 目标已变回 detached → 拒绝（状态不匹配）。
        let mismatch = confirm_action(ActionKind::Detach, "12346.llm");
        let now_detached = enumeration(
            "There is a screen on:\n\t12346.llm\t(09/23/2026 10:01:00 AM)\t(Detached)\n1 Socket in /tmp/.screen.\n",
        );
        assert!(app.validate_action(now_detached, &mismatch).is_err());

        // detach 目标仍 attached → 通过。
        let ok = confirm_action(ActionKind::Detach, "12346.llm");
        let attached_fresh = enumeration(
            "There is a screen on:\n\t12346.llm\t(09/23/2026 10:01:00 AM)\t(Attached)\n1 Socket in /tmp/.screen.\n",
        );
        assert!(app.validate_action(attached_fresh, &ok).is_ok());

        // wipe：无 dead → 拒绝；有 dead → 通过。
        let wipe = confirm_action(ActionKind::Wipe, "");
        let no_dead = enumeration(
            "There is a screen on:\n\t12345.work\t(09/23/2026 10:00:00 AM)\t(Detached)\n1 Socket in /tmp/.screen.\n",
        );
        assert!(app.validate_action(no_dead, &wipe).is_err());
        assert!(app.validate_action(enumeration(FOUR), &wipe).is_ok());
    }

    #[test]
    fn rename_flow_edits_validates_and_produces_request() {
        let mut app = app_with(FOUR);
        app.selected = 2; // llm
        app.on_key(key(KeyCode::Char('r')));
        assert_eq!(app.mode, Mode::Rename);
        assert_eq!(app.rename.as_ref().unwrap().name, "llm");
        assert_eq!(app.rename.as_ref().unwrap().target, "12346.llm");

        // 编辑为非法名 → 报错并留在输入框。
        app.rename.as_mut().unwrap().name = "-bad".into();
        app.on_key(key(KeyCode::Enter));
        assert!(app.rename.as_ref().unwrap().error.is_some());
        assert!(app.take_rename_request().is_none());

        // 合法名 → 产出重命名请求。
        app.rename.as_mut().unwrap().name = "renamed".into();
        app.on_key(key(KeyCode::Enter));
        let request = app.take_rename_request().expect("rename request");
        assert_eq!(request.target, "12346.llm");
        assert_eq!(request.new_name, "renamed");
        assert_eq!(app.mode, Mode::List);

        // Esc 取消。
        app.on_key(key(KeyCode::Char('r')));
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::List);
        assert!(app.rename.is_none());
    }

    #[test]
    fn rename_outcome_reports_success_and_failure() {
        let mut app = app_with(FOUR);
        let request = RenameRequest {
            target: "12346.llm".into(),
            new_name: "renamed".into(),
        };
        let ok_run = cmd::Run {
            command: "screen -S 12346.llm -X sessionname renamed".into(),
            code: 0,
            stdout: String::new(),
            stderr: String::new(),
        };
        app.note_rename_outcome(&request, &ok_run);
        assert!(
            app.status
                .as_deref()
                .unwrap_or_default()
                .contains("renamed to 'renamed'")
        );

        let fail_run = cmd::Run {
            command: "screen -S 12346.llm -X sessionname renamed".into(),
            code: 1,
            stdout: String::new(),
            stderr: "no such session".into(),
        };
        app.note_rename_outcome(&request, &fail_run);
        assert!(
            app.status
                .as_deref()
                .unwrap_or_default()
                .contains("rename failed")
        );
    }

    #[test]
    fn action_outcome_reports_success_and_failure() {
        let mut app = app_with(FOUR);
        let action = confirm_action(ActionKind::Kill, "12346.llm");
        let ok_run = cmd::Run {
            command: "screen -S 12346.llm -X quit".into(),
            code: 0,
            stdout: String::new(),
            stderr: String::new(),
        };
        app.note_action_outcome(&action, &ok_run);
        assert!(
            app.status
                .as_deref()
                .unwrap_or_default()
                .contains("kill done")
        );

        let fail_run = cmd::Run {
            command: "screen -S 12346.llm -X quit".into(),
            code: 1,
            stdout: String::new(),
            stderr: "no such session".into(),
        };
        app.note_action_outcome(&action, &fail_run);
        let status = app.status.as_deref().unwrap_or_default();
        assert!(status.contains("kill"), "{status}");
        assert!(status.contains("exit 1"), "{status}");
    }

    // ------------------------------------------------------------- T2.5 过滤 / 数字直连 / 详情增强

    /// 替身枚举器：返回与 `enumeration(FOUR)` 相同的固定结果（不跑真实 screen）。
    fn fake_enumerate() -> crate::screen::Result<Enumeration> {
        Ok(enumeration(FOUR))
    }

    #[test]
    fn digits_attach_the_row_they_index() {
        let mut app = app_with(FOUR); // 排序后：dep / legacy / llm / work
        app.enumerate = fake_enumerate;

        app.on_key(key(KeyCode::Char('2'))); // 第 2 行 = legacy（dead）→ 拒连
        assert!(app.take_attach_request().is_none());

        app.on_key(key(KeyCode::Char('3'))); // 第 3 行 = llm（attached）→ 选择框
        assert_eq!(app.mode, Mode::AttachChoice);
        app.on_key(key(KeyCode::Char('2'))); // 接管
        let request = app.take_attach_request().unwrap();
        assert_eq!(request.target, "llm");

        // 越界数字（> 会话数）无事发生。
        app.on_key(key(KeyCode::Char('9')));
        assert!(app.take_attach_request().is_none());
    }

    #[test]
    fn x_shares_directly_without_the_conflict_choice() {
        let mut app = app_with(FOUR);
        app.enumerate = fake_enumerate;
        app.selected = 2; // llm（attached）
        app.on_key(key(KeyCode::Char('x')));
        let request = app.take_attach_request().expect("share request");
        assert_eq!(request.kind, AttachKind::Share);
        assert_eq!(request.target, "llm");

        // dead 会话拒绝共享。
        let mut app = app_with(FOUR);
        app.enumerate = fake_enumerate;
        app.selected = 1; // legacy（dead）
        app.on_key(key(KeyCode::Char('x')));
        assert!(app.take_attach_request().is_none());
        assert!(
            app.status
                .as_deref()
                .unwrap_or_default()
                .contains("not connectable")
        );
    }

    #[test]
    fn filter_narrows_and_survives_refresh() {
        let mut app = app_with(FOUR); // dep / legacy / llm / work
        app.enumerate = fake_enumerate;

        app.on_key(key(KeyCode::Char('/')));
        assert_eq!(app.mode, Mode::Filter);
        app.on_key(key(KeyCode::Char('l')));
        app.on_key(key(KeyCode::Char('l')));
        // 输入即筛：只剩 llm。
        assert_eq!(app.sessions().len(), 1);
        assert_eq!(app.sessions()[0].name, "llm");
        assert_eq!(app.mode, Mode::Filter);

        // 回列表：查询词仍生效（过滤不因离开输入态而重置）。
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.mode, Mode::List);
        assert_eq!(app.sessions().len(), 1);

        // refresh 不重置过滤（FR-19 验收 2 延伸）。
        app.refresh();
        assert_eq!(app.filter, "ll");
        assert_eq!(app.sessions().len(), 1);

        // Esc 清空并回全量。
        app.on_key(key(KeyCode::Char('/')));
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.filter, "");
        assert_eq!(app.sessions().len(), 4);
    }

    #[test]
    fn filter_matches_pid_and_case_insensitively() {
        let mut app = app_with(FOUR);
        app.filter = "WORK".into();
        let visible = app.sessions();
        let names: Vec<&str> = visible.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["work"]);

        // 按pid：12347 是 dep。
        app.filter = "12347".into();
        let visible = app.sessions();
        let names: Vec<&str> = visible.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["dep"]);
    }

    #[test]
    fn window_count_is_parsed_from_q_output() {
        assert_eq!(parse_window_count("0$ bash\n1$* zsh\n"), 2);
        assert_eq!(parse_window_count("0$ bash\n\n  \n"), 1);
        assert_eq!(parse_window_count(""), 0);
    }

    #[test]
    fn window_count_stays_hidden_unless_query_support_is_proven() {
        // caps.query = Unknown（默认）：refresh 不得发出 -Q 查询，窗口数保持 None。
        // 这里不跑真实 screen —— 直接断言「未探测到就没有值」的不变式。
        let mut app = app_with(FOUR);
        app.refresh_meta();
        assert!(app.window_count.is_none());
        assert!(!app.caps.query.usable(), "default caps must stay Unknown");
    }

    // ------------------------------------------------------------- T2.3 预览

    #[test]
    fn preview_open_guards_capabilities_and_dead() {
        // hardcopy 能力未证实（Unknown）→ 拒绝并说明（FR-15 验收 3 的降级路径）。
        let mut app = app_with(FOUR);
        app.selected = 0;
        app.on_key(key(KeyCode::Char('p')));
        assert!(app.take_preview_request().is_none());
        assert!(
            app.status
                .as_deref()
                .unwrap_or_default()
                .contains("preview unavailable")
        );

        // dead 会话拒绝预览（2.3d）。
        let mut app = app_with(FOUR);
        app.caps.hardcopy = crate::screen::caps::Support::Yes;
        app.selected = 1; // legacy（dead）
        app.on_key(key(KeyCode::Char('p')));
        assert!(app.take_preview_request().is_none());
        assert!(
            app.status
                .as_deref()
                .unwrap_or_default()
                .contains("nothing to preview")
        );

        // detached + 能力可用 → 产出手动抓取请求。
        app.selected = 0; // dep
        app.on_key(key(KeyCode::Char('p')));
        let request = app.take_preview_request().expect("preview request");
        assert!(request.manual);
        assert_eq!(request.full, "12347.dep");
    }

    #[test]
    fn preview_outcome_lands_as_view_or_status() {
        let mut app = app_with(FOUR);
        let request = PreviewRequest {
            full: "12345.dep".into(),
            name: "dep".into(),
            manual: true,
        };

        // 成功：视图带抓取时间；手动请求进入弹层。
        app.note_preview(&request, vec!["ready".into()]);
        let view = app.preview.as_ref().expect("view stored");
        assert_eq!(view.lines, vec!["ready"]);
        assert!(!view.fetched.is_empty());
        assert_eq!(app.mode, Mode::Preview);

        // 失败（手动）：status 给原因，视图被清，不留陈旧内容。
        app.note_preview_failed(&request, "preview unavailable: boom".into());
        assert!(app.preview.is_none());
        assert!(app.status.as_deref().unwrap_or_default().contains("boom"));
    }

    #[test]
    fn wide_auto_preview_fetches_once_per_selection() {
        let mut app = app_with(FOUR);
        app.caps.hardcopy = crate::screen::caps::Support::Yes;
        app.selected = 0;

        // 非 wide 不自动抓。
        assert!(app.wide_preview_due(false).is_none());

        // wide + 选中变化 → 一次自动请求。
        let request = app.wide_preview_due(true).expect("auto fetch");
        assert!(!request.manual);
        // 同一会话不重复抓。
        assert!(app.wide_preview_due(true).is_none());
        // 会话消失时宽屏请求自然为 None（空列表）。
        let mut empty = App::new(Caps::default());
        empty.caps.hardcopy = crate::screen::caps::Support::Yes;
        assert!(empty.wide_preview_due(true).is_none());

        // 自动失败：不写 status（不刷屏），只清目标让下轮重试。
        app.note_preview_failed(&request, "boom".into());
        assert!(app.status.is_none());
        assert!(
            app.wide_preview_due(true).is_some(),
            "cleared target retries"
        );
    }

    // ------------------------------------------------------------- T2.6 元数据持久化

    fn app_with_config_dir(tag: &str) -> (App, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("stui-t26-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = app_with(FOUR);
        app.config_path_override = Some(dir.join("config.json"));
        (app, dir)
    }

    #[test]
    fn managed_and_seen_records_land_in_config() {
        let (mut app, dir) = app_with_config_dir("records");

        // 创建 = managed（可重启）。
        app.record_managed("newtask", "/srv/app", "claude");
        let entry = app.config.sessions.get("newtask").unwrap();
        assert!(entry.managed);
        assert_eq!(entry.command.as_deref(), Some("claude"));
        assert_eq!(entry.cwd.as_deref(), Some("/srv/app"));
        assert!(entry.last_seen.is_some());

        // 连接过的外部会话 = unmanaged 观察记录。
        app.record_seen("dep");
        let entry = app.config.sessions.get("dep").unwrap();
        assert!(!entry.managed, "external sessions stay unmanaged");

        // 落盘到注入路径（证明持久化真的发生）。
        assert!(app.save_config());
        let saved: crate::config::Config =
            serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).unwrap())
                .unwrap();
        assert!(saved.sessions.get("newtask").unwrap().managed);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restart_is_restricted_to_dead_managed_sessions() {
        let (mut app, dir) = app_with_config_dir("restart");

        // detached 会话：无需重启。
        app.selected = 0; // dep（detached）
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.take_restart_request().is_none());
        assert!(
            app.status
                .as_deref()
                .unwrap_or_default()
                .contains("still running")
        );

        // dead 但没有元数据 → 明确拒绝。
        app.selected = 1; // legacy（dead）
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.take_restart_request().is_none());
        assert!(
            app.status
                .as_deref()
                .unwrap_or_default()
                .contains("not created by stui")
        );

        // dead + unmanaged 记录 → 拒绝（FR-24 验收 1）。
        app.record_seen("legacy");
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.take_restart_request().is_none());
        assert!(
            app.status
                .as_deref()
                .unwrap_or_default()
                .contains("unmanaged")
        );

        // dead + managed 记录 → 产出重启请求（记录的 command+cwd）。
        app.record_managed("legacy", "/srv/legacy", "bash -l");
        app.on_key(key(KeyCode::Char('s')));
        let request = app.take_restart_request().expect("restart request");
        assert_eq!(request.name, "legacy");
        assert_eq!(request.command, "bash -l");
        assert_eq!(request.dir, std::path::PathBuf::from("/srv/legacy"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_removes_only_stale_metadata_after_confirmation() {
        let (mut app, dir) = app_with_config_dir("cleanup");
        app.record_managed("gone-task", "/srv/gone", "top");
        app.record_managed("legacy", "/srv/legacy", "bash -l"); // legacy 还在列表里

        // 没有陈旧项时不弹确认框。
        let (mut clean_app, clean_dir) = app_with_config_dir("cleanup-none");
        clean_app.on_key(key(KeyCode::Char('X')));
        assert_eq!(clean_app.mode, Mode::List);
        assert!(
            clean_app
                .status
                .as_deref()
                .unwrap_or_default()
                .contains("no stale")
        );
        let _ = std::fs::remove_dir_all(&clean_dir);

        // 有陈旧项：确认框（默认焦点取消）→ y 确认 → 只删陈旧项。
        app.on_key(key(KeyCode::Char('X')));
        assert_eq!(app.mode, Mode::Confirm);
        let confirm = app.confirm.as_ref().unwrap();
        assert_eq!(confirm.kind, ActionKind::Cleanup);
        assert!(!confirm.focus_yes);

        app.on_key(key(KeyCode::Char('y')));
        assert_eq!(app.mode, Mode::List);
        assert!(!app.config.sessions.contains_key("gone-task"));
        assert!(
            app.config.sessions.contains_key("legacy"),
            "live entry kept"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn alias_edit_updates_metadata_and_survives_escalation() {
        let (mut app, dir) = app_with_config_dir("alias");
        app.selected = 0; // dep

        // 详情弹层里 a → 别名编辑。
        app.on_key(key(KeyCode::Char('i')));
        assert_eq!(app.mode, Mode::Detail);
        app.on_key(key(KeyCode::Char('a')));
        assert_eq!(app.mode, Mode::MetaEdit);
        app.on_key(key(KeyCode::Char('x')));
        app.on_key(key(KeyCode::Enter));

        let entry = app.config.sessions.get("dep").unwrap();
        assert_eq!(entry.alias.as_deref(), Some("x"));
        assert!(dir.join("config.json").exists(), "config persisted");

        // t → 描述；空值 = 清除。
        app.on_key(key(KeyCode::Char('i')));
        app.on_key(key(KeyCode::Char('t')));
        app.on_key(key(KeyCode::Char('y')));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(
            app.config.sessions.get("dep").unwrap().note.as_deref(),
            Some("y")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn screenrc_escape_parser_covers_both_forms() {
        // 单 token 形态：escape ^Aa
        assert_eq!(
            parse_screenrc_escape("escape ^Aa\n").as_deref(),
            Some("Ctrl-A")
        );
        // 两 token 形态：escape x x（字面前缀）
        assert_eq!(parse_screenrc_escape("escape x x\n").as_deref(), Some("x"));
        // 带注释与缩进的行。
        assert_eq!(
            parse_screenrc_escape("  escape ^]]   # my prefix\n").as_deref(),
            Some("Ctrl-]")
        );
        // 非 escape 行不干扰。
        assert_eq!(parse_screenrc_escape("term xterm-256color\n"), None);
        assert_eq!(parse_screenrc_escape(""), None);
        // 未改前缀的 ^Aa 返回 Ctrl-A，与默认一致 —— 提示文本不变。
        assert_eq!(
            parse_screenrc_escape("escape ^Aa"),
            parse_screenrc_escape("# nothing\nescape ^Aa")
        );
    }

    /// M1 出口自查：40 列目标尺寸下「看 → 选 → 进 → 出」纯键盘全流程 +
    /// FR-03 返回契约（子进程退出必回列表，q 才退出）。
    /// 渲染层的 40 列覆盖见 ui::list / ui::layout 的 TestBackend 断言。
    #[test]
    fn m1_exit_criterion_full_walk() {
        let mut app = app_with(FOUR); // 排序后：dep / legacy / llm / work

        // 看 → 选：j/k 移到 attached 的 llm。
        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.sessions()[app.selected].name, "llm");

        // 进：Enter 的重校验（替身注入新鲜枚举）→ attached → 选择框 → 2 接管。
        app.plan_connect(enumeration(FOUR));
        assert_eq!(app.mode, Mode::AttachChoice);
        app.on_key(key(KeyCode::Char('2')));
        let request = app.take_attach_request().expect("attach request");
        assert_eq!(request.kind, AttachKind::Takeover);
        assert_eq!(request.target, "llm");

        // 出：子进程退出（任意退出码）→ 必回列表，TUI 不退出（FR-03 验收 2）。
        app.note_attach_outcome(
            &request,
            &crate::screen::cmd::Run {
                command: "screen -U -d -r llm".into(),
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
        );
        assert_eq!(app.mode, Mode::List);
        assert!(!app.should_quit);

        // 唯有 q 退出。
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.should_quit);
    }
}
