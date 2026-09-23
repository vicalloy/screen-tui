//! 渲染层与终端守护（tech-design §2 / §3.1）。
//!
//! 职责边界：`ui` 只读 [`crate::app::App`] 并绘制，不得改变它；
//! 终端状态（raw mode / alternate screen / 光标）的还原由 [`TuiGuard`] 的
//! RAII 兜底，另配 panic hook 与信号 handler 三重保障（tech-design §10 风险 4）。

use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::cursor::{Hide, Show};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::{App, Mode};
use crate::ui::layout::Tier;

pub mod detail;
pub mod layout;
pub mod list;
pub mod new;
pub mod theme;

/// 信号 handler 置位的退出请求。事件循环每轮检查，走正常退出路径还原终端。
pub static SHUTDOWN: AtomicBool = AtomicBool::new(false);

pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

extern "C" fn request_shutdown(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// 注册 SIGINT / SIGTERM / SIGHUP：只置原子标志（async-signal-safe），
/// 实际的终端还原交给事件循环的正常退出路径 —— handler 里绝不碰终端。
///
/// Ctrl-C 在 raw mode 下不会产生 SIGINT（ISIG 已关），而是作为普通按键到达，
/// 因此这里只兜「外部 `kill`」路径。
pub fn install_signal_handlers() {
    unsafe {
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::signal(sig, request_shutdown as libc::sighandler_t);
        }
    }
}

/// panic 时先还原终端再走默认 hook —— 保证崩溃信息可读、终端不报废。
/// 与 [`TuiGuard::restore`] 一样幂等。
pub fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        TuiGuard::restore();
        default_hook(info);
    }));
}

/// 一次性安装全部 hook（panic + 信号）。
pub fn install_hooks() {
    install_panic_hook();
    install_signal_handlers();
}

/// RAII 终端守护：`enter()` 进入 raw mode + 备用屏幕并隐藏光标，
/// `Drop`（含 unwind 中的 panic 路径）无条件还原。
///
/// `suspend()`/`resume()` 供连接闭环（T1.5）使用：把终端还给 screen 前台，
/// detach 后再拿回来。所有还原动作幂等，重复调用无害。
pub struct TuiGuard {
    /// `true` 表示「我们当前持有终端的 raw/alternate 状态」。
    /// suspend 成功后置 `false`（终端已还，Drop 不再重复还原）。
    active: bool,
}

impl TuiGuard {
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        // 不启用键盘增强协议（kitty protocol）—— 手机 SSH 客户端兼容优先（tech-design §2.2 要点 2）。
        execute!(out, EnterAlternateScreen)?;
        execute!(out, Hide)?;
        Ok(Self { active: true })
    }

    /// 幂等还原：退出 raw mode、离开备用屏幕、显示光标。任何一步失败都继续其余步骤。
    pub fn restore() {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
    }

    /// 让出终端（连接 screen 前调用）。
    ///
    /// 注意：此处**不清除** [`SHUTDOWN`] —— 信号标志只在事件循环里检查，
    /// suspend 期间循环已停；连接结束后由调用方显式清除（T1.5）。
    pub fn suspend(&mut self) -> io::Result<()> {
        let result = (|| -> io::Result<()> {
            disable_raw_mode()?;
            execute!(io::stdout(), LeaveAlternateScreen, Show)?;
            Ok(())
        })();
        // 全部步骤成功才解除 RAII 责任；中途失败则保持 active，让 Drop 重试兜底
        // （还原动作幂等，重试无害）。
        if result.is_ok() {
            self.active = false;
        }
        result
    }

    /// 拿回终端（detach 后调用）。
    pub fn resume(&mut self) -> io::Result<()> {
        let result = (|| -> io::Result<()> {
            enable_raw_mode()?;
            execute!(io::stdout(), EnterAlternateScreen, Hide)?;
            Ok(())
        })();
        if result.is_ok() {
            self.active = true;
        }
        result
    }
}

impl Drop for TuiGuard {
    fn drop(&mut self) {
        if self.active {
            Self::restore();
            self.active = false;
        }
    }
}

