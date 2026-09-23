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

use crate::app::{App, Mode};

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
        Mode::List => render_list(f, app),
        Mode::Help => render_help(f, app),
    }
}

fn render_list(f: &mut Frame<'_>, app: &App) {
    // T1.1 骨架：正文直接铺满。页眉/页脚/图标列表在 T1.2 落成。
    let rows: Vec<String> = app
        .sessions()
        .iter()
        .enumerate()
        .map(|(idx, s)| {
            let cursor = if idx == app.selected { ">" } else { " " };
            format!("{cursor} {:2} {} ({})", idx + 1, s.name, s.status.label())
        })
        .collect();

    let text = if rows.is_empty() {
        "No screen sessions.".to_string()
    } else {
        rows.join("\n")
    };

    f.render_widget(ratatui::widgets::Paragraph::new(text), f.area());
}

fn render_help(f: &mut Frame<'_>, _app: &App) {
    // T1.1 骨架：占位弹层；正式按键表随 T1.2/T1.3 补齐。
    let text = "Keys: j/k move  R refresh  q quit  Esc/? close";
    f.render_widget(
        ratatui::widgets::Paragraph::new(text)
            .block(ratatui::widgets::Block::bordered().title(" Help ")),
        f.area(),
    );
}

/// 后端类型别名（crossterm + stdout）。
pub type TuiTerminal = ratatui::Terminal<ratatui::backend::CrosstermBackend<Stdout>>;

/// 建立与 stdout 绑定的 ratatui 终端。
pub fn new_terminal() -> io::Result<TuiTerminal> {
    ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(io::stdout()))
}
