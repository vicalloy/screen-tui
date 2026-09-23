//! 环境自检 `stui doctor`（FR-21 / T0.5）。
//!
//! 11 项检查逐项产出 `Pass | Warn | Fail` + 原因 + 可执行的修复建议
//! （检查项与修复建议沿用 `design/requirements.md` §10）。
//!
//! 两条纪律：
//!
//! 1. **不猜**：检查不出来就是 `Warn`，绝不为了让报告好看而写成 `Pass`（C-5）。
//! 2. **`Fail` 才算异常**：`Warn` 是「功能降级但可用」，不改变退出码 ——
//!    例如预览不可用（FR-15 验收 3 明确要求显示「预览不可用」而不是报错）。
//!
//! 唯一有副作用的是第 11 项：它会为一个真实会话落盘一次 `hardcopy`（0600 临时文件，
//! 读完立刻删）。报告末尾会注明这一点。

use std::process::{Command, Stdio};

use crate::config;
use crate::screen::{
    self, Status,
    caps::{self, Caps, Support},
    cmd, parse,
};
use crate::util::width::{display_width, pad_right};

/// 检查项总数，与 `requirements.md` §10 对齐。
pub const CHECK_COUNT: usize = 11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Pass,
    Warn,
    Fail,
}

impl Level {
    pub fn label(self) -> &'static str {
        match self {
            Level::Pass => "PASS",
            Level::Warn => "WARN",
            Level::Fail => "FAIL",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub level: Level,
    pub detail: String,
    pub fix: Option<String>,
}

impl Check {
    fn pass(name: &'static str, detail: impl Into<String>) -> Check {
        Check {
            name,
            level: Level::Pass,
            detail: detail.into(),
            fix: None,
        }
    }

    fn warn(name: &'static str, detail: impl Into<String>) -> Check {
        Check {
            name,
            level: Level::Warn,
            detail: detail.into(),
            fix: None,
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
        Check {
            name,
            level: Level::Fail,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }

    fn with_fix(mut self, fix: impl Into<String>) -> Check {
        if self.fix.is_none() {
            self.fix = Some(fix.into());
        }
        self
    }
}

/// 自检报告。
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub checks: Vec<Check>,
    /// 额外说明（探测留痕、退出码异常等），不参与判定。
    pub notes: Vec<String>,
}

impl Report {
    pub fn has_failure(&self) -> bool {
        self.checks.iter().any(|c| c.level == Level::Fail)
    }

    /// `0` 正常 / `2` 存在失败项（与 FR-22 的退出码约定一致）。
    pub fn exit_code(&self) -> u8 {
        if self.has_failure() { 2 } else { 0 }
    }

    pub fn counts(&self) -> (usize, usize, usize) {
        let count = |level: Level| self.checks.iter().filter(|c| c.level == level).count();
        (count(Level::Pass), count(Level::Warn), count(Level::Fail))
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("stui doctor — environment self-check\n");
        out.push('\n');

        let mut name_width = 0usize;
        for check in &self.checks {
            name_width = name_width.max(display_width(check.name));
        }

        for (idx, check) in self.checks.iter().enumerate() {
            let head = format!("{:>2}. [{}] ", idx + 1, check.level.label());
            out.push_str(&head);
            out.push_str(&pad_right(check.name, name_width + 2));
            out.push_str(&check.detail);
            out.push('\n');

            if let Some(fix) = &check.fix {
                out.push_str(&" ".repeat(display_width(&head)));
                out.push_str("fix: ");
                out.push_str(fix);
                out.push('\n');
            }
        }

        let (pass, warn, fail) = self.counts();
        out.push('\n');
        out.push_str(&format!("{pass} pass · {warn} warn · {fail} fail\n"));

        if !self.notes.is_empty() {
            out.push('\n');
            out.push_str("notes:\n");
            for note in &self.notes {
                out.push_str("  - ");
                out.push_str(note);
                out.push('\n');
            }
        }

        out
    }