/// 总绘制入口。只读 `app`，任何绘制路径不得引入副作用（tech-design §2.1）。
pub fn render(f: &mut Frame<'_>, app: &App) {
    match app.mode {
        Mode::List => render_list_screen(f, app),
        Mode::Help => render_help_overlay(f, app),
        Mode::Detail => render_detail_overlay(f, app),
        Mode::NewSession => {
            render_list_screen(f, app);
            if let Some(draft) = &app.draft {
                new::render_overlay(f, draft);
            }
        }
        Mode::AttachChoice => {
            render_list_screen(f, app);
            if let Some(choice) = &app.attach {
                render_attach_choice(f, choice);
            }
        }
    }
}

/// attached 冲突选择框（1.5b）：1 共享 / 2 接管 / Esc 取消。
fn render_attach_choice(f: &mut Frame<'_>, choice: &crate::app::AttachChoice) {
    let mut lines = vec![
        Line::from(Span::styled(
            format!(" '{}' is attached", choice.name),
            Style::default().add_modifier(ratatui::style::Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                " 1 ",
                Style::default().add_modifier(ratatui::style::Modifier::BOLD),
            ),
            Span::raw("share (-x)"),
        ]),
        Line::from(vec![
            Span::styled(
                " 2 ",
                Style::default().add_modifier(ratatui::style::Modifier::BOLD),
            ),
            Span::raw("takeover (-d -r)"),
        ]),
        Line::from(""),
    ];
    if let Some(note) = &choice.note {
        lines.push(Line::from(Span::styled(
            note.clone(),
            Style::default().fg(ratatui::style::Color::Yellow),
        )));
    }
    lines.push(Line::from(Span::styled(
        " 1/2 choose · Esc cancel".to_string(),
        theme::dimmed(),
    )));

    let height = lines.len() as u16 + 2;
    let area = centered_rect(f.area(), 44, height);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Attached ")),
        area,
    );
}

/// 主列表屏：页眉 1 行 / 正文（档位驱动）/ 页脚（档位决定行数）。
fn render_list_screen(f: &mut Frame<'_>, app: &App) {
    let screen = f.area();
    // 档位只按整屏尺寸算一次，正文/页脚共用 —— body 少 2–3 行不能拿来判 Tiny。
    let tier = Tier::from_size(screen.width, screen.height);
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(tier.footer_rows()),
    ])
    .areas(screen);

    f.render_widget(header_widget(app), header);

    match tier {
        // 宽屏：左列表 + 右详情（常驻，随选中更新）。
        Tier::Wide => {
            let [left, right] =
                Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                    .areas(body);
            list::render(f, app, left, tier);
            detail::render_panel(f, app, right);
        }
        // 中屏：单栏列表 + 底部详情区（跟随选中）。
        Tier::Mid => {
            let [list_area, detail_area] =
                Layout::vertical([Constraint::Min(0), Constraint::Length(7)]).areas(body);
            list::render(f, app, list_area, tier);
            detail::render_panel(f, app, detail_area);
        }
        // 窄屏 / 极小屏：单栏列表；详情按 `i` 弹层。
        Tier::Narrow | Tier::Tiny => list::render(f, app, body, tier),
    }

    render_footer(f, app, tier, footer);
}

