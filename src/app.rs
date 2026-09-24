//! 应用状态机与主事件循环（tech-design §2.1 / §2.2）。
//!
//! `Mode` 之间只通过显式事件转换；`Esc` 统一回退上一层，`q` 在 `List` 才退出。
//! 渲染层只读 `App`；`App` 的按键处理是纯状态变更（除显式标注的动作外不碰进程环境），
//! 因此可脱离终端做单测。

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};

use crate::config::Config;
use crate::screen::caps::Caps;
use crate::screen::cmd::{self, AttachKind};
use crate::screen::parse::{self, Enumeration, SessionRecord, Status};
use crate::screen::probe;
use crate::ui;

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

/// 向导的字段焦点：Name / Directory / Command 三字段同屏（FR-02 验收 6）。
///
/// 这不是「步骤」—— 三个字段不是必须逐个通过的闸门，而是表单里可来回切换的焦点；
/// `Enter` 在任意字段都直接校验并创建，`Esc` 在任意字段都取消。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewField {
    Name,
    Dir,
    Command,
}

impl NewField {
    pub fn index(self) -> usize {
        match self {
            NewField::Name => 0,
            NewField::Dir => 1,
            NewField::Command => 2,
        }
    }

    pub fn label(self) -> &'static str {
        let t = crate::i18n::t();
        match self {
            NewField::Name => t.f_name,
            NewField::Dir => t.f_dir,
            NewField::Command => t.f_command,
        }
    }

    /// 焦点后移一位，到末字段**回绕**到首字段（FR-02 验收 6）。
    pub fn next(self) -> Self {
        match self {
            NewField::Name => NewField::Dir,
            NewField::Dir => NewField::Command,
            NewField::Command => NewField::Name,
        }
    }

    /// 焦点前移一位，到首字段**回绕**到末字段。
    pub fn prev(self) -> Self {
        match self {
            NewField::Name => NewField::Command,
            NewField::Dir => NewField::Name,
            NewField::Command => NewField::Dir,
        }
    }
}

/// 新建向导的草稿状态。`error` 是阻断性错误（表单保持打开），`note` 是非阻断提示
/// （如重名提示 —— FR-02 验收 2 允许重名创建，但提示寻址方式）。
///
/// 不派生 `Default`：`NewField` 没有合理初值，向导一律经 `open_new_session()` 显式构造。
#[derive(Debug, Clone)]
pub struct NewDraft {
    /// 当前焦点字段（FR-02 验收 6）：`Tab`/`↓`/`↑` 循环切换，编辑与错误都落在它身上。
    pub focus: NewField,
    pub name: String,
    /// 打开向导时按 cwd 推出的名字基名（FR-02 验收 1）。
    /// 提交前重定名以它为基 —— 拿已带后缀的当前名再追加会得到 `work22` 这种叠后缀。
    pub name_base: String,
    /// 用户是否改过名字。只有**没改过**时才在提交前自动换后缀；
    /// 手打出来的重名仍只给非阻断提示（验收 2，不静默改名）。
    pub name_edited: bool,
    pub dir: String,
    pub command: String,
    /// 收藏目录快照（T2.7 / FR-23）：打开向导时从配置取的最近目录（最多 9 条），
    /// 目录字段聚焦时可按 `1`–`9` 直选。
    pub recent: Vec<String>,
    pub error: Option<String>,
    pub note: Option<String>,
}

/// 会话名校验（纯函数）：空名 / 前导 `-`（会被 screen 当选项）/ 空白与控制字符 /
/// 超长即时报错；重名不在此拦 —— 见 `NewDraft::note`。
pub fn validate_name(name: &str) -> Result<(), String> {
    let t = crate::i18n::t();
    if name.is_empty() {
        return Err(t.name_empty.into());
    }
    if name.starts_with('-') {
        return Err(t.name_leading_dash.into());
    }
    if let Some(bad) = name.chars().find(|c| c.is_control() || c.is_whitespace()) {
        return Err(crate::i18n::fmt(t.name_bad_char, &[&format!("{bad:?}")]));
    }
    if name.chars().count() > NAME_MAX {
        return Err(crate::i18n::fmt(t.name_too_long, &[&NAME_MAX.to_string()]));
    }
    Ok(())
}

/// 会话名基名：目录名 → 合法名字（FR-02 验收 1）。
///
/// 清洗口径与 `validate_name` 一致，保证默认值必过校验：空白转 `-`、去掉控制字符、
/// 剥去前导 `-` 与前导 `.`、取不到名字时用 `session`、超长按字符数截断。
/// 前导 `-` 会被 screen 当选项读；前导 `.` 会让 socket 变成 `<pid>..name`。
pub fn base_name(dir: &std::path::Path) -> String {
    let raw = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .collect();
    let stripped = cleaned.trim_start_matches(['-', '.']);
    if stripped.is_empty() {
        "session".into()
    } else {
        clip_chars(stripped, NAME_MAX)
    }
}

/// 按字符数截断（`validate_name` 的长度口径是字符数，不是显示宽度）。
fn clip_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect()
}

/// 重名后缀的上限（`基名` → `基名2` … `基名9999`）。
///
/// 到达上限仍全被占用时退回基名：重名本身是允许的（screen 不做唯一性检查），
/// 寻址方式由 FR-02 验收 2 的提示交代，这里不为了凑唯一而无限循环。
const SUFFIX_MAX: u32 = 9999;

