//! 应用状态机与主事件循环（tech-design §2.1 / §2.2）。
//!
//! `Mode` 之间只通过显式事件转换；`Esc` 统一回退上一层，`q` 在 `List` 才退出。
//! 渲染层只读 `App`；`App` 的按键处理是纯状态变更（除显式标注的动作外不碰进程环境），
//! 因此可脱离终端做单测。

use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};

use crate::screen::caps::Caps;
use crate::screen::parse::{self, Enumeration, SessionRecord};
use crate::ui;

/// 刷新间隔（FR-19：默认 3 秒，介于 spv 的 1s 与 screen-manager 的 5s 之间）。
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(3);

/// 事件轮询的最大等待。同时封顶「响应外部信号」的延迟。
const POLL_CAP: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 主列表（默认轮询）。
    List,
    /// `?` 帮助弹层。
    Help,
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

    /// 按键分发。循环层已过滤非 Press 事件，这里再挡一次（双保险，tech-design §2.2 要点 1）。
    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        match self.mode {
            Mode::List => self.on_key_list(key.code),
            Mode::Help => self.on_key_help(key.code),
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
            // dead 清理（FR-01 验收 2 的提示入口）在 T2.4 落地；M1 给明确回执，不静默。
            KeyCode::Char('W') => {
                self.status = Some("session wipe is not implemented yet (planned for M2)".into());
            }
            // Enter / 数字键 / n 的连接与新建语义在 T1.4/T1.5 接线。
            _ => {}
        }
    }

    fn on_key_help(&mut self, code: KeyCode) {
        match code {
            // 任何界面下 q / Esc 回上一层（requirements §9）。
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('?') => self.mode = Mode::List,
            _ => {}
        }
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

    let guard = match ui::TuiGuard::enter() {
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
    app.refresh();

    let outcome = event_loop(&mut terminal, &mut app);

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

fn event_loop(terminal: &mut ui::TuiTerminal, app: &mut App) -> std::io::Result<()> {
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
    }
    Ok(())
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
}
