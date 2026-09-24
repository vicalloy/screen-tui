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
        let t = crate::i18n::t();
        let mut out = String::new();
        out.push_str(t.d_title);
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
                out.push_str(t.d_fix);
                out.push_str(fix);
                out.push('\n');
            }
        }

        let (pass, warn, fail) = self.counts();
        out.push('\n');
        out.push_str(&crate::i18n::fmt(
            t.d_summary,
            &[&pass.to_string(), &warn.to_string(), &fail.to_string()],
        ));
        out.push('\n');

        if !self.notes.is_empty() {
            out.push('\n');
            out.push_str(t.d_notes);
            out.push('\n');
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
    report.notes.push(crate::i18n::t().d_note11.to_string());

    report
}

// ---------------------------------------------------------------- 1
fn check_screen_executable(program: &screen::Result<std::path::PathBuf>) -> Check {
    let t = crate::i18n::t();
    match program {
        Ok(path) => Check::pass(t.d_name_screen, path.display().to_string()),
        Err(err) => Check::fail(t.d_name_screen, err.to_string(), t.d_screen_missing_fix),
    }
}

// ---------------------------------------------------------------- 2
fn check_version(caps: &screen::Result<Caps>) -> Check {
    let t = crate::i18n::t();
    let Ok(caps) = caps else {
        return Check::fail(t.d_name_version, t.d_version_na, t.d_install_screen);
    };

    let Some(version) = caps.version else {
        let shown = if caps.version_line.is_empty() {
            t.d_value_empty.to_string()
        } else {
            caps.version_line.clone()
        };
        return Check::warn(
            t.d_name_version,
            crate::i18n::fmt(t.d_version_unparsed, &[&shown]),
        )
        .with_fix(t.d_caps_live_fix);
    };

    // 版本号只用于展示；「功能是否降级」由实跑探测决定（tech-design §2 原则 2）。
    let detail = format!("{version} — {}", caps.version_line);
    match caps.query {
        Support::Yes => Check::pass(t.d_name_version, crate::i18n::fmt(t.d_q_ok, &[&detail])),
        Support::No => Check::warn(t.d_name_version, crate::i18n::fmt(t.d_q_no, &[&detail]))
            .with_fix(t.d_q_no_fix),
        Support::Unknown => Check::warn(
            t.d_name_version,
            crate::i18n::fmt(t.d_q_unknown, &[&detail]),
        )
        .with_fix(t.d_q_unknown_fix),
    }
}

// ---------------------------------------------------------------- 3
fn check_socket_dir(enumeration: &screen::Result<parse::Enumeration>) -> Check {
    let t = crate::i18n::t();
    let e = match enumeration {
        Err(err) => {
            return Check::fail(
                t.d_name_socket,
                crate::i18n::fmt(t.d_ls_failed, &[&err.to_string()]),
                t.d_screendir_fix,
            );
        }
        Ok(e) => e,
    };

    // 本机实测：$SCREENDIR 被导出为空值时 screen 仍能工作（回退到 $TMPDIR/.screen），
    // 但 capability 文档记录过它导致 `Cannot access ...` 的实例，故按 Warn 提示。
    if std::env::var_os("SCREENDIR").is_some_and(|v| v.is_empty()) {
        return Check::warn(t.d_name_socket, t.d_screendir_empty).with_fix(t.d_screendir_empty_fix);
    }

    let dir = e
        .list
        .socket_dir
        .clone()
        .unwrap_or_else(|| t.d_not_reported.to_string());
    Check::pass(t.d_name_socket, crate::i18n::fmt(t.d_socket_ok, &[&dir]))
}

// ---------------------------------------------------------------- 4
fn check_enumeration(enumeration: &screen::Result<parse::Enumeration>) -> Check {
    use parse::Outlook;

    let t = crate::i18n::t();
    let Ok(e) = enumeration else {
        return Check::fail(t.d_name_enum, t.d_enum_cannot, t.d_fix_screen_first);
    };

    let count = e.count();
    match (e.outlook, count) {
        (Outlook::Unconnectable, n) => Check::warn(
            t.d_name_enum,
            crate::i18n::fmt(t.d_enum_exit10, &[&n.to_string()]),
        )
        .with_fix(t.d_enum_perm_fix),
        (Outlook::NoSessions, n) if n > 0 => Check::warn(
            t.d_name_enum,
            crate::i18n::fmt(t.d_enum_exit9, &[&n.to_string()]),
        ),
        (Outlook::Available(k), n) if k as usize != n => Check::warn(
            t.d_name_enum,
            crate::i18n::fmt(t.d_enum_mismatch, &[&k.to_string(), &n.to_string()]),
        ),
        (Outlook::Inconclusive(code), n) => Check::warn(
            t.d_name_enum,
            crate::i18n::fmt(t.d_enum_code, &[&code.to_string(), &n.to_string()]),
        )
        .with_fix(t.d_enum_code_fix),
        (outlook, n) => Check::pass(
            t.d_name_enum,
            crate::i18n::fmt(t.d_enum_ok, &[&n.to_string(), &outlook.label()]),
        ),
    }
}