/// 页脚：有瞬态消息时优先显示（刷新失败 / 动作结果），否则按档位给按键提示。
fn render_footer(f: &mut Frame<'_>, app: &App, tier: Tier, area: ratatui::layout::Rect) {
    if let Some(status) = &app.status {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                status.clone(),
                Style::default().fg(ratatui::style::Color::Red),
            ))),
            area,
        );
        return;
    }

    let has_dead = app
        .sessions()
        .iter()
        .any(|s| matches!(s.status, crate::screen::parse::Status::Dead));

    let rows: Vec<Line<'static>> = match tier {
        // 宽屏：2 行完整快捷键（FR-04）。
        Tier::Wide => {
            let mut first = vec![Span::styled(
                " j/k move  i detail  R refresh  ? help  q quit".to_string(),
                theme::dimmed(),
            )];
            if has_dead {
                // `W` 在 M1 只给「未实现」回执，T2.4 落地（FR-01 验收 2）。
                first.push(Span::styled("  W wipe dead", theme::dimmed()));
            }
            let mut second = vec![Span::styled(
                format!(" refresh every {}s", app.refresh_interval.as_secs()),
                theme::dimmed(),
            )];
            if let Some(dir) = app.socket_dir() {
                second.push(Span::styled(format!(" · socket {dir}"), theme::dimmed()));
            }
            vec![Line::from(first), Line::from(second)]
        }
        // 中屏 / 窄屏：1 行精简。
        Tier::Mid | Tier::Narrow => {
            let mut spans = vec![Span::styled(
                " j/k move  n new  i detail  ? help  q quit".to_string(),
                theme::dimmed(),
            )];
            if has_dead && tier == Tier::Mid {
                spans.push(Span::styled("  W wipe", theme::dimmed()));
            }
            vec![Line::from(spans)]
        }
        // 极小屏：只留退出与帮助入口（帮助折叠为 ? 弹层，FR-04/05）。
        Tier::Tiny => vec![Line::from(vec![Span::styled(
            " ? help  q quit".to_string(),
            theme::dimmed(),
        )])],
    };

    f.render_widget(Paragraph::new(rows), area);
}

/// 页眉：工具名 + 会话统计 + screen 版本（数据缺失就少说，不编造）。
fn header_widget(app: &App) -> Paragraph<'static> {
    let sessions = app.sessions();
    let attached = sessions
        .iter()
        .filter(|s| {
            matches!(
                s.status,
                crate::screen::parse::Status::Attached | crate::screen::parse::Status::Multi
            )
        })
        .count();
    let dead = sessions
        .iter()
        .filter(|s| matches!(s.status, crate::screen::parse::Status::Dead))
        .count();

    let mut line = vec![
        Span::styled(
            " stui".to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  {} session(s) · {attached} attached",
            sessions.len()
        )),
    ];
    if dead > 0 {
        line.push(Span::styled(
            format!(" · {dead} dead"),
            Style::default().fg(ratatui::style::Color::Red),
        ));
    }
    if let Some(version) = &app.caps.version_text {
        line.push(Span::styled(
            format!("  ·  screen {version}"),
            theme::dimmed(),
        ));
    }
    Paragraph::new(Line::from(line))
}

/// `?` 帮助弹层：居中模态，`Esc` / `q` / `?` 关闭。
fn render_help_overlay(f: &mut Frame<'_>, app: &App) {
    // 先画底层列表，再叠弹层 —— 视觉上有上下文。
    render_list_screen(f, app);

    let area = centered_rect(f.area(), 46, 9);
    let lines = vec![
        Line::from(vec![help_key("j/k / ↑/↓"), help_desc(" move selection")]),
        Line::from(vec![
            help_key("i"),
            help_desc("        detail of selection"),
        ]),
        Line::from(vec![help_key("R"), help_desc("        refresh now")]),
        Line::from(vec![help_key("?"), help_desc("        this help")]),
        Line::from(vec![help_key("q / Esc"), help_desc("  quit / close")]),
        Line::from(""),
        Line::from(Span::styled(
            format!(
                "screen {}",
                app.caps
                    .version_text
                    .as_deref()
                    .unwrap_or("version unknown")
            ),
            theme::dimmed(),
        )),
    ];
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Help ")),
        area,
    );
}

/// `i` 详情弹层（窄屏的主要信息入口，FR-05 验收 2）。
fn render_detail_overlay(f: &mut Frame<'_>, app: &App) {
    render_list_screen(f, app);

    let Some(session) = app.sessions().get(app.selected) else {
        return; // 无会话时列表层已给空态，弹层不画。
    };
    let lines = detail::detail_lines(session);
    let height = lines.len() as u16 + 2; // + 边框
    // 用显示宽度算盒宽：CJK 名字 chars().count() 会低估列数导致折行（NFR-06）。
    let width = lines
        .iter()
        .map(|l| crate::util::width::display_width(l) as u16)
        .max()
        .unwrap_or(20)
        + 2;
    let area = centered_rect(f.area(), width, height);
    let text: Vec<Line<'static>> = lines.into_iter().map(Line::from).collect();
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(text).block(Block::bordered().title(" Detail ")),
        area,
    );
}