    pub fn print(&self) {
        print!("{}", self.render());
    }
}

/// 跑完全部 11 项检查。
pub fn run() -> Report {
    let mut report = Report::default();

    let program = cmd::program();
    let enumeration = parse::enumerate();
    let caps = Caps::detect(
        enumeration
            .as_ref()
            .ok()
            .and_then(|e| e.list.probe_target())
            .map(|s| s.full.as_str()),
    );

    report.checks.push(check_screen_executable(&program));
    report.checks.push(check_version(&caps));
    report.checks.push(check_socket_dir(&enumeration));
    report.checks.push(check_enumeration(&enumeration));
    report.checks.push(check_terminfo());
    report.checks.push(check_utf8());
    report.checks.push(check_dead_sessions(&enumeration));
    report.checks.push(check_inside_screen());
    report.checks.push(check_terminal_size());
    report.checks.push(check_config_writable());
    report.checks.push(check_preview(&caps, &enumeration));

    if let Ok(e) = &enumeration {
        report.notes.extend(e.notes());
    }
    if let Ok(c) = &caps {
        report
            .notes
            .extend(c.probe_notes.iter().map(|n| format!("caps: {n}")));
    }
    report.notes.push(
        "check 11 is the only side-effecting one: it writes a real `-X hardcopy` to a 0600 \
         temp file and removes it immediately."
            .to_string(),
    );

    report
}

// ---------------------------------------------------------------- 1
fn check_screen_executable(program: &screen::Result<std::path::PathBuf>) -> Check {
    match program {
        Ok(path) => Check::pass("screen executable", path.display().to_string()),
        Err(err) => Check::fail(
            "screen executable",
            err.to_string(),
            "install GNU Screen: `apt install screen` (Debian/Ubuntu) | `yum install screen` (RHEL) | `brew install screen` (macOS)",
        ),
    }
}

// ---------------------------------------------------------------- 2
fn check_version(caps: &screen::Result<Caps>) -> Check {
    let Ok(caps) = caps else {
        return Check::fail(
            "version",
            "screen is not available, cannot determine the version",
            "install GNU Screen first",
        );
    };

    let Some(version) = caps.version else {
        return Check::warn(
            "version",
            format!(
                "cannot parse a version number from `screen -v` output: {}",
                if caps.version_line.is_empty() {
                    "<empty>".to_string()
                } else {
                    caps.version_line.clone()
                }
            ),
        )
        .with_fix("capability flags still come from live probes, so stui keeps working");
    };

    // 版本号只用于展示；「功能是否降级」由实跑探测决定（tech-design §2 原则 2）。
    let detail = format!("{version} — {}", caps.version_line);
    match caps.query {
        Support::Yes => Check::pass("version", format!("{detail}; `-Q` query available")),
        Support::No => Check::warn(
            "version",
            format!(
                "{detail}; `-Q` query unavailable on this build — window count/title need `-Q` \
                 and will be hidden. Core functions are unaffected."
            ),
        )
        .with_fix("no action needed; this is expected below Screen 4.6"),
        Support::Unknown => Check::warn(
            "version",
            format!("{detail}; could not verify `-Q` (no session was available to test with)"),
        )
        .with_fix("run `stui doctor` again while a session exists to get a definitive answer"),
    }
}

// ---------------------------------------------------------------- 3
fn check_socket_dir(enumeration: &screen::Result<parse::Enumeration>) -> Check {
    let e = match enumeration {
        Err(err) => {
            return Check::fail(
                "socket dir",
                format!("`screen -ls` failed: {err}"),
                "check that $SCREENDIR and $TMPDIR are unset or non-empty — an empty value breaks every screen call; also verify the socket directory permissions",
            );
        }
        Ok(e) => e,
    };

    // 本机实测：$SCREENDIR 被导出为空值时 screen 仍能工作（回退到 $TMPDIR/.screen），
    // 但 capability 文档记录过它导致 `Cannot access ...` 的实例，故按 Warn 提示。
    if std::env::var_os("SCREENDIR").is_some_and(|v| v.is_empty()) {
        return Check::warn(
            "socket dir",
            "`-ls` works, but $SCREENDIR is exported and EMPTY. It happens to be tolerated on \
             this build (screen falls back to `$TMPDIR/.screen`), yet an empty SCREENDIR has \
             caused `Cannot access ...` on other builds.",
        )
        .with_fix("unset SCREENDIR, or export it with a real path");
    }

    let dir = e
        .list
        .socket_dir
        .clone()
        .unwrap_or_else(|| "not reported by `-ls`".to_string());
    Check::pass("socket dir", format!("`-ls` reachable; socket dir {dir}"))
}

// ---------------------------------------------------------------- 4
fn check_enumeration(enumeration: &screen::Result<parse::Enumeration>) -> Check {
    use parse::Outlook;

    let Ok(e) = enumeration else {
        return Check::fail(
            "session enumeration",
            "cannot run `screen -q -ls`",
            "fix the `screen executable` check first",
        );
    };

    let count = e.count();
    match (e.outlook, count) {
        (Outlook::Unconnectable, n) => Check::warn(
            "session enumeration",
            format!(
                "`-q -ls` exit 10: sessions exist but none are connectable ({n} listed); \
                 the manual flags this as a permissions problem"
            ),
        )
        .with_fix("check ownership and permissions of the socket directory"),
        (Outlook::NoSessions, n) if n > 0 => Check::warn(
            "session enumeration",
            format!(
                "`-q -ls` said no sessions (exit 9) but `-ls` listed {n}; trusting `-ls`. \
                 The documented 9/10/11+ exit-code table is not reliable on every build."
            ),
        ),
        (Outlook::Available(k), n) if k as usize != n => Check::warn(
            "session enumeration",
            format!("`-q -ls` reported {k} session(s) but `-ls` detailed {n}; trusting `-ls`"),
        ),
        (Outlook::Inconclusive(code), n) => Check::warn(
            "session enumeration",
            format!(
                "`-q -ls` exited {code}, which is outside the documented 9/10/11+ table \
                 (measured on Screen 4.00.03: 8 with no sessions). stui falls back to parsing \
                 `-ls` text; {n} session(s) found."
            ),
        )
        .with_fix("no action needed; recorded for the version-capability matrix"),
        (outlook, n) => Check::pass(
            "session enumeration",
            format!("{n} session(s) · {}", outlook.label()),
        ),
    }
}

// ---------------------------------------------------------------- 5
fn check_terminfo() -> Check {
    match run_quietly("infocmp", &["screen-256color"]) {
        Some(status) if status.success() => Check::pass("terminfo", "`screen-256color` is present"),
        Some(status) => Check::warn(
            "terminfo",
            format!(
                "`infocmp screen-256color` failed (exit {}); stui degrades to the plain `screen` \
                 terminfo, and 256-color output inside sessions may not render",
                status.code().unwrap_or(-1)
            ),
        )
        .with_fix(
            "install the ncurses terminfo extras: `apt install ncurses-term` (Debian/Ubuntu)",
        ),
        None => Check::warn(
            "terminfo",
            "`infocmp` is not available, cannot verify terminfo",
        )
        .with_fix("install ncurses-bin (Debian/Ubuntu) if you want this check to work"),
    }
}

// ---------------------------------------------------------------- 6
fn check_utf8() -> Check {
    const KEYS: [&str; 3] = ["LC_ALL", "LC_CTYPE", "LANG"];

    let mut observed = Vec::new();
    let mut utf8 = false;
    for key in KEYS {
        if let Some(value) = std::env::var_os(key) {
            let text = value.to_string_lossy().into_owned();
            if text.to_ascii_lowercase().contains("utf-8")
                || text.to_ascii_lowercase().contains("utf8")
            {
                utf8 = true;
            }
            observed.push(format!("{key}={text}"));
        }
    }

    if utf8 {
        return Check::pass("utf-8 locale", observed.join(" "));
    }

    if observed.is_empty() {
        return Check::warn("utf-8 locale", "no locale variable is set").with_fix(
            "export LANG=<your>.UTF-8; CJK and emoji session names may otherwise misalign",
        );
    }

    Check::warn(
        "utf-8 locale",
        format!(
            "locale is not UTF-8 ({}); CJK/emoji session names may render wrong or misalign",
            observed.join(" ")
        ),
    )
    .with_fix(
        "export LANG=<your>.UTF-8 (suggested, not applied): add `defutf8 on` to ~/.screenrc — \
         stui never edits your screenrc",
    )
}

// ---------------------------------------------------------------- 7
fn check_dead_sessions(enumeration: &screen::Result<parse::Enumeration>) -> Check {
    let Ok(e) = enumeration else {
        return Check::fail(
            "dead sessions",
            "cannot enumerate sessions",
            "fix the `screen executable` check first",
        );
    };

    let dead: Vec<&str> = e
        .list
        .sessions
        .iter()
        .filter(|s| s.status == Status::Dead)
        .map(|s| s.name.as_str())
        .collect();
    let unreachable = e
        .list
        .sessions
        .iter()
        .filter(|s| s.status == Status::Unreachable)
        .count();

    if dead.is_empty() {
        return Check::pass(
            "dead sessions",
            if unreachable == 0 {
                "none".to_string()
            } else {
                format!(
                    "none dead, but {unreachable} unreachable (display only, never connectable)"
                )
            },
        );
    }

    Check::warn(
        "dead sessions",
        format!(
            "{} dead session(s): {} — they pollute the list and can be mis-clicked",
            dead.len(),
            dead.join(", ")
        ),
    )
    .with_fix("run `screen -wipe` (or press `W` in the TUI) — stui always asks for confirmation")
}

// ---------------------------------------------------------------- 8
fn check_inside_screen() -> Check {
    match std::env::var_os("STY").filter(|v| !v.is_empty()) {
        None => Check::pass("screen nesting", "not running inside a screen session ($STY unset)"),
        Some(sty) => Check::warn(
            "screen nesting",
            format!(
                "running inside a screen session (STY={}); screen refuses to attach from within \
                 itself",
                sty.to_string_lossy()
            ),
        )
        .with_fix(
            "detach first, or use shared attachment (`-x`) which is the only mode that works from inside",
        ),
    }
}

// ---------------------------------------------------------------- 9
fn check_terminal_size() -> Check {
    match crossterm::terminal::size() {
        Ok((cols, rows)) => {
            if cols < 40 || rows < 10 {
                Check::warn(
                    "terminal size",
                    format!(
                        "{cols}x{rows} — very small; the TUI would fall back to the minimal layout \
                         (only affects the TUI, not `ls` or `doctor`)"
                    ),
                )
            } else {
                Check::pass("terminal size", format!("{cols}x{rows}"))
            }
        }
        Err(err) => Check::warn(
            "terminal size",
            format!(
                "cannot determine terminal size ({err}); stdout is probably not a TTY — \
                 only the TUI needs it, `ls` and `doctor` do not"
            ),
        ),
    }
}

// ---------------------------------------------------------------- 10
fn check_config_writable() -> Check {
    let Some((dir, source)) = config::config_dir() else {
        return Check::fail(
            "config directory",
            "$HOME is unset and neither $SCREEN_TUI_HOME nor $XDG_CONFIG_HOME is usable",
            "set $HOME, or point $SCREEN_TUI_HOME at a writable directory",
        );
    };

    let shown = format!("{} (from {})", dir.display(), source.label());
    match config::probe_writable(&dir) {
        Ok(()) => Check::pass("config directory", format!("{shown} — writable, mode 0700")),
        Err(err) => Check::fail(
            "config directory",
            format!("{shown} — write probe failed: {err}"),
            format!("check ownership and permissions of {}", dir.display()),
        ),
    }
}

// ---------------------------------------------------------------- 11
fn check_preview(
    caps: &screen::Result<Caps>,
    enumeration: &screen::Result<parse::Enumeration>,
) -> Check {
    if let Err(err) = caps {
        return Check::fail(
            "preview ability",
            err.to_string(),
            "install GNU Screen first",
        );
    }

    let target = enumeration
        .as_ref()
        .ok()
        .and_then(|e| e.list.probe_target())
        .map(|s| s.full.clone());

    let Some(full) = target else {
        return Check::warn(
            "preview ability",
            "no connectable session available to test `-X hardcopy`; capability left unverified \
             (nothing was written to disk)",
        )
        .with_fix(
            "create a session (`screen -dmS probe sleep 60`) and re-run doctor to verify preview",
        );
    };

    match caps::probe_hardcopy(&full) {
        Ok(probe) => {
            let history = match probe.history {
                Support::Yes => "history buffer (-h) available",
                Support::No => "history buffer (-h) unavailable",
                Support::Unknown => "history buffer (-h) unverified",
            };
            match probe.hardcopy {
                Support::Yes => Check::pass(
                    "preview ability",
                    format!("`-X hardcopy` works on {full} ({history})"),
                ),
                Support::No => Check::warn(
                    "preview ability",
                    format!(
                        "`-X hardcopy` is unsupported on this build ({}); the preview pane will \
                         show metadata only — never stale or fabricated content",
                        probe.detail
                    ),
                ),
                Support::Unknown => Check::warn(
                    "preview ability",
                    format!(
                        "could not verify `-X hardcopy` ({}); preview degrades to metadata only",
                        probe.detail
                    ),
                ),
            }
        }
        Err(err) => Check::warn(
            "preview ability",
            format!("hardcopy probe failed: {err}; preview degrades to metadata only"),
        ),
    }
}

/// 跑一个外部命令并丢弃输出，只取退出状态。命令不存在时返回 `None`。
fn run_quietly(program: &str, args: &[&str]) -> Option<std::process::ExitStatus> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_is_driven_by_failures_only() {
        let mut report = Report::default();
        report.checks.push(Check::pass("a", "fine"));
        report.checks.push(Check::warn("b", "degraded"));
        assert_eq!(
            report.exit_code(),
            0,
            "warnings alone must not fail the build"
        );

        report.checks.push(Check::fail("c", "broken", "fix it"));
        assert_eq!(report.exit_code(), 2);
        assert_eq!(report.counts(), (1, 1, 1));
    }

    #[test]
    fn render_lists_every_check_with_fix_lines() {
        let mut report = Report::default();
        report
            .checks
            .push(Check::pass("screen executable", "/usr/bin/screen"));
        report
            .checks
            .push(Check::fail("config directory", "not writable", "chmod it"));
        report.notes.push("a note".to_string());

        let text = report.render();
        assert!(text.contains("[PASS] screen executable"));
        assert!(text.contains("[FAIL] config directory"));
        assert!(text.contains("fix: chmod it"));
        assert!(text.contains("1 pass · 0 warn · 1 fail"));
        assert!(text.contains("- a note"));
    }

    #[test]
    fn run_produces_all_eleven_checks_in_this_environment() {
        let report = run();
        assert_eq!(
            report.checks.len(),
            CHECK_COUNT,
            "names: {:?}",
            report.checks.iter().map(|c| c.name).collect::<Vec<_>>()
        );
    }
}