// ---------------------------------------------------------------- 5
fn check_terminfo() -> Check {
    let t = crate::i18n::t();
    match run_quietly("infocmp", &["screen-256color"]) {
        Some(status) if status.success() => Check::pass(t.d_name_terminfo, t.d_ti_present),
        Some(status) => Check::warn(
            t.d_name_terminfo,
            crate::i18n::fmt(t.d_ti_failed, &[&status.code().unwrap_or(-1).to_string()]),
        )
        .with_fix(t.d_ti_fix),
        None => Check::warn(t.d_name_terminfo, t.d_ti_missing).with_fix(t.d_ti_missing_fix),
    }
}

// ---------------------------------------------------------------- 6
fn check_utf8() -> Check {
    const KEYS: [&str; 3] = ["LC_ALL", "LC_CTYPE", "LANG"];
    let t = crate::i18n::t();

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
        return Check::pass(t.d_name_utf8, observed.join(" "));
    }

    if observed.is_empty() {
        return Check::warn(t.d_name_utf8, t.d_utf8_none).with_fix(t.d_utf8_none_fix);
    }

    Check::warn(
        t.d_name_utf8,
        crate::i18n::fmt(t.d_utf8_not, &[&observed.join(" ")]),
    )
    .with_fix(t.d_utf8_fix)
}

// ---------------------------------------------------------------- 7
fn check_dead_sessions(enumeration: &screen::Result<parse::Enumeration>) -> Check {
    let t = crate::i18n::t();
    let Ok(e) = enumeration else {
        return Check::fail(t.d_name_dead, t.d_cannot_enum, t.d_fix_screen_first);
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
            t.d_name_dead,
            if unreachable == 0 {
                t.d_dead_none.to_string()
            } else {
                crate::i18n::fmt(t.d_dead_unreachable, &[&unreachable.to_string()])
            },
        );
    }

    Check::warn(
        t.d_name_dead,
        crate::i18n::fmt(t.d_dead_list, &[&dead.len().to_string(), &dead.join(", ")]),
    )
    .with_fix(t.d_dead_fix)
}

// ---------------------------------------------------------------- 8
fn check_inside_screen() -> Check {
    let t = crate::i18n::t();
    match std::env::var_os("STY").filter(|v| !v.is_empty()) {
        None => Check::pass(t.d_name_nesting, t.d_not_inside),
        Some(sty) => Check::warn(
            t.d_name_nesting,
            crate::i18n::fmt(t.d_inside, &[&sty.to_string_lossy()]),
        )
        .with_fix(t.d_inside_fix),
    }
}

// ---------------------------------------------------------------- 9
fn check_terminal_size() -> Check {
    let t = crate::i18n::t();
    match crossterm::terminal::size() {
        Ok((cols, rows)) => {
            if cols < 40 || rows < 10 {
                Check::warn(
                    t.d_name_size,
                    crate::i18n::fmt(t.d_size_small, &[&cols.to_string(), &rows.to_string()]),
                )
            } else {
                Check::pass(t.d_name_size, format!("{cols}x{rows}"))
            }
        }
        Err(err) => Check::warn(
            t.d_name_size,
            crate::i18n::fmt(t.d_size_err, &[&err.to_string()]),
        ),
    }
}

// ---------------------------------------------------------------- 10
fn check_config_writable() -> Check {
    let t = crate::i18n::t();
    let Some((dir, source)) = config::config_dir() else {
        return Check::fail(t.d_name_config, t.d_config_missing, t.d_config_missing_fix);
    };

    let shown = format!("{} (from {})", dir.display(), source.label());
    match config::probe_writable(&dir) {
        Ok(()) => Check::pass(t.d_name_config, crate::i18n::fmt(t.d_config_ok, &[&shown])),
        Err(err) => Check::fail(
            t.d_name_config,
            crate::i18n::fmt(t.d_config_fail, &[&shown, &err.to_string()]),
            crate::i18n::fmt(t.d_config_perm_fix, &[&dir.display().to_string()]),
        ),
    }
}

// ---------------------------------------------------------------- 11
fn check_preview(
    caps: &screen::Result<Caps>,
    enumeration: &screen::Result<parse::Enumeration>,
) -> Check {
    let t = crate::i18n::t();
    if let Err(err) = caps {
        return Check::fail(t.d_name_preview, err.to_string(), t.d_install_screen);
    }

    let target = enumeration
        .as_ref()
        .ok()
        .and_then(|e| e.list.probe_target())
        .map(|s| s.full.clone());

    let Some(full) = target else {
        return Check::warn(t.d_name_preview, t.d_no_session_probe)
            .with_fix(t.d_no_session_probe_fix);
    };

    match caps::probe_hardcopy(&full) {
        Ok(probe) => {
            let history = match probe.history {
                Support::Yes => t.d_hist_yes,
                Support::No => t.d_hist_no,
                Support::Unknown => t.d_hist_unknown,
            };
            match probe.hardcopy {
                Support::Yes => Check::pass(
                    t.d_name_preview,
                    crate::i18n::fmt(t.d_hc_ok, &[&full, history]),
                ),
                Support::No => Check::warn(
                    t.d_name_preview,
                    crate::i18n::fmt(t.d_hc_no, &[&probe.detail]),
                ),
                Support::Unknown => Check::warn(
                    t.d_name_preview,
                    crate::i18n::fmt(t.d_hc_unknown, &[&probe.detail]),
                ),
            }
        }
        Err(err) => Check::warn(
            t.d_name_preview,
            crate::i18n::fmt(t.d_hc_probe_failed, &[&err.to_string()]),
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