fn help_key(key: &str) -> Span<'static> {
    Span::styled(
        format!(" {key:<8}"),
        Style::default().add_modifier(Modifier::BOLD),
    )
}

fn help_desc(desc: &str) -> Span<'static> {
    Span::raw(desc.to_string())
}

/// 居中矩形：内容宽 `content_width`、高 `content_height`，四周留白。窄屏自动贴边收缩。
pub(crate) fn centered_rect(
    area: ratatui::layout::Rect,
    content_width: u16,
    content_height: u16,
) -> ratatui::layout::Rect {
    let w = content_width.min(area.width);
    let h = content_height.min(area.height);
    let x = area.x + (area.width - w) / 2;
    let y = area.y + (area.height - h) / 2;
    ratatui::layout::Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

/// 后端类型别名（crossterm + stdout）。
pub type TuiTerminal = ratatui::Terminal<ratatui::backend::CrosstermBackend<Stdout>>;

/// 建立与 stdout 绑定的 ratatui 终端。
pub fn new_terminal() -> io::Result<TuiTerminal> {
    ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(io::stdout()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::caps::Caps;
    use crate::screen::parse::{self, Enumeration, Outlook};
    use crossterm::event::KeyCode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ls-46-with-date.txt"
    );

    fn app_with_fixture() -> App {
        let text = std::fs::read_to_string(FIXTURE).unwrap();
        let list = parse::parse_list_output(&text).unwrap();
        let mut app = App::new(Caps::default());
        app.caps.version_text = Some("4.06.02".into());
        app.apply_enumeration(Enumeration {
            outlook: Outlook::Available(list.len() as u32),
            list,
            list_error: None,
        });
        app
    }

    fn draw(app: &App, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        terminal
    }

    fn line_at(terminal: &Terminal<TestBackend>, y: u16) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    #[test]
    fn header_shows_counts_and_version() {
        let app = app_with_fixture();
        let terminal = draw(&app, 80, 12);
        let header = line_at(&terminal, 0);
        assert!(header.contains("stui"));
        assert!(header.contains("session(s)"));
        assert!(header.contains("attached"));
        // fixture 版本行是 4.6+，版本号应出现在页眉。
        assert!(header.contains("4.06.02"), "header: {header:?}");
    }

    #[test]
    fn footer_shows_hints_or_status_message() {
        let mut app = app_with_fixture();
        let terminal = draw(&app, 80, 12);
        let last = 11u16;
        assert!(line_at(&terminal, last).contains("q quit"));

        // 有瞬态消息时页脚整行让位给消息。
        app.status = Some("refresh failed: boom".into());
        let terminal = draw(&app, 80, 12);
        assert!(line_at(&terminal, last).contains("refresh failed: boom"));
        assert!(!line_at(&terminal, last).contains("q quit"));
    }

    #[test]
    fn help_overlay_renders_on_top_of_list() {
        let mut app = app_with_fixture();
        app.mode = Mode::Help;
        let terminal = draw(&app, 80, 12);
        let all: String = (0..12u16).map(|y| line_at(&terminal, y)).collect();
        assert!(all.contains(" Help "), "bordered title: {all:?}");
        assert!(all.contains("move selection"));
        // 底层列表仍然可见（弹层居中，四周留白露出列表）。
        assert!(all.contains("stui"));
    }

    #[test]
    fn new_session_wizard_renders_steps_input_and_error() {
        let mut app = app_with_fixture();
        app.open_new_session();

        // 第 1 步：步骤指示 + 名字输入回显。
        let terminal = draw(&app, 80, 20);
        let all: String = (0..20u16).map(|y| line_at(&terminal, y)).collect();
        assert!(all.contains(" New session "), "{all:?}");
        assert!(all.contains("[1 Name]"), "{all:?}");
        assert!(all.contains("Name: "), "{all:?}");

        // 名字步报错：错误文本出现在弹层里。
        app.draft.as_mut().unwrap().name = "-bad".into();
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.mode, Mode::NewSession);
        let terminal = draw(&app, 80, 20);
        let all: String = (0..20u16).map(|y| line_at(&terminal, y)).collect();
        assert!(
            all.contains("must not start with '-'"),
            "error is visible: {all:?}"
        );
    }

    fn key(code: ratatui::crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::empty())
    }

    /// 8 条会话的 `-ls` 输出（FR-05 验收 1 用：40×20 首屏 ≥5 条）。
    fn eight_sessions() -> Enumeration {
        let mut text = String::from("There are screens on:\n");
        for i in 0..8 {
            text.push_str(&format!(
                "\t{pid}.sess{i}\t(09/23/2026 10:0{i}:00 AM)\t(Detached)\n",
                pid = 10000 + i
            ));
        }
        text.push_str("8 Sockets in /tmp/.screen.\n");
        Enumeration {
            outlook: Outlook::Available(8),
            list: parse::parse_list_output(&text).unwrap(),
            list_error: None,
        }
    }

    #[test]
    fn tiny_screen_shows_at_least_five_sessions_on_first_screen() {
        // FR-05 验收 1：40×20 极端尺寸下首屏 ≥5 条会话（页眉页脚之外全是行）。
        let mut app = App::new(Caps::default());
        app.apply_enumeration(eight_sessions());
        let terminal = draw(&app, 40, 20);

        for idx in 0..5 {
            let line = line_at(&terminal, 1 + idx as u16); // 第 0 行是页眉
            assert!(
                line.contains(&format!("sess{idx}")),
                "row {idx} must be visible at 40x20: {line:?}"
            );
        }
        // 甚至 8 条全部可见。
        let line = line_at(&terminal, 8);
        assert!(line.contains("sess7"), "all 8 fit at 40x20");
    }

    #[test]
    fn wide_layout_has_permanent_detail_panel() {
        let app = app_with_fixture();
        let terminal = draw(&app, 120, 30);
        let all: String = (0..30u16).map(|y| line_at(&terminal, y)).collect();
        assert!(
            all.contains(" Detail "),
            "wide shows permanent detail panel"
        );
        // 详情面板跟随选中项：默认选中第一条。
        let first = &app.sessions()[0];
        assert!(all.contains(&first.name));
    }

    #[test]
    fn mid_layout_shows_detail_below_list() {
        let app = app_with_fixture();
        let terminal = draw(&app, 80, 20);
        let all: String = (0..20u16).map(|y| line_at(&terminal, y)).collect();
        assert!(all.contains(" Detail "), "mid shows detail area");
        assert!(all.contains("j/k move"), "mid footer is one line of hints");
    }

    #[test]
    fn narrow_layout_has_no_permanent_detail() {
        let app = app_with_fixture();
        let terminal = draw(&app, 40, 20);
        let all: String = (0..20u16).map(|y| line_at(&terminal, y)).collect();
        assert!(
            !all.contains(" Detail "),
            "narrow has no permanent detail panel"
        );
        assert!(all.contains("? help"), "narrow footer points at help");
    }

    #[test]
    fn detail_overlay_opens_with_i_and_follows_selection() {
        let mut app = app_with_fixture();
        app.selected = 2;
        app.mode = Mode::Detail;
        let terminal = draw(&app, 40, 20);
        let all: String = (0..20u16).map(|y| line_at(&terminal, y)).collect();
        assert!(all.contains(" Detail "), "overlay title present");
        let selected = &app.sessions()[2];
        assert!(all.contains(&selected.name), "overlay follows selection");
        // 弹层含 pid 行（该 fixture 全部有 pid）。
        assert!(all.contains("pid"));
    }

    #[test]
    fn resize_changes_layout_tier_on_next_draw() {
        let app = app_with_fixture();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| render(f, &app)).unwrap();
        assert!(
            (0..30u16)
                .map(|y| line_at(&terminal, y))
                .collect::<String>()
                .contains(" Detail "),
            "wide has detail panel"
        );

        // 缩到手机宽度：下一次 draw 自动重排，详情面板消失（FR-04 验收 1）。
        terminal.backend_mut().resize(40, 20);
        terminal.draw(|f| render(f, &app)).unwrap();
        let all: String = (0..20u16).map(|y| line_at(&terminal, y)).collect();
        assert!(!all.contains(" Detail "), "narrow drops the panel: {all:?}");
    }
}