/// 取第一个空闲会话名（FR-02 验收 1）：基名空闲就是基名，否则依次试 `基名2`、`基名3`…
///
/// `taken` 的判定范围由调用方给全（活跃会话 ∪ 配置里的留档名）。
/// 加后缀前先截基名，**后缀必须完整保留** —— 否则 `…9` 之后会退化成同一个名字。
pub fn next_free_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_string();
    }
    for n in 2..=SUFFIX_MAX {
        let suffix = n.to_string();
        let stem = clip_chars(base, NAME_MAX.saturating_sub(suffix.chars().count()));
        let candidate = format!("{stem}{suffix}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    base.to_string()
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
        let t = crate::i18n::t();
        match self {
            ActionKind::Detach => t.act_detach,
            ActionKind::Kill => t.act_kill,
            ActionKind::Wipe => t.act_wipe,
            ActionKind::Cleanup => t.act_cleanup,
        }
    }

    /// 确认框的动词描述（危险操作要把后果说清楚）。
    pub fn consequence(self) -> &'static str {
        let t = crate::i18n::t();
        match self {
            ActionKind::Detach => t.consequence_detach,
            ActionKind::Kill => t.consequence_kill,
            ActionKind::Wipe => t.consequence_wipe,
            ActionKind::Cleanup => t.consequence_cleanup,
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
        let t = crate::i18n::t();
        match self {
            MetaField::Alias => t.meta_alias,
            MetaField::Note => t.meta_note,
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
    let t = crate::i18n::t();
    match prefix {
        Some(p) => crate::i18n::fmt(t.detach_hint_some, &[p]),
        None => t.detach_hint_default.into(),
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
    ///
    /// 懒获取：详情可见时才查（[`App::ensure_window_count`]），带 10s TTL 缓存 ——
    /// 详见 [`WindowCountCache`]。
    pub window_count: Option<usize>,
    /// 窗口数缓存：同一会话 10s 内不重复 `-Q`（FR-17 修订）。失败结果同样入缓存，
    /// 防止探测失败时每个渲染帧重试成风暴。
    window_count_cache: Option<WindowCountCache>,
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
    /// 会话创建器（可测性）：生产用 [`cmd::create`]，测试注入替身 —— 单测不得真的起 screen。
    create: fn(&str, &std::path::Path, &str) -> crate::screen::Result<cmd::Run>,
    /// 待事件循环消费的**动作**请求（已过确认框）。
    action_request: Option<ConfirmAction>,
    /// 待事件循环消费的重命名请求。
    rename_request: Option<RenameRequest>,
    /// 待事件循环消费的连接请求（`take_attach_request` 取走后执行前台连接）。
    attach_request: Option<AttachRequest>,
    pub should_quit: bool,
    /// 自动刷新间隔；`None` = 纯手动（默认，FR-19 修订）。
    /// 由 `$STUI_AUTO_REFRESH`（秒）开启，见 [`auto_refresh_interval`]。
    pub refresh_interval: Option<Duration>,
    last_refresh: Option<Instant>,
}

/// 窗口数缓存条目（FR-17 修订）。
#[derive(Debug, Clone)]
struct WindowCountCache {
    /// 缓存归属的会话（`-ls` 全名，换会话即失效）。
    full: String,
    /// 抓到的窗口数；`None` = 探测失败（同样缓存，避免逐帧重试）。
    count: Option<usize>,
    fetched_at: Instant,
}

/// 窗口数缓存的复用窗口：10s 内刚抓过就用缓存（FR-17 修订）。
const WINDOW_COUNT_TTL: Duration = Duration::from_secs(10);

/// `$STUI_AUTO_REFRESH`：自动刷新间隔（秒）。
///
/// 设为正整数 → 自动刷新；未设置、为 0 或非法值 → 纯手动刷新（默认）。
pub const AUTO_REFRESH_ENV: &str = "STUI_AUTO_REFRESH";

/// 读取自动刷新配置（纯函数 [`parse_auto_refresh_secs`] 的环境变量入口）。
fn auto_refresh_interval() -> Option<Duration> {
    parse_auto_refresh_secs(&std::env::var(AUTO_REFRESH_ENV).unwrap_or_default())
        .map(Duration::from_secs)
}

/// 解析 `$STUI_AUTO_REFRESH` 的值（纯函数，供测试）：正整数秒，其余一律 `None`。
fn parse_auto_refresh_secs(raw: &str) -> Option<u64> {
    raw.trim().parse::<u64>().ok().filter(|secs| *secs > 0)
}

impl App {
    pub fn new(caps: Caps) -> Self {
        Self::with_config(caps, Config::default())
    }

    /// 带配置构造（T2.1）。自动刷新默认关闭（FR-19 修订），`run()` 入口按
    /// `$STUI_AUTO_REFRESH` 打开 —— 测试路径不读环境变量，保持确定性。
    pub fn with_config(caps: Caps, config: Config) -> Self {
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
            window_count_cache: None,
            preview: None,
            last_preview_target: None,
            preview_request: None,
            restart_request: None,
            meta_edit: None,
            config_path_override: None,
            config_read_only: false,
            enumerate: parse::enumerate,
            create: cmd::create,
            action_request: None,
            rename_request: None,
            attach_request: None,
            should_quit: false,
            refresh_interval: None,
            last_refresh: None,
        }
    }

    /// 可见会话（渲染层与选中语义统一走这里；FR-16 过滤生效后的子集）。
    ///
    /// 过滤匹配（大小写不敏感的子串）：会话名 / PID 恒参与；
    /// 运行命令与工作目录在探测缓存里有就参与（进入过滤模式时一次性补齐缓存，
    /// 之后按缓存匹配 —— 不为过滤在每次 refresh 里对全部会话各跑一轮探测）。
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
                self.status = Some(crate::i18n::fmt(
                    crate::i18n::t().refresh_failed,
                    &[&err.to_string()],
                ));
            }
        }
        self.last_refresh = Some(Instant::now());
        self.refresh_meta();
    }

    /// 重新探测选中会话的元数据（T2.2）。探测失败只影响展示字段，不影响主流程。
    ///
    /// 窗口数不在这里查（FR-17 修订）：改为详情可见时的懒获取（[`App::ensure_window_count`]）。
    fn refresh_meta(&mut self) {
        self.meta = None;
        // 显示值随 refresh 作废；缓存不动作（懒获取，FR-17 修订），
        // 下一次 `ensure_window_count` 按缓存新鲜度决定是否重查。
        self.window_count = None;
        if let Some(session) = self.sessions().get(self.selected)
            && let Some(pid) = session.pid
            && let Ok(pid) = u32::try_from(pid)
        {
            self.meta_cache.invalidate(pid);
            self.meta = self.meta_cache.get(pid).cloned();
        }
    }

    /// 窗口数是否需要抓取（纯决策，不 spawn）：详情可见 + 能力可用 + 缓存
    /// 过期（10s TTL / 换了会话 / 手动刷新已作废）。返回要抓的会话全名。
    pub fn window_count_due(&self, detail_visible: bool) -> Option<String> {
        if !detail_visible || !self.caps.query.usable() {
            return None;
        }
        let visible = self.sessions();
        let session = visible.get(self.selected)?;
        let full = session.full.clone();
        if let Some(cache) = &self.window_count_cache
            && cache.full == full
            && cache.fetched_at.elapsed() < WINDOW_COUNT_TTL
        {
            return None;
        }
        Some(full)
    }

    /// 窗口数懒获取（FR-17 修订）：只在详情展示需要时才 `-Q windows`，
    /// 同一会话 10s 内复用缓存；失败结果同样入缓存，防止逐帧重试。
    ///
    /// 事件循环每轮调用；`detail_visible` 由终端尺寸档位 + 当前模式决定。
    pub fn ensure_window_count(&mut self, detail_visible: bool) {
        self.window_count = None;
        let visible = self.sessions();
        let Some(session) = visible.get(self.selected) else {
            return;
        };
        let full = session.full.clone();
        // 缓存新鲜：直接复用，不 spawn。
        if let Some(cache) = &self.window_count_cache
            && cache.full == full
            && cache.fetched_at.elapsed() < WINDOW_COUNT_TTL
        {
            self.window_count = cache.count;
            return;
        }
        if self.window_count_due(detail_visible).is_none() {
            return;
        }
        let count = cmd::run(["-S", &full, "-Q", "windows"])
            .ok()
            .map(|run| parse_window_count(&run.text()));
        self.window_count_cache = Some(WindowCountCache {
            full,
            count,
            fetched_at: Instant::now(),
        });
        self.window_count = count;
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
                        self.status = Some(crate::i18n::fmt(
                            crate::i18n::t().not_connectable_share,
                            &[&name],
                        ));
                    }
                    Some(Status::Unknown(raw)) => {
                        self.status = Some(crate::i18n::fmt(
                            crate::i18n::t().unknown_state,
                            &[&name, &raw],
                        ));
                    }
                    Some(_) => self.request_attach(AttachKind::Share, name),
                    None => {
                        self.status =
                            Some(crate::i18n::fmt(crate::i18n::t().session_gone, &[&name]));
                    }
                }
            }
            Err(err) => {
                self.status = Some(crate::i18n::fmt(
                    crate::i18n::t().verify_failed,
                    &[&err.to_string()],
                ));
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

    /// 距下次自动刷新的剩余时间；自动刷新关闭或尚未刷新过时返回 `MAX`
    /// （事件循环会把它钳到 `POLL_CAP`，纯阻塞等待）。
    pub fn next_tick_in(&self) -> Duration {
        match (self.refresh_interval, self.last_refresh) {
            (Some(interval), Some(at)) => interval.saturating_sub(at.elapsed()),
            _ => Duration::MAX,
        }
    }

    /// 是否到达自动刷新点。关闭（`refresh_interval == None`）时恒否 —— 手动模型。
    pub fn tick_due(&self) -> bool {
        self.next_tick_in().is_zero()
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
            // 手动刷新：窗口数缓存一并作废（FR-17 修订「除非手动刷新」），
            // 自动轮询计时由 refresh() 内统一更新的 last_refresh 重置。
            KeyCode::Char('R') => {
                self.window_count_cache = None;
                self.refresh();
            }
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
                self.status = Some(crate::i18n::fmt(
                    crate::i18n::t().verify_failed,
                    &[&err.to_string()],
                ));
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
                self.status = Some(crate::i18n::fmt(crate::i18n::t().session_gone, &[&name]));
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
                            note: Some(crate::i18n::t().multi_note.into()),
                        });
                        self.mode = Mode::AttachChoice;
                    }
                    // dead / unreachable 拒连（FR-03 表）。
                    Status::Dead => {
                        self.status =
                            Some(crate::i18n::fmt(crate::i18n::t().dead_wipe_hint, &[&name]));
                    }
                    Status::Unreachable => {
                        self.status = Some(crate::i18n::fmt(
                            crate::i18n::t().unreachable_hint,
                            &[&name],
                        ));
                    }
                    Status::Unknown(raw) => {
                        self.status = Some(crate::i18n::fmt(
                            crate::i18n::t().unknown_state,
                            &[&name, &raw],
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
        // 收藏目录（T2.7 / FR-23）：连接成功且缓存里已有 cwd 时记录（只读窥视，
        // 不为记录目录再跑一轮探测 —— 取不到就不记，C-5）。
        let mut cwd_to_record = None;
        if run.success()
            && let Some(session) = self.all_sessions().iter().find(|s| s.name == name)
            && let Some(pid) = session.pid
            && let Ok(pid) = u32::try_from(pid)
            && let Some(meta) = self.meta_cache.peek(pid)
            && let Some(cwd) = &meta.cwd
        {
            cwd_to_record = Some(cwd.clone());
        }
        if let Some(cwd) = cwd_to_record {
            self.config.touch_dir(&cwd);
        }
        self.save_config();
        self.status = Some(if run.success() {
            crate::i18n::fmt(crate::i18n::t().detached_from, &[&request.target])
        } else {
            crate::i18n::fmt(
                crate::i18n::t().screen_exit_code,
                &[&run.code.to_string(), request.kind.label()],
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
            self.status = Some(crate::i18n::fmt(
                crate::i18n::t().preview_not_running,
                &[&session.name],
            ));
            return;
        }
        if !self.caps.hardcopy.usable() {
            self.status = Some(crate::i18n::fmt(
                crate::i18n::t().preview_unavailable,
                &[self.caps.hardcopy.label()],
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
            self.status = Some(crate::i18n::fmt(
                crate::i18n::t().restart_still_running,
                &[&session.name],
            ));
            return;
        }
        let Some(meta) = self.config.sessions.get(&session.name).cloned() else {
            self.status = Some(crate::i18n::fmt(
                crate::i18n::t().restart_not_managed,
                &[&session.name],
            ));
            return;
        };
        if !meta.managed {
            self.status = Some(crate::i18n::fmt(
                crate::i18n::t().restart_unmanaged,
                &[&session.name],
            ));
            return;
        }
        let (Some(command), Some(cwd)) = (meta.command.clone(), meta.cwd.clone()) else {
            self.status = Some(crate::i18n::fmt(
                crate::i18n::t().restart_no_record,
                &[&session.name],
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
            self.config.touch_dir(&request.dir.display().to_string());
            self.save_config();
            self.status = Some(crate::i18n::fmt(
                crate::i18n::t().restarted,
                &[&request.name],
            ));
        } else {
            self.status = Some(crate::i18n::fmt(
                crate::i18n::t().restart_failed,
                &[&request.name, &run.code.to_string(), run.text().trim()],
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
            self.status = Some(crate::i18n::t().no_stale_metadata.into());
            return;
        }
        let count = stale.len();
        self.confirm = Some(ConfirmAction {
            kind: ActionKind::Cleanup,
            target: String::new(),
            display: crate::i18n::fmt(crate::i18n::t().stale_entries, &[&count.to_string()]),
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
            crate::i18n::fmt(crate::i18n::t().removed_stale, &[&count.to_string()])
        } else {
            crate::i18n::fmt(crate::i18n::t().removed_stale_ro, &[&count.to_string()])
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
                    crate::i18n::fmt(crate::i18n::t().meta_updated, &[&session, &field_label])
                } else {
                    crate::i18n::fmt(crate::i18n::t().meta_kept_ro, &[&session, &field_label])
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
                    self.status = Some(crate::i18n::t().no_dead_to_wipe.into());
                    return;
                }
                self.confirm = Some(ConfirmAction {
                    kind,
                    target: String::new(),
                    display: crate::i18n::t().dead_sessions_display.into(),
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
                    self.status = Some(crate::i18n::fmt(
                        crate::i18n::t().not_attached,
                        &[&session.name],
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
                    Err(crate::i18n::t().no_dead_left.into())
                } else {
                    Ok(())
                }
            }
            ActionKind::Kill => match found {
                Some(_) => Ok(()),
                None => Err(crate::i18n::fmt(
                    crate::i18n::t().gone_kill,
                    &[&action.display],
                )),
            },
            ActionKind::Detach => match found {
                Some(Status::Attached | Status::Multi) => Ok(()),
                Some(other) => Err(crate::i18n::fmt(
                    crate::i18n::t().no_longer_attached,
                    &[&action.display, &other.label()],
                )),
                None => Err(crate::i18n::fmt(
                    crate::i18n::t().gone_detach,
                    &[&action.display],
                )),
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
                ActionKind::Wipe => crate::i18n::t().wiped.to_string(),
                _ => crate::i18n::fmt(
                    crate::i18n::t().action_done,
                    &[&action.display, action.kind.label()],
                ),
            }
        } else {
            let detail = run.text();
            let detail = detail.trim();
            crate::i18n::fmt(
                crate::i18n::t().action_failed,
                &[
                    action.kind.label(),
                    &action.display,
                    &run.code.to_string(),
                    &if detail.is_empty() {
                        String::new()
                    } else {
                        format!(": {detail}")
                    },
                ],
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
            crate::i18n::fmt(crate::i18n::t().renamed_to, &[&request.new_name])
        } else {
            crate::i18n::fmt(
                crate::i18n::t().rename_failed,
                &[&run.code.to_string(), run.text().trim()],
            )
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

    /// 打开新建向导：预填当前目录、默认名（目录名 + 重名数字后缀）、`$SHELL`（FR-02 验收 1）。
    ///
    /// 取名吃的是**当前缓存列表** —— 打开向导不该为了取名先起一次 `screen`。
    /// 真有并发创建时由提交前那次重定名兜住（见 [`App::refit_default_name`]）。
    pub fn open_new_session(&mut self) {
        let dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let recent = self.recent_dirs_for_wizard();
        let taken = self.taken_names(None);
        let base = base_name(&dir);
        let name = next_free_name(&base, |candidate| taken.contains(candidate));
        self.draft = Some(NewDraft {
            focus: NewField::Name,
            name,
            name_base: base,
            name_edited: false,
            dir: dir.display().to_string(),
            command: default_command(),
            recent,
            error: None,
            note: None,
        });
        self.mode = Mode::NewSession;
    }

    /// 向导目录字段展示的收藏目录（最多 9 条 —— 聚焦时数字键 1–9 直选）。
    fn recent_dirs_for_wizard(&self) -> Vec<String> {
        self.config
            .dirs
            .iter()
            .take(9)
            .map(|d| d.path.clone())
            .collect()
    }

    /// 已占用的会话名集合（FR-02 验收 1）：活跃会话（含 dead —— dead 名仍占 socket 名）
    /// ∪ 配置里留档的会话名。
    ///
    /// 走 `all_sessions()` / 原始枚举而不是 `sessions()`：过滤是展示层的视图，
    /// 不能因为用户正在筛东西，就把被筛掉的会话名当成空闲。
    fn taken_names(&self, fresh: Option<&Enumeration>) -> HashSet<String> {
        let mut taken: HashSet<String> = self.config.sessions.keys().cloned().collect();
        match fresh {
            Some(enumeration) => {
                taken.extend(enumeration.list.sessions.iter().map(|s| s.name.clone()));
            }
            None => taken.extend(self.all_sessions().iter().map(|s| s.name.clone())),
        }
        taken
    }

    fn on_key_new(&mut self, code: KeyCode) {
        // 收藏目录数字直选（T2.7 / FR-23）：只在目录字段聚焦且该序号存在时接管 1–9，
        // 其余字段里数字就是普通字符。
        if let Some(path) = self.recent_pick(code) {
            if let Some(draft) = self.draft.as_mut() {
                draft.dir = path;
                draft.error = None;
            }
            return;
        }

        // 结构性按键先处理完：它们整体接管 `self.draft`，不和字段编辑的借用缠在一起。
        match code {
            KeyCode::Esc => return self.cancel_new(),
            KeyCode::Enter => return self.commit_new(),
            KeyCode::Tab | KeyCode::Down => return self.cycle_focus(true),
            KeyCode::BackTab | KeyCode::Up => return self.cycle_focus(false),
            _ => {}
        }

        let Some(draft) = self.draft.as_mut() else {
            // 草稿丢失（不应发生）：回到列表而不是卡死。
            self.mode = Mode::List;
            return;
        };
        match code {
            KeyCode::Backspace => {
                draft.error = None;
                if draft.focus == NewField::Name {
                    draft.name_edited = true;
                }
                let field = current_field_mut(draft);
                field.pop();
            }
            KeyCode::Char(c) if !c.is_control() => {
                draft.error = None;
                if draft.focus == NewField::Name {
                    draft.name_edited = true;
                }
                let field = current_field_mut(draft);
                field.push(c);
            }
            _ => {}
        }
    }

    /// 目录字段的数字直选：返回被选中的收藏目录；非目录字段 / 非 1–9 / 序号越界一律 `None`。
    fn recent_pick(&self, code: KeyCode) -> Option<String> {
        let KeyCode::Char(c) = code else {
            return None;
        };
        if !c.is_ascii_digit() || c == '0' {
            return None;
        }
        let draft = self.draft.as_ref()?;
        if draft.focus != NewField::Dir {
            return None;
        }
        draft.recent.get((c as u8 - b'1') as usize).cloned()
    }

    /// `Esc` 取消（FR-02 验收 6）：单表单没有「上一步」，任何字段都是直接放弃草稿回列表。
    fn cancel_new(&mut self) {
        self.draft = None;
        self.mode = Mode::List;
    }

    /// `Tab`/`↓` 后移、`↑`/Shift+`Tab` 前移，到边界循环回绕（FR-02 验收 6）。
    ///
    /// 焦点离开 Name 字段时把重名提示落到 `note` 上 —— 早看到早决定，
    /// 不用等到按 `Enter` 才知道 `<pid>.<name>` 这件事。
    fn cycle_focus(&mut self, forward: bool) {
        let Some(draft) = self.draft.as_ref() else {
            return;
        };
        let leaving_name = draft.focus == NewField::Name;
        let name = draft.name.clone();

        let note = if leaving_name {
            self.duplicate_note(&name)
        } else {
            None
        };
        let Some(draft) = self.draft.as_mut() else {
            return;
        };
        draft.focus = if forward {
            draft.focus.next()
        } else {
            draft.focus.prev()
        };
        if let Some(note) = note {
            draft.note = Some(note);
        }
    }

    /// 提交草稿并创建（FR-02 验收 4/6）：三字段一起校验，任一不过就把焦点跳到
    /// 出错字段、红字报错，表单保持打开。
    ///
    /// `Enter` 在任意字段都是这个终点 —— 默认值既然已经预填，就不该再要求用户
    /// 逐字段回车「批准」它们。
    fn commit_new(&mut self) {
        let Some(mut draft) = self.draft.take() else {
            self.mode = Mode::List;
            return;
        };

        for field in [NewField::Name, NewField::Dir, NewField::Command] {
            if let Err(err) = check_field(&draft, field) {
                draft.focus = field;
                draft.error = Some(err);
                self.draft = Some(draft);
                return;
            }
        }

        draft.name = draft.name.trim().to_string();
        draft.dir = expand_tilde(draft.dir.trim());
        // 命令留空 = 落回默认 shell（FR-02）：清空字段是合法动作，不报错。
        let command = draft.command.trim();
        draft.command = if command.is_empty() {
            default_command()
        } else {
            command.to_string()
        };

        if draft.name_edited {
            draft.note = self.duplicate_note(&draft.name);
        } else {
            // 名字没被动过：用最新列表再确认一次（NFR-08「列表不可信，动作前重验」）。
            let refit = self.refit_default_name(&draft.name_base, &draft.name);
            if refit != draft.name {
                draft.note = Some(crate::i18n::fmt(
                    crate::i18n::t().name_was_taken,
                    &[&draft.name, &refit],
                ));
                draft.name = refit;
            }
        }

        let name = draft.name.clone();
        let dir = draft.dir.clone();
        let command = draft.command.clone();
        let note = draft.note.take();

        match self.create_session(&name, &dir, &command) {
            Ok(selected) => {
                self.draft = None;
                self.mode = Mode::List;
                self.selected = selected;
                // 先刷新再落账：refresh() 成功时会清掉瞬态消息，结果消息必须留在最后。
                self.refresh();
                self.status = Some(match note {
                    Some(hint) => {
                        crate::i18n::fmt(crate::i18n::t().created_with_hint, &[&name, &hint])
                    }
                    None => crate::i18n::fmt(crate::i18n::t().created, &[&name]),
                });
                if self.config.defaults.attach_after_create {
                    self.auto_enter_created(&name);
                }
            }
            Err(message) => {
                // 失败：草稿原样保留（焦点不动），可行动报错，不静默（FR-02 验收 5）。
                draft.error = Some(message);
                self.draft = Some(draft);
            }
        }
    }

    /// 创建后自动进入新会话（FR-02 验收 4）。
    ///
    /// 只在能确定「列表里那一个就是刚建的这一个」时才进。名字不唯一时 `-r <name>` 本身就有歧义，
    /// 而 [`unambiguous_target`] 取的是列表里先出现的那个 —— 那可能是早就存在的同名会话，
    /// 于是我们会**静默进入错误的会话**。这种情况不猜：说清楚，让用户自己选。
    fn auto_enter_created(&mut self, name: &str) {
        let matches = self.sessions().iter().filter(|s| s.name == name).count();
        let skip_reason = match matches {
            0 => Some(crate::i18n::t().skip_not_listed),
            1 => None,
            _ => Some(crate::i18n::t().skip_not_unique),
        };
        match skip_reason {
            // 用刚刷新过的那份列表交给连接闭环重校验，不再额外枚举一次。
            None => {
                if let Some(fresh) = self.enumeration.clone() {
                    self.plan_connect(fresh);
                }
            }
            Some(reason) => self.note_auto_enter_skipped(name, reason),
        }
    }

    /// 自动进入被跳过时，把原因追加到「已创建」这条消息后面 —— 创建本身成功，不覆盖这个事实。
    fn note_auto_enter_skipped(&mut self, name: &str, reason: &str) {
        let t = crate::i18n::t();
        let created = crate::i18n::fmt(t.created, &[name]);
        let base = self
            .status
            .take()
            .filter(|status| status.starts_with(&created))
            .unwrap_or(created);
        self.status = Some(crate::i18n::fmt(t.not_entering, &[&base, reason]));
    }

    /// 提交前确认默认名（FR-02 验收 1）。
    ///
    /// 只在**名字没被改过**时介入，且只在当前这个名字已被占用时才换 ——
    /// 名字仍然空闲就保留用户看到的那一个，不让「屏幕上显示 work2、创建出来 work」发生。
    /// 拿不到新鲜列表就退回当前名字：宁可多给一次重名提示，也不阻断创建。
    fn refit_default_name(&self, base: &str, current: &str) -> String {
        let Ok(fresh) = (self.enumerate)() else {
            return current.to_string();
        };
        let taken = self.taken_names(Some(&fresh));
        if !taken.contains(current) {
            return current.to_string();
        }
        next_free_name(base, |candidate| taken.contains(candidate))
    }

    /// 重名提示（非阻断）：FR-02 验收 2 —— 允许创建，提示「将以 `<pid>.<name>` 寻址」。
    fn duplicate_note(&self, name: &str) -> Option<String> {
        self.sessions()
            .iter()
            .any(|s| s.name == name)
            .then(|| crate::i18n::fmt(crate::i18n::t().duplicate_note, &[name]))
    }

    /// 执行创建并刷新列表。成功返回新会话在（刷新后）列表中的下标。
    ///
    /// 这里只负责「建出来 + 把选中项对准它 + 记元数据」；要不要直接进去由
    /// [`App::auto_enter_created`] 在拿到刷新后的列表之后再决定（FR-02 验收 4）。
    fn create_session(&mut self, name: &str, dir: &str, command: &str) -> Result<usize, String> {
        let t = crate::i18n::t();
        let path = std::path::PathBuf::from(dir);
        let run = (self.create)(name, &path, command)
            .map_err(|err| crate::i18n::fmt(t.create_failed, &[&err.to_string()]))?;

        if !run.success() {
            let detail = run.text();
            let detail = detail.trim();
            return Err(crate::i18n::fmt(
                t.screen_refused,
                &[
                    name,
                    &run.code.to_string(),
                    &run.command,
                    dir,
                    &if detail.is_empty() {
                        t.screen_no_diagnostic.to_string()
                    } else {
                        detail.to_string()
                    },
                ],
            ));
        }

        // 创建成功：记录 managed 元数据 + 收藏目录（FR-24 / FR-23）。
        self.record_managed(name, dir, command);
        self.config.touch_dir(dir);
        self.save_config();

        // 创建成功后立刻重枚举，把选中项对准新会话（FR-02 验收 5）。
        match (self.enumerate)() {
            Ok(enumeration) => {
                let found = enumeration
                    .list
                    .sessions
                    .iter()
                    .position(|s| s.name == name);
                self.apply_enumeration(enumeration);
                // 绕过了 refresh()，这里补计时起点，避免下一轮立即重复枚举。
                self.last_refresh = Some(Instant::now());

                // 找不到就报 0（列表第 0 行）—— 调用方据此不会去连接一个没认出来的会话。
                self.selected = found.unwrap_or(0);
                Ok(self.selected)
            }
            Err(_) => Ok(0), // 列表刷新失败不回滚创建本身；下个轮询周期自会补上。
        }
    }
}

/// 单字段校验（纯函数）：返回阻断性错误，`Ok` 表示该字段可以放行。
///
/// Name 与 Directory 沿用各自的原有口径；Command **没有**阻断性错误 ——
/// 留空等于落回默认 shell（FR-02），清空字段是合法动作。
fn check_field(draft: &NewDraft, field: NewField) -> Result<(), String> {
    let t = crate::i18n::t();
    match field {
        NewField::Name => validate_name(draft.name.trim()),
        NewField::Dir => {
            let dir = expand_tilde(draft.dir.trim());
            if dir.is_empty() {
                return Err(t.dir_empty.into());
            }
            if !std::path::Path::new(&dir).is_dir() {
                return Err(crate::i18n::fmt(t.not_a_directory, &[&dir]));
            }
            Ok(())
        }
        NewField::Command => Ok(()),
    }
}

/// 当前焦点字段对应的可编辑字段。
fn current_field_mut(draft: &mut NewDraft) -> &mut String {
    match draft.focus {
        NewField::Name => &mut draft.name,
        NewField::Dir => &mut draft.dir,
        NewField::Command => &mut draft.command,
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
    // 自动刷新（FR-19 修订）：默认纯手动；`$STUI_AUTO_REFRESH=<秒>` 显式开启。
    app.refresh_interval = auto_refresh_interval();
    app.escape_prefix = match explicit_prefix {
        Some(explicit) => Some(explicit),
        None => detect_escape_prefix(),
    };
    // $STY 非空 = 已经在一个 screen 会话里（FR-03 验收 5）：警告一次，不阻塞。
    if std::env::var("STY").map(|v| !v.is_empty()).unwrap_or(false) {
        app.status = Some(crate::i18n::t().inside_sty.into());
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

        // 窗口数懒获取（FR-17 修订）：详情可见（详情弹层，或 Wide/Mid 档的常驻
        // 详情面板）时才按 10s TTL 查 `-Q windows`，其余档位不产生任何 spawn。
        let size = terminal.size()?;
        let tier = ui::layout::Tier::from_size(size.width, size.height);
        let detail_visible = app.mode == Mode::Detail
            || matches!(tier, ui::layout::Tier::Wide | ui::layout::Tier::Mid);
        app.ensure_window_count(detail_visible);

        // 连接请求：suspend → 前台 screen → resume → 强制重绘（1.5d）。
        if let Some(request) = app.take_attach_request() {
            let hint = detach_hint_text(app.escape_prefix.as_deref());
            match attach_foreground(terminal, guard, &request, &hint) {
                Ok(run) => app.note_attach_outcome(&request, &run),
                Err(err) => {
                    app.mode = Mode::List;
                    app.status = Some(crate::i18n::fmt(
                        crate::i18n::t().attach_failed,
                        &[&err.to_string()],
                    ));
                }
            }
            // 子进程画过屏幕：清掉 ratatui 的 diff 基线，强制整屏重绘。
            terminal.clear()?;
        }

        // 危险动作（T2.4）：确认框通过后，**执行前**拿新鲜枚举重校验（NFR-08）。
        if let Some(action) = app.take_action() {
            let validation = match (app.enumerate)() {
                Ok(fresh) => app.validate_action(fresh, &action),
                Err(err) => Err(crate::i18n::fmt(
                    crate::i18n::t().verify_failed_action,
                    &[action.kind.label(), &err.to_string()],
                )),
            };
            match validation {
                Ok(()) => match action.kind.to_cmd() {
                    Some(session_action) => match cmd::action(session_action, &action.target) {
                        Ok(run) => app.note_action_outcome(&action, &run),
                        Err(err) => {
                            app.refresh();
                            app.status = Some(crate::i18n::fmt(
                                crate::i18n::t().action_failed_short,
                                &[action.kind.label(), &err.to_string()],
                            ));
                        }
                    },
                    // Cleanup 在确认框内直接执行，不产动作请求（防御性兜底）。
                    None => app.status = Some(crate::i18n::t().nothing_to_do.into()),
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
                    app.status = Some(crate::i18n::fmt(
                        crate::i18n::t().rename_failed_short,
                        &[&err.to_string()],
                    ));
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
                    app.status = Some(crate::i18n::fmt(
                        crate::i18n::t().restart_failed_short,
                        &[&err.to_string()],
                    ));
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
        // 默认就装上创建替身：单测里任何一条路径都不许真的起 screen。
        app.create = fake_create;
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
        // 自动刷新默认关闭（FR-19 修订）：无论是否刷新过都不 tick，纯手动。
        assert_eq!(app.next_tick_in(), Duration::MAX);
        assert!(!app.tick_due());
        app.refresh();
        assert!(!app.tick_due());

        // `$STUI_AUTO_REFRESH` 开启后才按间隔 tick。（间隔不能取 1ms ——
        // refresh 本身可能超过 1ms，断言就永远轮不到「刚刷完」这个状态。）
        app.refresh_interval = Some(Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(50));
        assert!(app.tick_due());

        // 手动刷新把计时器重置。
        app.refresh();
        assert!(!app.tick_due());
    }

    #[test]
    fn auto_refresh_interval_parsing_follows_env_contract() {
        assert_eq!(parse_auto_refresh_secs(""), None);
        assert_eq!(parse_auto_refresh_secs("0"), None);
        assert_eq!(parse_auto_refresh_secs("abc"), None);
        assert_eq!(parse_auto_refresh_secs("-3"), None);
        assert_eq!(parse_auto_refresh_secs(" 10 "), Some(10));
        assert_eq!(parse_auto_refresh_secs("1"), Some(1));
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
    fn base_name_cleans_a_dirname_into_a_legal_name() {
        let base = |p: &str| base_name(std::path::Path::new(p));
        // 空格 → `-`（沿用原口径）。
        assert_eq!(base("/Users/x/my proj"), "my-proj");
        // 前导 `-`：目录名 `-foo` 会让 screen 把名字当选项读。
        assert_eq!(base("/tmp/-foo"), "foo");
        // 前导 `.`：隐藏目录会做出 `<pid>..config` 这种 socket 名。
        assert_eq!(base("/home/u/.config"), "config");
        // 控制字符直接去掉。
        assert_eq!(base("/tmp/a\u{7}b"), "ab");
        // 取不到目录名（根）与洗完为空 → `session`。
        assert_eq!(base("/"), "session");
        assert_eq!(base("/tmp/..."), "session");
        // 无论怎么洗，结果都必须过 `validate_name`。
        for path in [
            "/Users/x/my proj",
            "/tmp/-foo",
            "/home/u/.config",
            "/",
            "/tmp/...",
        ] {
            let name = base(path);
            assert!(validate_name(&name).is_ok(), "{path} -> {name}");
        }
    }

    #[test]
    fn next_free_name_appends_digits_until_free() {
        let taken: HashSet<String> = ["work", "work2", "work3"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // 基名空闲就是基名本身。
        assert_eq!(next_free_name("idle", |n| taken.contains(n)), "idle");
        // 占用了就往后数，不是只试一次。
        assert_eq!(next_free_name("work", |n| taken.contains(n)), "work4");
        // 判定函数看到的是完整候选名（含后缀），不是基名。
        assert_eq!(next_free_name("work2", |n| n == "work2"), "work22");
    }

    #[test]
    fn next_free_name_keeps_the_suffix_when_clipping() {
        // 基名顶到 NAME_MAX：加后缀前必须先截基名，否则名字会超长。
        let base = "a".repeat(NAME_MAX);
        let name = next_free_name(&base, |candidate| candidate == base);
        assert_eq!(name.chars().count(), NAME_MAX, "{name}");
        assert!(name.ends_with('2'), "{name}");
        assert!(validate_name(&name).is_ok(), "{name}");
    }

    #[test]
    fn next_free_name_gives_up_and_reuses_the_base() {
        // 后缀打满仍占满时退回基名，不无限循环；重名由 FR-02 验收 2 的提示兜底。
        assert_eq!(next_free_name("work", |_| true), "work");
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

    // ------------------------------------------------------------- T1.4 新建会话

    // 创建替身记录的「最后一次创建请求」。
    thread_local! {
        static CREATE_LOG: std::cell::RefCell<Option<(String, std::path::PathBuf, String)>> =
            const { std::cell::RefCell::new(None) };
    }

    /// 创建替身：不碰 screen，只记参数，回一个「成功」的退出码。
    fn fake_create(
        name: &str,
        dir: &std::path::Path,
        command: &str,
    ) -> crate::screen::Result<cmd::Run> {
        let request = (name.to_string(), dir.to_path_buf(), command.to_string());
        CREATE_LOG.with(|log| *log.borrow_mut() = Some(request));
        Ok(cmd::Run {
            command: format!("screen -U -dmS {name} {command}"),
            code: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }

    /// 创建替身：screen 明确拒绝（非零退出码 + 诊断文本）。
    fn fake_create_fails(
        _name: &str,
        _dir: &std::path::Path,
        _command: &str,
    ) -> crate::screen::Result<cmd::Run> {
        Ok(cmd::Run {
            command: "screen -U -dmS work zsh".into(),
            code: 1,
            stdout: String::new(),
            stderr: "Session name 'work' already exists".into(),
        })
    }

    /// 枚举替身：回一张只含「最后被请求创建的那个会话」的表。
    ///
    /// 这样创建后的选中 / 自动进入有真实对象可用，同时把「向导打开后名字被别人占了」
    /// 这件事也表达成「这个替身已经能看见它」—— 不需要为测试再造一条旁路。
    fn enumerate_created() -> crate::screen::Result<Enumeration> {
        let name = CREATE_LOG.with(|log| log.borrow().as_ref().map(|(name, ..)| name.clone()));
        let text = match name {
            Some(name) => format!(
                "There is a screen on:\n\t12345.{name}\t(09/23/2026 10:00:00 AM)\t(Detached)\n1 Socket in /tmp/.screen.\n"
            ),
            None => "No Sockets found in /tmp/.screen.\n".to_string(),
        };
        Ok(enumeration(&text))
    }

    /// 枚举替身：永远空表（创建成功但列表里查无此会话）。
    fn enumerate_empty() -> crate::screen::Result<Enumeration> {
        Ok(enumeration("No Sockets found in /tmp/.screen.\n"))
    }

    /// 枚举替身：回两张**同名**的表（模拟用户手打了一个已存在的名字）。
    fn enumerate_created_ambiguous() -> crate::screen::Result<Enumeration> {
        let name = CREATE_LOG
            .with(|log| log.borrow().as_ref().map(|(name, ..)| name.clone()))
            .unwrap_or_else(|| "work".into());
        Ok(enumeration(&format!(
            "There are screens on:\n\t12345.{name}\t(09/23/2026 10:00:00 AM)\t(Detached)\n\t12346.{name}\t(09/23/2026 10:01:00 AM)\t(Detached)\n2 Sockets in /tmp/.screen.\n"
        )))
    }

    /// 装上创建替身并清掉上一轮的记录 —— 测试线程会被复用，`CREATE_LOG` 必须由各用例自己清零。
    fn stub_creation(app: &mut App, enumerate: fn() -> crate::screen::Result<Enumeration>) {
        CREATE_LOG.with(|log| *log.borrow_mut() = None);
        app.create = fake_create;
        app.enumerate = enumerate;
    }

    #[test]
    fn wizard_opens_with_prefilled_defaults() {
        let mut app = app_with(FOUR);
        app.on_key(key(KeyCode::Char('n')));
        assert_eq!(app.mode, Mode::NewSession);

        // 默认名 = 当前目录名，重名才追加数字（FR-02 验收 1）。
        let base = base_name(&std::env::current_dir().unwrap());
        let expected = next_free_name(&base, |candidate| {
            app.config.sessions.contains_key(candidate)
                || app.all_sessions().iter().any(|s| s.name == candidate)
        });

        let cwd = std::env::current_dir().unwrap();
        let draft = app.draft.as_ref().expect("draft created");
        assert_eq!(draft.focus, NewField::Name);
        assert_eq!(draft.name_base, base);
        assert_eq!(draft.name, expected);
        assert!(!draft.name_edited, "预填的默认名不等于「用户改过」");
        assert!(validate_name(&draft.name).is_ok(), "{}", draft.name);
        assert_eq!(draft.dir, cwd.display().to_string());
        assert_eq!(draft.command, default_command());

        // 再按 n 不叠加草稿。
        app.on_key(key(KeyCode::Char('n')));
        assert_eq!(app.draft.as_ref().unwrap().focus, NewField::Name);
    }

    #[test]
    fn wizard_default_name_dodges_names_kept_in_config() {
        // 占用判定含配置里留档的会话名：只出现在 config 里的名字也要避开（FR-02 验收 1）。
        let base = base_name(&std::env::current_dir().unwrap());
        let mut app = app_with(FOUR);
        app.config
            .sessions
            .insert(base.clone(), crate::config::SessionMeta::default());

        app.on_key(key(KeyCode::Char('n')));
        assert_eq!(app.draft.as_ref().unwrap().name, format!("{base}2"));
        assert_eq!(app.draft.as_ref().unwrap().name_base, base);
    }

    #[test]
    fn wizard_esc_cancels_from_any_field() {
        let mut app = app_with(FOUR);
        app.open_new_session();

        // 单表单没有「上一步」：任何字段的 Esc 都是取消（FR-02 验收 6）。
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::List);
        assert!(app.draft.is_none());

        app.open_new_session();
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.draft.as_ref().unwrap().focus, NewField::Command);
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::List);
        assert!(app.draft.is_none());
    }

    #[test]
    fn wizard_input_edits_only_the_focused_field() {
        let mut app = app_with(FOUR);
        app.open_new_session();
        let original_name = app.draft.as_ref().unwrap().name.clone();

        app.on_key(key(KeyCode::Char('x')));
        assert_eq!(
            app.draft.as_ref().unwrap().name,
            format!("{original_name}x")
        );
        assert!(
            app.draft.as_ref().unwrap().name_edited,
            "改过名字要留痕（决定提交前是否换后缀）"
        );

        app.on_key(key(KeyCode::Backspace));
        assert_eq!(app.draft.as_ref().unwrap().name, original_name);

        // 焦点在目录字段时输入不会误改名字。
        app.draft.as_mut().unwrap().focus = NewField::Dir;
        app.on_key(key(KeyCode::Char('/')));
        assert!(app.draft.as_ref().unwrap().dir.ends_with('/'));
        assert_eq!(app.draft.as_ref().unwrap().name, original_name);
    }

    #[test]
    fn wizard_invalid_name_jumps_focus_back_to_name() {
        let mut app = app_with(FOUR);
        app.open_new_session();
        // 用户在目录字段按回车，但名字非法 → 焦点跳回 Name 并报错（FR-02 验收 6）。
        app.draft.as_mut().unwrap().focus = NewField::Dir;
        app.draft.as_mut().unwrap().name = "-bad".into();

        app.on_key(key(KeyCode::Enter));
        let draft = app.draft.as_ref().unwrap();
        assert_eq!(draft.focus, NewField::Name, "jumps to the offending field");
        assert!(draft.error.is_some(), "reports the reason");
    }

    #[test]
    fn wizard_tab_and_arrows_cycle_focus() {
        let mut app = app_with(FOUR);
        app.open_new_session();
        // 收藏目录在打开向导时就装好（表单没有「进入目录步」的动作了）。
        assert_eq!(
            app.draft.as_ref().unwrap().recent,
            app.recent_dirs_for_wizard()
        );

        // Tab 与 ↓ 同向循环：Name → Dir → Command → Name（回绕）。
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.draft.as_ref().unwrap().focus, NewField::Dir);
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.draft.as_ref().unwrap().focus, NewField::Command);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.draft.as_ref().unwrap().focus, NewField::Name, "回绕");

        // ↑ 与 Shift+Tab（BackTab）反向循环：Name → Command（回绕）→ Dir。
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.draft.as_ref().unwrap().focus, NewField::Command);
        app.on_key(key(KeyCode::BackTab));
        assert_eq!(app.draft.as_ref().unwrap().focus, NewField::Dir);
    }

    #[test]
    fn wizard_enter_on_name_creates_with_all_defaults() {
        let (mut app, dir) = app_with_config_dir("wizard-enter");
        stub_creation(&mut app, enumerate_created);

        app.on_key(key(KeyCode::Char('n')));
        let expected_name = app.draft.as_ref().unwrap().name.clone();
        let expected_dir = app.draft.as_ref().unwrap().dir.clone();

        app.on_key(key(KeyCode::Enter)); // 名字步一次回车 = 创建（FR-02 验收 6）

        let (name, got_dir, command) = CREATE_LOG
            .with(|log| log.borrow().clone())
            .expect("created");
        assert_eq!(name, expected_name);
        assert_eq!(got_dir.display().to_string(), expected_dir);
        assert_eq!(command, default_command(), "命令用默认值");

        assert_eq!(app.mode, Mode::List);
        assert!(app.draft.is_none());
        assert_eq!(app.selected, 0, "选中新会话");
        assert!(
            app.status
                .as_deref()
                .unwrap_or_default()
                .contains("created")
        );
        // 默认直接进入新会话（FR-02 验收 4）。
        let request = app.take_attach_request().expect("auto attach requested");
        assert_eq!(request.kind, AttachKind::Resume);
        assert_eq!(request.target, expected_name);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wizard_attach_after_create_is_configurable() {
        let (mut app, dir) = app_with_config_dir("wizard-attach");
        stub_creation(&mut app, enumerate_created);
        assert!(
            app.config.defaults.attach_after_create,
            "默认直接进入（FR-02 验收 4）"
        );

        // 关掉之后停留列表，不产生连接请求。
        app.config.defaults.attach_after_create = false;
        app.on_key(key(KeyCode::Char('n')));
        app.on_key(key(KeyCode::Enter));
        assert!(app.take_attach_request().is_none());
        assert_eq!(app.mode, Mode::List);
        assert!(
            app.status
                .as_deref()
                .unwrap_or_default()
                .contains("created")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wizard_empty_command_falls_back_to_the_default_shell() {
        let (mut app, dir) = app_with_config_dir("wizard-empty-cmd");
        stub_creation(&mut app, enumerate_created);
        app.on_key(key(KeyCode::Char('n')));
        app.draft.as_mut().unwrap().command = "   ".into();

        app.on_key(key(KeyCode::Enter));

        let (_, _, command) = CREATE_LOG
            .with(|log| log.borrow().clone())
            .expect("created");
        assert_eq!(command, default_command(), "清空命令 = 落回默认 shell");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wizard_hand_typed_name_is_not_silently_renamed() {
        let (mut app, dir) = app_with_config_dir("wizard-typed");
        stub_creation(&mut app, enumerate_created);
        app.on_key(key(KeyCode::Char('n')));

        let draft = app.draft.as_mut().unwrap();
        draft.name = "work".into(); // 与 FOUR 里的 work 撞名
        draft.name_edited = true;
        app.on_key(key(KeyCode::Enter));

        let (name, ..) = CREATE_LOG
            .with(|log| log.borrow().clone())
            .expect("created");
        assert_eq!(name, "work", "手打的名字原样提交，不擅自改名");
        assert!(
            app.status
                .as_deref()
                .unwrap_or_default()
                .contains("<pid>.work"),
            "改为提示寻址方式（FR-02 验收 2）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wizard_refits_the_default_name_if_it_got_taken() {
        let (mut app, dir) = app_with_config_dir("wizard-refit");
        stub_creation(&mut app, enumerate_created);
        app.on_key(key(KeyCode::Char('n')));
        let shown = app.draft.as_ref().unwrap().name.clone();

        // 向导开着的时候，这个名字被别处占了（列表 3 秒轮询之外的窗口）。
        CREATE_LOG.with(|log| {
            *log.borrow_mut() = Some((shown.clone(), PathBuf::from("/tmp"), "/bin/sh".into()));
        });
        app.on_key(key(KeyCode::Enter));

        let (name, ..) = CREATE_LOG
            .with(|log| log.borrow().clone())
            .expect("created");
        assert_eq!(name, format!("{shown}2"), "默认名被占则换后缀");
        assert!(
            app.status
                .as_deref()
                .unwrap_or_default()
                .contains("was taken"),
            "改名要说清原因：{:?}",
            app.status
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wizard_skips_attach_when_the_new_session_is_missing_from_the_list() {
        let (mut app, dir) = app_with_config_dir("wizard-missing");
        stub_creation(&mut app, enumerate_empty);
        app.on_key(key(KeyCode::Char('n')));

        app.on_key(key(KeyCode::Enter));

        assert_eq!(app.mode, Mode::List);
        assert!(
            app.take_attach_request().is_none(),
            "找不到新会话就不动选中项，否则会一头扎进列表第 0 行"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wizard_skips_attach_when_the_new_name_is_ambiguous() {
        let (mut app, dir) = app_with_config_dir("wizard-ambiguous");
        stub_creation(&mut app, enumerate_created_ambiguous);
        app.on_key(key(KeyCode::Char('n')));

        let draft = app.draft.as_mut().unwrap();
        draft.name = "work".into(); // 手打一个已存在的名字 → 列表里同名两条
        draft.name_edited = true;
        app.on_key(key(KeyCode::Enter));

        assert_eq!(app.mode, Mode::List, "创建本身成功，退出向导");
        assert!(
            app.take_attach_request().is_none(),
            "同名两条时 `-r work` 指哪个不确定，不能猜"
        );
        let status = app.status.as_deref().unwrap_or_default();
        assert!(status.contains("created 'work'"), "{status}");
        assert!(status.contains("not entering"), "{status}");
        assert!(status.contains("not unique"), "{status}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wizard_failed_create_keeps_the_draft_and_reports() {
        let (mut app, dir) = app_with_config_dir("wizard-create-fail");
        stub_creation(&mut app, enumerate_created);
        app.create = fake_create_fails;
        app.on_key(key(KeyCode::Char('n')));

        app.on_key(key(KeyCode::Enter));

        assert_eq!(app.mode, Mode::NewSession, "留在向导里");
        let draft = app.draft.as_ref().expect("draft kept");
        assert_eq!(draft.focus, NewField::Name, "焦点不动");
        let error = draft.error.as_deref().unwrap_or_default();
        assert!(error.contains("already exists"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_name_gets_a_note_but_is_allowed() {
        let mut app = app_with(FOUR); // 含 work / llm / dep / legacy
        app.open_new_session();
        let draft = app.draft.as_mut().unwrap();
        draft.name = "work".into();
        draft.name_edited = true; // 手打的 → 只提示，不自动改名

        app.on_key(key(KeyCode::Tab));
        let draft = app.draft.as_ref().unwrap();
        assert_eq!(draft.focus, NewField::Dir, "Tab 只是切焦点，重名不拦路");
        assert!(draft.error.is_none());
        let note = draft.note.as_deref().expect("duplicate note set");
        assert!(note.contains("<pid>.work"), "{note}");
    }

    #[test]
    fn nonexistent_dir_is_rejected_before_creation() {
        let mut app = app_with(FOUR);
        app.open_new_session();
        let draft = app.draft.as_mut().unwrap();
        draft.focus = NewField::Dir;
        draft.dir = "/no/such/dir/stui-test".into();

        app.on_key(key(KeyCode::Enter));
        let draft = app.draft.as_ref().unwrap();
        assert_eq!(draft.focus, NewField::Dir);
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
        // caps.query = Unknown（默认）：详情可见也不得发出 -Q 查询，窗口数保持 None。
        // 这里不跑真实 screen —— 直接断言「未探测到就没有值」的不变式。
        let mut app = app_with(FOUR);
        app.selected = 0;
        app.ensure_window_count(true);
        assert!(app.window_count.is_none());
        assert!(app.window_count_cache.is_none());
        assert!(app.window_count_due(true).is_none());
        assert!(!app.caps.query.usable(), "default caps must stay Unknown");
    }

    #[test]
    fn window_count_is_lazy_with_ttl_and_invalidation() {
        // 不可见（Narrow 档未开详情）→ 不产生抓取决策。
        let mut app = app_with(FOUR);
        app.selected = 0;
        app.caps.query = crate::screen::caps::Support::Yes;
        assert!(app.window_count_due(false).is_none());

        // 详情可见 → 决策给出目标会话。
        let expected_full = app.sessions()[0].full.clone();
        let full = app.window_count_due(true).expect("due for visible detail");
        assert_eq!(full, expected_full);

        // 抓取落账后：TTL 内同会话不再 due（不重复 spawn）。
        app.window_count_cache = Some(WindowCountCache {
            full: full.clone(),
            count: Some(3),
            fetched_at: Instant::now(),
        });
        assert!(app.window_count_due(true).is_none());
        app.ensure_window_count(true);
        assert_eq!(app.window_count, Some(3), "fresh cache is reused");

        // 换选中会话 → 立即 due。
        app.selected = 1;
        assert!(app.window_count_due(true).is_some());

        // 手动刷新（R 路径）作废缓存。
        app.selected = 0;
        app.window_count_cache = None;
        app.enumerate = fake_enumerate; // refresh 走替身，不跑真实 screen -ls。
        app.refresh();
        // refresh 本身不再查窗口数（懒获取），窗口数保持 None 直到下次显示时抓取。
        assert!(app.window_count.is_none());
        assert!(app.window_count_due(true).is_some(), "invalidated by manual refresh");
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

    // ------------------------------------------------------------- T2.7 收藏目录

    #[test]
    fn touch_dir_dedupes_sorts_and_caps() {
        let mut config = Config::new();
        config.max_recent_dirs = 3;

        config.touch_dir("/a");
        config.touch_dir("/b");
        config.touch_dir("/c");
        // /a 重复使用 → 提到最前。
        config.touch_dir("/a");
        let paths: Vec<&str> = config.dirs.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, vec!["/a", "/c", "/b"]);

        // 上限淘汰最旧。
        config.touch_dir("/d");
        let paths: Vec<&str> = config.dirs.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, vec!["/d", "/a", "/c"]);
    }

    #[test]
    fn wizard_dir_field_lists_recent_and_digits_pick() {
        let (mut app, dir) = app_with_config_dir("wizard");
        let real_dir = dir.display().to_string();
        // 入库顺序决定展示顺序：最近的在前。
        app.config.touch_dir(&real_dir);
        app.config.touch_dir("/nonexistent-for-test");

        app.on_key(key(KeyCode::Char('n')));
        app.on_key(key(KeyCode::Tab)); // 焦点切到目录字段
        let draft = app.draft.as_ref().unwrap();
        assert_eq!(draft.focus, NewField::Dir);
        assert_eq!(
            draft.recent,
            vec!["/nonexistent-for-test".to_string(), real_dir.clone()]
        );

        // 越界数字（> 收藏数）不消费，按普通输入追加到目录字段。
        app.on_key(key(KeyCode::Char('9')));
        assert!(app.draft.as_ref().unwrap().dir.ends_with('9'));

        // 数字 2 → 直选 real_dir 填入，焦点不动（表单没有「下一步」了）。
        app.on_key(key(KeyCode::Char('2')));
        assert_eq!(app.draft.as_ref().unwrap().focus, NewField::Dir);
        assert_eq!(app.draft.as_ref().unwrap().dir, real_dir);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn attach_records_cwd_into_recent_dirs() {
        let (mut app, dir) = app_with_config_dir("attach-dirs");
        app.enumerate = fake_enumerate; // refresh 用替身，列表不被真实 screen 清空
        let request = AttachRequest {
            kind: AttachKind::Resume,
            target: "12347.dep".into(),
        };
        // dep（12347）的 cwd 已在缓存里（此前选中时探测过）；
        // 选中别的行，避免 refresh_meta 把 12347 的缓存作废。
        app.selected = 3;
        app.meta_cache.entries_insert_for_test(
            12347,
            probe::Meta {
                cwd: Some("/srv/dep".into()),
                command: Some("top".into()),
            },
        );

        let ok_run = cmd::Run {
            command: "screen -U -r 12347.dep".into(),
            code: 0,
            stdout: String::new(),
            stderr: String::new(),
        };
        app.note_attach_outcome(&request, &ok_run);
        let paths: Vec<&str> = app.config.dirs.iter().map(|d| d.path.as_str()).collect();
        assert!(paths.contains(&"/srv/dep"), "{paths:?}");

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
