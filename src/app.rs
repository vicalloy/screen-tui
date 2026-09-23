//! 应用状态机与主事件循环（tech-design §2.1 / §2.2）。
//!
//! `Mode` 之间只通过显式事件转换；`Esc` 统一回退上一层，`q` 在 `List` 才退出。
//! 渲染层只读 `App`；`App` 的按键处理是纯状态变更（除显式标注的动作外不碰进程环境），
//! 因此可脱离终端做单测。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};

use crate::screen::caps::Caps;
use crate::screen::cmd::{self, AttachKind};
use crate::screen::parse::{self, Enumeration, SessionRecord, Status};
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

/// detach 提示（1.5c）：连接前打印，M1 假定默认前缀并注明可自定义
/// （真实 `.screenrc` 前缀探测属 FR-18 / M2）。
pub fn detach_hint() -> &'static str {
    "Tip: detach with Ctrl-A D (default prefix - use your own prefix + d if you changed it)"
}

/// attached 冲突选择框的挂起状态（1.5b）。
#[derive(Debug, Clone)]
pub struct AttachChoice {
    pub name: String,
    /// Multi 会话的尺寸风险提示（FR-03）。
    pub note: Option<String>,
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

pub struct App {
    pub caps: Caps,
    pub mode: Mode,
    /// 最近一次 `-ls` 枚举结果；首次刷新失败时为 `None`（正文给空态，不闪退）。
    pub enumeration: Option<Enumeration>,
    /// 选中行下标，永远 clamp 在 `0..=len-1`。
    pub selected: usize,
    /// 页脚瞬态消息（刷新失败、动作结果等），不弹窗打扰。
    pub status: Option<String>,
    /// 新建向导草稿；仅在 `Mode::NewSession` 期间非空。
    pub draft: Option<NewDraft>,
    /// attached 冲突选择框状态；仅在 `Mode::AttachChoice` 期间非空。
    pub attach: Option<AttachChoice>,
    /// 待事件循环消费的连接请求（`take_attach_request` 取走后执行前台连接）。
    attach_request: Option<AttachRequest>,
    pub should_quit: bool,
    pub refresh_interval: Duration,
    last_refresh: Option<Instant>,
}

impl App {
    pub fn new(caps: Caps) -> Self {
        Self {
            caps,
            mode: Mode::List,
            enumeration: None,
            selected: 0,
            status: None,
            draft: None,
            attach: None,
            attach_request: None,
            should_quit: false,
            refresh_interval: REFRESH_INTERVAL,
            last_refresh: None,
        }
    }

    /// 会话切片（渲染层专用；无枚举结果时为空）。
    pub fn sessions(&self) -> &[SessionRecord] {
        self.enumeration
            .as_ref()
            .map(|e| e.list.sessions.as_slice())
            .unwrap_or(&[])
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
        match parse::enumerate() {
            Ok(enumeration) => {
                self.apply_enumeration(enumeration);
                self.status = None;
            }
            Err(err) => {
                self.status = Some(format!("refresh failed: {err}"));
            }
        }
        self.last_refresh = Some(Instant::now());
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
            Mode::Help | Mode::Detail => self.on_key_overlay(key.code),
            Mode::NewSession => self.on_key_new(key.code),
            Mode::AttachChoice => self.on_key_attach_choice(key.code),
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
            // 详情弹层：仅在有会话时可开（无会话保持列表空态）。
            KeyCode::Char('i') => {
                if !self.sessions().is_empty() {
                    self.mode = Mode::Detail;
                }
            }
            // dead 清理（FR-01 验收 2 的提示入口）在 T2.4 落地；M1 给明确回执，不静默。
            KeyCode::Char('W') => {
                self.status = Some("session wipe is not implemented yet (planned for M2)".into());
            }
            KeyCode::Char('n') => self.open_new_session(),
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
        match parse::enumerate() {
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
        let sessions: Vec<SessionRecord> = self.sessions().to_vec();
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

    fn on_key_overlay(&mut self, code: KeyCode) {
        match code {
            // 任何界面下 q / Esc 回上一层（requirements §9）；`i` 再次按下同样关闭。
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('i') | KeyCode::Char('?') => {
                self.mode = Mode::List;
            }
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

    let mut app = App::new(caps);
    // $STY 非空 = 已经在一个 screen 会话里（FR-03 验收 5）：警告一次，不阻塞。
    if std::env::var("STY").map(|v| !v.is_empty()).unwrap_or(false) {
        app.status = Some(
            "already inside a screen session ($STY); nested connections can be confusing".into(),
        );
    }
    app.refresh();

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
            match attach_foreground(terminal, guard, &request) {
                Ok(run) => app.note_attach_outcome(&request, &run),
                Err(err) => {
                    app.mode = Mode::List;
                    app.status = Some(format!("attach failed: {err}"));
                }
            }
            // 子进程画过屏幕：清掉 ratatui 的 diff 基线，强制整屏重绘。
            terminal.clear()?;
        }
    }
    Ok(())
}

/// 前台执行连接（1.5d）：spawn 而非 exec —— exec 会替换进程，detach 后无法回到 TUI。
fn attach_foreground(
    terminal: &mut ui::TuiTerminal,
    guard: &mut ui::TuiGuard,
    request: &AttachRequest,
) -> crate::screen::Result<crate::screen::cmd::Run> {
    use std::io::Write;

    // 离开备用屏前把缓冲刷掉，然后还原终端给 screen。
    terminal.flush()?;
    guard.suspend()?;

    // 1.5c：detach 提示打印到真实终端（留在滚动缓冲里，不进 TUI 画面）。
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "{}", detach_hint());
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
    fn detach_hint_mentions_default_prefix_and_customization() {
        let hint = detach_hint();
        assert!(hint.contains("Ctrl-A D"), "{hint}");
        assert!(hint.contains("your own prefix"), "{hint}");
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
