//! 界面文案 i18n（零依赖，FR-25）。
//!
//! 设计（Q&A 已确认的三个决策）：
//!
//! 1. **范围**：CLI 输出 + TUI 全部界面文案；screen 适配层的错误串与
//!    clap 的 help 文本保持英文（技术诊断信息，不在本层）。
//! 2. **切换**：`$STUI_LANG` 覆盖 > `config.json` 顶层 `language` 字段（`zh` / `en` /
//!    `auto`），`auto` 时按 `LC_ALL` > `LC_MESSAGES` > `LANG` 探测，都不含 zh/en
//!    前缀则回退英文。
//! 3. **实现**：手写翻译表 —— 一个 [`Dict`] 结构体装全部文案，
//!    `EN` / `ZH` 两个常量必须字段齐全（少一个就是编译错误），无任何第三方依赖。
//!
//! 关键机制：
//!
//! * 全局语言用 `OnceLock` 保存，`main` 入口（`cli::run`）初始化一次。
//!   **未初始化时一律英文** —— 全部既有测试的英文断言因此不需要改动，
//!   测试进程也不会受 shell 的 `LANG` 影响。
//! * 带参数的文案存 `{0}`/`{1}` 模板，经 [`fmt`] 插值。`fmt` 逐字符扫描模板、
//!   参数内容**不再扫描**，杜绝参数里出现 `{1}` 之类的二次替换。
//! * 中文文案宽度全部按 `util::width` 显示宽度对齐（CJK 占两列），
//!   列宽预算沿用英文档位，中文标签刻意取 ≤ 英文列宽的词（如 `#`、`名字`）。

use std::sync::OnceLock;

// ------------------------------------------------------------- 语言

/// 语言覆盖环境变量：设为 `zh` / `en` 等可被 [`Lang::detect`] 识别的值即生效。
pub const LANG_ENV: &str = "STUI_LANG";

/// 界面语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Zh,
}

impl Lang {
    /// 从配置值解析；`auto` / 未知值走 locale 探测。
    pub fn detect(setting: &str) -> Lang {
        match setting.trim().to_ascii_lowercase().as_str() {
            "zh" | "chinese" => Lang::Zh,
            "en" | "english" => Lang::En,
            _ => Self::from_locale(),
        }
    }

    /// locale 探测：`LC_ALL` > `LC_MESSAGES` > `LANG`，值以 zh / en 开头即定；
    /// 都不含则回退英文（宁缺毋滥，C-5）。
    fn from_locale() -> Lang {
        for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
            if let Some(value) = std::env::var_os(key) {
                let value = value.to_string_lossy().to_ascii_lowercase();
                if value.starts_with("zh") {
                    return Lang::Zh;
                }
                if value.starts_with("en") {
                    return Lang::En;
                }
            }
        }
        Lang::En
    }
}

static LANG: OnceLock<Lang> = OnceLock::new();

/// 进程入口初始化一次（`cli::run`）。重复调用静默忽略。
pub fn init(lang: Lang) {
    let _ = LANG.set(lang);
}

/// 当前语言。未初始化（含全部测试路径）一律英文 —— 既有测试断言保持稳定。
pub fn lang() -> Lang {
    *LANG.get().unwrap_or(&Lang::En)
}

/// 当前语言的文案表。
pub fn t() -> &'static Dict {
    dict(lang())
}

/// 指定语言的文案表（测试与预览用）。
pub fn dict(lang: Lang) -> &'static Dict {
    match lang {
        Lang::En => &DICT_EN,
        Lang::Zh => &DICT_ZH,
    }
}

/// 模板插值：`{0}` `{1}` … 按序替换为 `args` 对应项。
///
/// 逐字符扫描，替换进来的参数内容不会被再次扫描（无二次替换）；
/// 缺参数或空 `{}` 时占位符原样保留，绝不 panic。
pub fn fmt(template: &str, args: &[&str]) -> String {
    let mut out = String::with_capacity(template.len() + 32);
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '{' {
            out.push(c);
            continue;
        }
        let mut digits = String::new();
        while let Some(&d) = chars.peek() {
            if d.is_ascii_digit() {
                digits.push(d);
                chars.next();
            } else {
                break;
            }
        }
        match (chars.peek(), digits.parse::<usize>()) {
            (Some('}'), Ok(n)) => {
                chars.next(); // 吃掉 '}'
                match args.get(n) {
                    Some(arg) => out.push_str(arg),
                    None => {
                        out.push('{');
                        out.push_str(&digits);
                        out.push('}');
                    }
                }
            }
            _ => {
                out.push('{');
                out.push_str(&digits);
            }
        }
    }
    out
}

// ------------------------------------------------------------- 文案表

/// 全部用户可见文案。`EN` 与 `ZH` 必须字段齐全 —— 结构体字面量少字段直接编译失败。
///
/// 带参数的字段是 `{0}` 风格模板；字段名以消息语义命名，不按界面位置命名，
/// 便于同一条消息多处复用。
pub struct Dict {
    // ---- 通用
    pub note_label: &'static str,

    // ---- stui ls（cli.rs）
    pub ls_no_sessions: &'static str,
    pub ls_summary: &'static str,
    pub ls_col_idx: &'static str,
    pub ls_col_name: &'static str,
    pub ls_col_status: &'static str,
    pub ls_col_pid: &'static str,
    pub ls_col_created: &'static str,

    // ---- 会话状态 / 枚举结论（screen/parse.rs label）
    pub st_detached: &'static str,
    pub st_attached: &'static str,
    pub st_multi: &'static str,
    pub st_dead: &'static str,
    pub st_unreachable: &'static str,
    pub outlook_no_sessions: &'static str,
    pub outlook_unconnectable: &'static str,
    pub outlook_available: &'static str,
    pub outlook_inconclusive: &'static str,

    // ---- doctor（doctor.rs）
    pub d_title: &'static str,
    pub d_fix: &'static str,
    pub d_summary: &'static str,
    pub d_notes: &'static str,
    pub d_name_screen: &'static str,
    pub d_name_version: &'static str,
    pub d_name_socket: &'static str,
    pub d_name_enum: &'static str,
    pub d_name_terminfo: &'static str,
    pub d_name_utf8: &'static str,
    pub d_name_dead: &'static str,
    pub d_name_nesting: &'static str,
    pub d_name_size: &'static str,
    pub d_name_config: &'static str,
    pub d_name_preview: &'static str,
    pub d_screen_missing_fix: &'static str,
    pub d_version_na: &'static str,
    pub d_install_screen: &'static str,
    pub d_version_unparsed: &'static str,
    pub d_value_empty: &'static str,
    pub d_caps_live_fix: &'static str,
    pub d_q_ok: &'static str,
    pub d_q_no: &'static str,
    pub d_q_no_fix: &'static str,
    pub d_q_unknown: &'static str,
    pub d_q_unknown_fix: &'static str,
    pub d_ls_failed: &'static str,
    pub d_screendir_fix: &'static str,
    pub d_screendir_empty: &'static str,
    pub d_screendir_empty_fix: &'static str,
    pub d_socket_ok: &'static str,
    pub d_not_reported: &'static str,
    pub d_enum_cannot: &'static str,
    pub d_fix_screen_first: &'static str,
    pub d_enum_exit10: &'static str,
    pub d_enum_perm_fix: &'static str,
    pub d_enum_exit9: &'static str,
    pub d_enum_mismatch: &'static str,
    pub d_enum_code: &'static str,
    pub d_enum_code_fix: &'static str,
    pub d_enum_ok: &'static str,
    pub d_ti_present: &'static str,
    pub d_ti_failed: &'static str,
    pub d_ti_fix: &'static str,
    pub d_ti_missing: &'static str,
    pub d_ti_missing_fix: &'static str,
    pub d_utf8_none: &'static str,
    pub d_utf8_none_fix: &'static str,
    pub d_utf8_not: &'static str,
    pub d_utf8_fix: &'static str,
    pub d_cannot_enum: &'static str,
    pub d_dead_none: &'static str,
    pub d_dead_unreachable: &'static str,
    pub d_dead_list: &'static str,
    pub d_dead_fix: &'static str,
    pub d_not_inside: &'static str,
    pub d_inside: &'static str,
    pub d_inside_fix: &'static str,
    pub d_size_small: &'static str,
    pub d_size_err: &'static str,
    pub d_config_missing: &'static str,
    pub d_config_missing_fix: &'static str,
    pub d_config_ok: &'static str,
    pub d_config_fail: &'static str,
    pub d_config_perm_fix: &'static str,
    pub d_no_session_probe: &'static str,
    pub d_no_session_probe_fix: &'static str,
    pub d_hist_yes: &'static str,
    pub d_hist_no: &'static str,
    pub d_hist_unknown: &'static str,
    pub d_hc_ok: &'static str,
    pub d_hc_no: &'static str,
    pub d_hc_unknown: &'static str,
    pub d_hc_probe_failed: &'static str,
    pub d_note11: &'static str,

    // ---- 名字校验 / 向导（app.rs）
    pub name_empty: &'static str,
    pub name_leading_dash: &'static str,
    pub name_bad_char: &'static str,
    pub name_too_long: &'static str,
    pub f_name: &'static str,
    pub f_dir: &'static str,
    pub f_command: &'static str,
    pub dir_empty: &'static str,
    pub not_a_directory: &'static str,
    pub duplicate_note: &'static str,
    pub name_was_taken: &'static str,
    pub created: &'static str,
    pub created_with_hint: &'static str,
    pub skip_not_listed: &'static str,
    pub skip_not_unique: &'static str,
    pub not_entering: &'static str,
    pub create_failed: &'static str,
    pub screen_refused: &'static str,
    pub screen_no_diagnostic: &'static str,

    // ---- 会话动作（app.rs）
    pub act_detach: &'static str,
    pub act_kill: &'static str,
    pub act_wipe: &'static str,
    pub act_cleanup: &'static str,
    pub consequence_detach: &'static str,
    pub consequence_kill: &'static str,
    pub consequence_wipe: &'static str,
    pub consequence_cleanup: &'static str,
    pub verify_failed: &'static str,
    pub verify_failed_action: &'static str,
    pub session_gone: &'static str,
    pub unknown_state: &'static str,
    pub not_connectable_share: &'static str,
    pub dead_wipe_hint: &'static str,
    pub unreachable_hint: &'static str,
    pub not_attached: &'static str,
    pub no_dead_to_wipe: &'static str,
    pub dead_sessions_display: &'static str,
    pub no_dead_left: &'static str,
    pub gone_kill: &'static str,
    pub kill_attached: &'static str,
    pub gone_detach: &'static str,
    pub no_longer_attached: &'static str,
    pub wiped: &'static str,
    pub action_done: &'static str,
    pub action_failed: &'static str,
    pub action_failed_short: &'static str,
    pub nothing_to_do: &'static str,
    pub attach_failed: &'static str,
    pub attach_exit_code: &'static str,

    // ---- 重命名 / 元数据 / 清档 / 重启（app.rs）
    pub renamed_to: &'static str,
    pub rename_failed: &'static str,
    pub rename_failed_short: &'static str,
    pub meta_alias: &'static str,
    pub meta_note: &'static str,
    pub meta_updated: &'static str,
    pub meta_kept_ro: &'static str,
    pub no_stale_metadata: &'static str,
    pub stale_entries: &'static str,
    pub removed_stale: &'static str,
    pub removed_stale_ro: &'static str,
    pub restart_still_running: &'static str,
    pub restart_not_managed: &'static str,
    pub restart_unmanaged: &'static str,
    pub restart_no_record: &'static str,
    pub restarted: &'static str,
    pub restart_failed: &'static str,
    pub restart_failed_short: &'static str,

    // ---- 预览 / 连接提示（app.rs）
    pub preview_not_running: &'static str,
    pub preview_unavailable: &'static str,
    pub detach_hint_some: &'static str,
    pub detach_hint_default: &'static str,
    pub refresh_failed: &'static str,
    pub header_self: &'static str,
    pub attach_self: &'static str,
    pub desc_self_marker: &'static str,

    // ---- TUI 列表 / 页眉页脚（ui/list.rs、ui/mod.rs）
    pub list_empty: &'static str,
    pub list_filter_empty: &'static str,
    pub header_sessions: &'static str,
    pub header_dead: &'static str,
    pub footer_wide_1: &'static str,
    pub footer_wide_2: &'static str,
    pub footer_wipe_dead: &'static str,
    pub footer_mid: &'static str,
    pub footer_wipe: &'static str,
    pub footer_tiny: &'static str,
    pub footer_refresh: &'static str,
    pub footer_manual: &'static str,

    // ---- 错误弹层（ui/mod.rs，FR-15 修订：错误确认后关闭，不驻留页脚）
    pub error_title: &'static str,
    pub error_hint: &'static str,

    // ---- 帮助弹层（ui/mod.rs）
    pub help_title: &'static str,
    pub desc_move: &'static str,
    pub desc_attach: &'static str,
    pub desc_quick: &'static str,
    pub desc_share: &'static str,
    pub desc_preview: &'static str,
    pub desc_new: &'static str,
    pub desc_detail: &'static str,
    pub desc_filter: &'static str,
    pub desc_detach: &'static str,
    pub desc_kill: &'static str,
    pub desc_rename: &'static str,
    pub desc_wipe: &'static str,
    pub desc_refresh: &'static str,
    pub desc_quit: &'static str,
    pub version_unknown: &'static str,

    // ---- 详情 / 元数据 / 重命名 / 确认 / 过滤 / 冲突弹层（ui/*.rs）
    pub detail_title: &'static str,
    pub detail_nothing: &'static str,
    pub detail_hint: &'static str,
    pub dl_name: &'static str,
    pub dl_address: &'static str,
    pub dl_pid: &'static str,
    pub dl_status: &'static str,
    pub dl_created: &'static str,
    pub dl_windows: &'static str,
    pub dl_cwd: &'static str,
    pub dl_command: &'static str,
    pub dl_alias: &'static str,
    pub dl_note: &'static str,
    pub dl_managed: &'static str,
    pub dl_managed_yes: &'static str,
    pub meta_title: &'static str,
    pub meta_line: &'static str,
    pub meta_hint: &'static str,
    pub rename_title: &'static str,
    pub rename_new_name: &'static str,
    pub rename_hint: &'static str,
    pub confirm_title: &'static str,
    pub confirm_session: &'static str,
    pub confirm_command: &'static str,
    pub confirm_yes: &'static str,
    pub confirm_cancel: &'static str,
    pub confirm_hint: &'static str,
    pub filter_title: &'static str,
    pub filter_match: &'static str,
    pub filter_hint: &'static str,

    // ---- 预览 pane / 弹层（ui/preview.rs）
    pub preview_pane_hint: &'static str,
    pub preview_title: &'static str,
    pub preview_overlay_title: &'static str,
    pub preview_fetched: &'static str,
    pub preview_empty: &'static str,

    // ---- 新建向导（ui/new.rs）
    pub new_title: &'static str,
    pub new_hint: &'static str,
}

// ------------------------------------------------------------- 英文（默认）

const DICT_EN: Dict = Dict {
    note_label: "note",
    ls_no_sessions: "# no screen sessions found (socket dir {0})",
    ls_summary: "# {0} session(s) · socket dir {1} · {2}",
    ls_col_idx: "IDX",
    ls_col_name: "NAME",
    ls_col_status: "STATUS",
    ls_col_pid: "PID",
    ls_col_created: "CREATED",
    st_detached: "detached",
    st_attached: "attached",
    st_multi: "multi",
    st_dead: "dead",
    st_unreachable: "unreachable",
    outlook_no_sessions: "no sessions (exit 9)",
    outlook_unconnectable: "sessions exist but none connectable (exit 10)",
    outlook_available: "available (exit {0})",
    outlook_inconclusive: "inconclusive (exit {0}, outside the documented 9/10/11+ table)",
    d_title: "stui doctor — environment self-check",
    d_fix: "fix: ",
    d_summary: "{0} pass · {1} warn · {2} fail",
    d_notes: "notes:",
    d_name_screen: "screen executable",
    d_name_version: "version",
    d_name_socket: "socket dir",
    d_name_enum: "session enumeration",
    d_name_terminfo: "terminfo",
    d_name_utf8: "utf-8 locale",
    d_name_dead: "dead sessions",
    d_name_nesting: "screen nesting",
    d_name_size: "terminal size",
    d_name_config: "config directory",
    d_name_preview: "preview ability",
    d_screen_missing_fix: "install GNU Screen: `apt install screen` (Debian/Ubuntu) | `yum install screen` (RHEL) | `brew install screen` (macOS)",
    d_version_na: "screen is not available, cannot determine the version",
    d_install_screen: "install GNU Screen first",
    d_version_unparsed: "cannot parse a version number from `screen -v` output: {0}",
    d_value_empty: "<empty>",
    d_caps_live_fix: "capability flags still come from live probes, so stui keeps working",
    d_q_ok: "{0}; `-Q` query available",
    d_q_no: "{0}; `-Q` query unavailable on this build — window count/title need `-Q` and will be hidden. Core functions are unaffected.",
    d_q_no_fix: "no action needed; this is expected below Screen 4.6",
    d_q_unknown: "{0}; could not verify `-Q` (no session was available to test with)",
    d_q_unknown_fix: "run `stui doctor` again while a session exists to get a definitive answer",
    d_ls_failed: "`screen -ls` failed: {0}",
    d_screendir_fix: "check that $SCREENDIR and $TMPDIR are unset or non-empty — an empty value breaks every screen call; also verify the socket directory permissions",
    d_screendir_empty: "`-ls` works, but $SCREENDIR is exported and EMPTY. It happens to be tolerated on this build (screen falls back to `$TMPDIR/.screen`), yet an empty SCREENDIR has caused `Cannot access ...` on other builds.",
    d_screendir_empty_fix: "unset SCREENDIR, or export it with a real path",
    d_socket_ok: "`-ls` reachable; socket dir {0}",
    d_not_reported: "not reported by `-ls`",
    d_enum_cannot: "cannot run `screen -q -ls`",
    d_fix_screen_first: "fix the `screen executable` check first",
    d_enum_exit10: "`-q -ls` exit 10: sessions exist but none are connectable ({0} listed); the manual flags this as a permissions problem",
    d_enum_perm_fix: "check ownership and permissions of the socket directory",
    d_enum_exit9: "`-q -ls` said no sessions (exit 9) but `-ls` listed {0}; trusting `-ls`. The documented 9/10/11+ exit-code table is not reliable on every build.",
    d_enum_mismatch: "`-q -ls` reported {0} session(s) but `-ls` detailed {1}; trusting `-ls`",
    d_enum_code: "`-q -ls` exited {0}, which is outside the documented 9/10/11+ table (measured on Screen 4.00.03: 8 with no sessions). stui falls back to parsing `-ls` text; {1} session(s) found.",
    d_enum_code_fix: "no action needed; recorded for the version-capability matrix",
    d_enum_ok: "{0} session(s) · {1}",
    d_ti_present: "`screen-256color` is present",
    d_ti_failed: "`infocmp screen-256color` failed (exit {0}); stui degrades to the plain `screen` terminfo, and 256-color output inside sessions may not render",
    d_ti_fix: "install the ncurses terminfo extras: `apt install ncurses-term` (Debian/Ubuntu)",
    d_ti_missing: "`infocmp` is not available, cannot verify terminfo",
    d_ti_missing_fix: "install ncurses-bin (Debian/Ubuntu) if you want this check to work",
    d_utf8_none: "no locale variable is set",
    d_utf8_none_fix: "export LANG=<your>.UTF-8; CJK and emoji session names may otherwise misalign",
    d_utf8_not: "locale is not UTF-8 ({0}); CJK/emoji session names may render wrong or misalign",
    d_utf8_fix: "export LANG=<your>.UTF-8 (suggested, not applied): add `defutf8 on` to ~/.screenrc — stui never edits your screenrc",
    d_cannot_enum: "cannot enumerate sessions",
    d_dead_none: "none",
    d_dead_unreachable: "none dead, but {0} unreachable (display only, never connectable)",
    d_dead_list: "{0} dead session(s): {1} — they pollute the list and can be mis-clicked",
    d_dead_fix: "run `screen -wipe` (or press `W` in the TUI) — stui always asks for confirmation",
    d_not_inside: "not running inside a screen session ($STY unset)",
    d_inside: "running inside a screen session (STY={0}); screen refuses to attach from within itself",
    d_inside_fix: "detach first, or use shared attachment (`-x`) which is the only mode that works from inside",
    d_size_small: "{0}x{1} — very small; the TUI would fall back to the minimal layout (only affects the TUI, not `ls` or `doctor`)",
    d_size_err: "cannot determine terminal size ({0}); stdout is probably not a TTY — only the TUI needs it, `ls` and `doctor` do not",
    d_config_missing: "$HOME is unset and neither $SCREEN_TUI_HOME nor $XDG_CONFIG_HOME is usable",
    d_config_missing_fix: "set $HOME, or point $SCREEN_TUI_HOME at a writable directory",
    d_config_ok: "{0} — writable, mode 0700",
    d_config_fail: "{0} — write probe failed: {1}",
    d_config_perm_fix: "check ownership and permissions of {0}",
    d_no_session_probe: "no connectable session available to test `-X hardcopy`; capability left unverified (nothing was written to disk)",
    d_no_session_probe_fix: "create a session (`screen -dmS probe sleep 60`) and re-run doctor to verify preview",
    d_hist_yes: "history buffer (-h) available",
    d_hist_no: "history buffer (-h) unavailable",
    d_hist_unknown: "history buffer (-h) unverified",
    d_hc_ok: "`-X hardcopy` works on {0} ({1})",
    d_hc_no: "`-X hardcopy` is unsupported on this build ({0}); the preview pane will show metadata only — never stale or fabricated content",
    d_hc_unknown: "could not verify `-X hardcopy` ({0}); preview degrades to metadata only",
    d_hc_probe_failed: "hardcopy probe failed: {0}; preview degrades to metadata only",
    d_note11: "check 11 is the only side-effecting one: it writes a real `-X hardcopy` to a 0600 temp file and removes it immediately.",
    name_empty: "name must not be empty",
    name_leading_dash: "name must not start with '-' (screen would read it as an option)",
    name_bad_char: "name must not contain whitespace or control characters (found {0})",
    name_too_long: "name is longer than {0} characters",
    f_name: "Name",
    f_dir: "Directory",
    f_command: "Command",
    dir_empty: "directory must not be empty",
    not_a_directory: "not a directory: {0}",
    duplicate_note: "a session named '{0}' already exists; address it as <pid>.{0}",
    name_was_taken: "'{0}' was taken; used '{1}'",
    created: "created '{0}'",
    created_with_hint: "created '{0}' ({1})",
    skip_not_listed: "it is not in the session list yet",
    skip_not_unique: "the name is not unique; start it as <pid>.<name>",
    not_entering: "{0}; not entering: {1}",
    create_failed: "create failed: {0}",
    screen_refused: "screen refused to create '{0}' (exit {1}):\n  {2} ran in {3}\n  {4}",
    screen_no_diagnostic: "screen produced no diagnostic output; check the name and directory",
    act_detach: "detach",
    act_kill: "kill",
    act_wipe: "wipe",
    act_cleanup: "cleanup",
    consequence_detach: "detach the attached client (it keeps running)",
    consequence_kill: "TERMINATE the session and all its windows",
    consequence_wipe: "remove all dead session sockets",
    consequence_cleanup: "forget metadata of sessions that no longer exist",
    verify_failed: "cannot verify sessions before connecting: {0}",
    verify_failed_action: "cannot verify sessions before {0}: {1}",
    session_gone: "session '{0}' is gone; list refreshed",
    unknown_state: "'{0}' reports unknown state '{1}'; refusing to connect",
    not_connectable_share: "'{0}' is not connectable; cannot share",
    dead_wipe_hint: "'{0}' is dead; wipe it before connecting",
    unreachable_hint: "'{0}' is unreachable; check the socket dir",
    not_attached: "'{0}' is not attached; nothing to detach (use Enter to connect)",
    no_dead_to_wipe: "no dead sessions; nothing to wipe",
    dead_sessions_display: "dead sessions",
    no_dead_left: "no dead sessions left; nothing to wipe",
    gone_kill: "'{0}' is gone; nothing to kill",
    kill_attached: "'{0}' is attached; kill refused (detach it first)",
    gone_detach: "'{0}' is gone; nothing to detach",
    no_longer_attached: "'{0}' is no longer attached (now {1}); nothing to detach",
    wiped: "dead sessions wiped",
    action_done: "'{0}' {1} done",
    action_failed: "{0} '{1}' failed (exit {2}){3}",
    action_failed_short: "{0} failed: {1}",
    nothing_to_do: "nothing to do",
    attach_failed: "attach failed: {0}",
    attach_exit_code: "attach failed: screen exited with code {0}",
    renamed_to: "session renamed to '{0}'",
    rename_failed: "rename failed (exit {0}): {1}",
    rename_failed_short: "rename failed: {0}",
    meta_alias: "alias",
    meta_note: "note",
    meta_updated: "'{0}' {1} updated",
    meta_kept_ro: "'{0}' {1} kept for this session only (no config file written)",
    no_stale_metadata: "no stale session metadata to clean",
    stale_entries: "{0} stale metadata entrie(s)",
    removed_stale: "removed {0} stale metadata entrie(s)",
    removed_stale_ro: "removed {0} stale metadata entrie(s) for this session only (no config file written)",
    restart_still_running: "'{0}' is still running; restart applies to dead sessions",
    restart_not_managed: "'{0}' was not created by stui; restart is unavailable",
    restart_unmanaged: "'{0}' is unmanaged; restart is only available for sessions created by stui",
    restart_no_record: "'{0}' has no recorded command/cwd; cannot restart",
    restarted: "restarted '{0}' with its recorded command",
    restart_failed: "restart of '{0}' failed (exit {1}): {2}",
    restart_failed_short: "restart failed: {0}",
    preview_not_running: "'{0}' is not running; there is nothing to preview",
    preview_unavailable: "preview unavailable: hardcopy support is {0} on this screen build (run `stui doctor` for details)",
    detach_hint_some: "Tip: detach with {0} d",
    detach_hint_default: "Tip: detach with Ctrl-A D (default prefix - use your own prefix + d if you changed it)",
    refresh_failed: "refresh failed: {0}",
    header_self: "  ·  ● in screen: {0}",
    attach_self: "you are inside '{0}'; screen refuses to attach from within itself",
    desc_self_marker: "the session you are currently inside",
    list_empty: "No screen sessions. Press n to create one.",
    list_filter_empty: "No sessions match '/{0}'. Esc clears the filter.",
    header_sessions: "  {0} session(s) · {1} attached",
    header_dead: " · {0} dead",
    footer_wide_1: " j/k move  Enter attach  1-9 quick  n new  x share  i detail  R refresh",
    footer_wide_2: " / filter  D detach  K kill  r rename  W wipe(dead)  s restart  X cleanup  ? help  q quit",
    footer_wipe_dead: "  W wipe dead",
    footer_mid: " n new  1-9 attach  i detail  / find  K kill  ? help  q quit",
    footer_wipe: "  W wipe",
    footer_tiny: " ? help  q quit",
    footer_refresh: " refresh every {0}s",
    footer_manual: " manual refresh (R)",
    error_title: " Error ",
    error_hint: "Enter / Esc close",
    help_title: " Help ",
    desc_move: " move selection",
    desc_attach: "     enter session (stui is replaced by screen)",
    desc_quick: "        quick attach by row",
    desc_share: "        share attach (-x)",
    desc_preview: "        preview snapshot",
    desc_new: "        new session",
    desc_detail: "        detail of selection",
    desc_filter: "        filter sessions",
    desc_detach: "        remote detach",
    desc_kill: "        kill (confirm)",
    desc_rename: "        rename",
    desc_wipe: "        wipe dead (confirm)",
    desc_refresh: "        refresh now",
    desc_quit: "  quit / close",
    version_unknown: "version unknown",
    detail_title: " Detail ",
    detail_nothing: " nothing selected",
    detail_hint: " a alias · t note · Esc close",
    dl_name: "name",
    dl_address: "address",
    dl_pid: "pid",
    dl_status: "status",
    dl_created: "created",
    dl_windows: "windows",
    dl_cwd: "cwd",
    dl_command: "command",
    dl_alias: "alias",
    dl_note: "note",
    dl_managed: "managed",
    dl_managed_yes: "yes (created by stui; restart with s)",
    meta_title: " Metadata ",
    meta_line: " {0} for '{1}':",
    meta_hint: " Enter save · empty clears · Esc cancel",
    rename_title: " Rename ",
    rename_new_name: " New name:",
    rename_hint: " Enter rename · Esc cancel",
    confirm_title: " Confirm: {0}",
    confirm_session: " session: {0}",
    confirm_command: " command: {0}",
    confirm_yes: " [ Yes ] ",
    confirm_cancel: " [ Cancel ] ",
    confirm_hint: " ←/→ switch focus · Enter run focused · y confirm · Esc cancel",
    filter_title: " Filter ",
    filter_match: " {0} of {1} sessions match",
    filter_hint: " Enter apply · Esc clear filter",
    preview_pane_hint: " press p to snapshot the selected session",
    preview_title: " Preview ",
    preview_overlay_title: " Preview - {0} · fetched {1} ",
    preview_fetched: " fetched {0} · session {1}",
    preview_empty: " (empty window)",
    new_title: " New session ",
    new_hint: " Tab/↓ next · ↑ back · Enter create · Esc cancel",
};

// ------------------------------------------------------------- 中文

const DICT_ZH: Dict = Dict {
    note_label: "提示",
    ls_no_sessions: "# 未发现 screen 会话（socket 目录 {0}）",
    ls_summary: "# {0} 个会话 · socket 目录 {1} · {2}",
    ls_col_idx: "#",
    ls_col_name: "名字",
    ls_col_status: "状态",
    ls_col_pid: "PID",
    ls_col_created: "创建时间",
    st_detached: "分离",
    st_attached: "连接",
    st_multi: "多路",
    st_dead: "死亡",
    st_unreachable: "不可达",
    outlook_no_sessions: "无会话（exit 9）",
    outlook_unconnectable: "有会话但均不可连接（exit 10）",
    outlook_available: "可用（exit {0}）",
    outlook_inconclusive: "无法判定（exit {0}，不在手册 9/10/11+ 表内）",
    d_title: "stui doctor — 环境自检",
    d_fix: "修复：",
    d_summary: "{0} 项通过 · {1} 项警告 · {2} 项失败",
    d_notes: "说明：",
    d_name_screen: "screen 可执行文件",
    d_name_version: "版本",
    d_name_socket: "socket 目录",
    d_name_enum: "会话枚举",
    d_name_terminfo: "terminfo",
    d_name_utf8: "UTF-8 locale",
    d_name_dead: "dead 会话",
    d_name_nesting: "screen 嵌套",
    d_name_size: "终端尺寸",
    d_name_config: "配置目录",
    d_name_preview: "预览能力",
    d_screen_missing_fix: "安装 GNU Screen：`apt install screen`（Debian/Ubuntu）| `yum install screen`（RHEL）| `brew install screen`（macOS）",
    d_version_na: "screen 不可用，无法确定版本",
    d_install_screen: "请先安装 GNU Screen",
    d_version_unparsed: "无法从 `screen -v` 输出解析版本号：{0}",
    d_value_empty: "<空>",
    d_caps_live_fix: "能力开关来自实跑探测，stui 仍可正常工作",
    d_q_ok: "{0}；`-Q` 查询可用",
    d_q_no: "{0}；此版本不支持 `-Q` 查询 —— 窗口数/标题依赖 `-Q`，将不显示。核心功能不受影响。",
    d_q_no_fix: "无需处理；Screen 4.6 以下属正常现象",
    d_q_unknown: "{0}；无法验证 `-Q`（没有可用会话来测试）",
    d_q_unknown_fix: "存在会话时重新运行 `stui doctor` 可得到确定结论",
    d_ls_failed: "`screen -ls` 失败：{0}",
    d_screendir_fix: "确认 $SCREENDIR 与 $TMPDIR 未设置或非空 —— 空值会让所有 screen 调用失败；同时检查 socket 目录权限",
    d_screendir_empty: "`-ls` 可用，但 $SCREENDIR 被导出为空。当前版本恰好容忍此情况（screen 回退到 `$TMPDIR/.screen`），但在其他版本上空 SCREENDIR 曾导致 `Cannot access ...`。",
    d_screendir_empty_fix: "取消 SCREENDIR，或导出为真实路径",
    d_socket_ok: "`-ls` 可达；socket 目录 {0}",
    d_not_reported: "`-ls` 未报告",
    d_enum_cannot: "无法运行 `screen -q -ls`",
    d_fix_screen_first: "请先修复「screen 可执行文件」检查项",
    d_enum_exit10: "`-q -ls` 退出码 10：有会话但均不可连接（列出 {0} 条）；手册将此归为权限问题",
    d_enum_perm_fix: "检查 socket 目录的属主与权限",
    d_enum_exit9: "`-q -ls` 报告无会话（退出码 9）但 `-ls` 列出 {0} 条；以 `-ls` 为准。手册的 9/10/11+ 退出码表并非在所有版本上都可靠。",
    d_enum_mismatch: "`-q -ls` 报告 {0} 个会话但 `-ls` 明细为 {1} 条；以 `-ls` 为准",
    d_enum_code: "`-q -ls` 退出码 {0}，不在手册 9/10/11+ 表内（Screen 4.00.03 实测：无会话时为 8）。stui 回退为解析 `-ls` 文本；找到 {1} 个会话。",
    d_enum_code_fix: "无需处理；已记录到版本能力矩阵",
    d_enum_ok: "{0} 个会话 · {1}",
    d_ti_present: "`screen-256color` 存在",
    d_ti_failed: "`infocmp screen-256color` 失败（退出码 {0}）；stui 退回普通 `screen` terminfo，会话内的 256 色输出可能无法渲染",
    d_ti_fix: "安装 ncurses terminfo 扩展：`apt install ncurses-term`（Debian/Ubuntu）",
    d_ti_missing: "`infocmp` 不可用，无法验证 terminfo",
    d_ti_missing_fix: "如需此检查生效请安装 ncurses-bin（Debian/Ubuntu）",
    d_utf8_none: "未设置任何 locale 变量",
    d_utf8_none_fix: "export LANG=<你的>.UTF-8；否则 CJK/emoji 会话名可能错位",
    d_utf8_not: "locale 不是 UTF-8（{0}）；CJK/emoji 会话名可能显示错误或错位",
    d_utf8_fix: "export LANG=<你的>.UTF-8（仅建议，不会替你执行）：在 ~/.screenrc 加 `defutf8 on` —— stui 绝不修改你的 screenrc",
    d_cannot_enum: "无法枚举会话",
    d_dead_none: "无",
    d_dead_unreachable: "无 dead，但有 {0} 个 unreachable（仅展示，绝不连接）",
    d_dead_list: "{0} 个 dead 会话：{1} —— 会污染列表且可能误触",
    d_dead_fix: "运行 `screen -wipe`（或在 TUI 中按 `W`）—— stui 总会先要求确认",
    d_not_inside: "不在 screen 会话内运行（$STY 未设置）",
    d_inside: "正运行在 screen 会话内（STY={0}）；screen 拒绝从自身内部连接",
    d_inside_fix: "先 detach，或使用共享连接（`-x`）—— 唯一能从内部使用的方式",
    d_size_small: "{0}x{1} —— 过小；TUI 将回退到最小布局（只影响 TUI，不影响 `ls` 与 `doctor`）",
    d_size_err: "无法确定终端尺寸（{0}）；stdout 可能不是 TTY —— 只有 TUI 需要，`ls` 与 `doctor` 不需要",
    d_config_missing: "$HOME 未设置，且 $SCREEN_TUI_HOME 与 $XDG_CONFIG_HOME 均不可用",
    d_config_missing_fix: "设置 $HOME，或将 $SCREEN_TUI_HOME 指向可写目录",
    d_config_ok: "{0} —— 可写，权限 0700",
    d_config_fail: "{0} —— 写入探测失败：{1}",
    d_config_perm_fix: "检查 {0} 的属主与权限",
    d_no_session_probe: "没有可连接会话来测试 `-X hardcopy`；能力未验证（未写盘）",
    d_no_session_probe_fix: "创建一个会话（`screen -dmS probe sleep 60`）后重新运行 doctor 以验证预览",
    d_hist_yes: "历史缓冲（-h）可用",
    d_hist_no: "历史缓冲（-h）不可用",
    d_hist_unknown: "历史缓冲（-h）未验证",
    d_hc_ok: "`-X hardcopy` 在 {0} 上可用（{1}）",
    d_hc_no: "此版本不支持 `-X hardcopy`（{0}）；预览面板将只显示元数据 —— 绝不显示过期或伪造内容",
    d_hc_unknown: "无法验证 `-X hardcopy`（{0}）；预览降级为仅元数据",
    d_hc_probe_failed: "hardcopy 探测失败：{0}；预览降级为仅元数据",
    d_note11: "第 11 项是唯一有副作用的检查：它会向一个 0600 临时文件写入真实的 `-X hardcopy`，读完立即删除。",
    name_empty: "名字不能为空",
    name_leading_dash: "名字不能以 '-' 开头（screen 会把它当选项）",
    name_bad_char: "名字不能包含空白或控制字符（发现 {0}）",
    name_too_long: "名字超过 {0} 个字符",
    f_name: "名字",
    f_dir: "目录",
    f_command: "命令",
    dir_empty: "目录不能为空",
    not_a_directory: "不是目录：{0}",
    duplicate_note: "同名会话 '{0}' 已存在；请用 <pid>.{0} 寻址",
    name_was_taken: "'{0}' 已被占用；已使用 '{1}'",
    created: "已创建 '{0}'",
    created_with_hint: "已创建 '{0}'（{1}）",
    skip_not_listed: "它尚未出现在会话列表中",
    skip_not_unique: "名字不唯一；请用 <pid>.<名字> 连接",
    not_entering: "{0}；未进入：{1}",
    create_failed: "创建失败：{0}",
    screen_refused: "screen 拒绝创建 '{0}'（退出码 {1}）：\n  {2} 运行于 {3}\n  {4}",
    screen_no_diagnostic: "screen 未给出诊断输出；请检查名字与目录",
    act_detach: "断开",
    act_kill: "终止",
    act_wipe: "清理",
    act_cleanup: "清档",
    consequence_detach: "断开已连接的客户端（会话继续运行）",
    consequence_kill: "终止该会话及其全部窗口",
    consequence_wipe: "删除所有 dead 会话的 socket",
    consequence_cleanup: "忘记已不存在会话的元数据",
    verify_failed: "连接前无法校验会话状态：{0}",
    verify_failed_action: "{0}前无法校验会话状态：{1}",
    session_gone: "会话 '{0}' 已消失；列表已刷新",
    unknown_state: "'{0}' 状态未知（'{1}'）；拒绝连接",
    not_connectable_share: "'{0}' 不可连接；无法共享",
    dead_wipe_hint: "'{0}' 已死亡；请先清理再连接",
    unreachable_hint: "'{0}' 不可达；请检查 socket 目录",
    not_attached: "'{0}' 未被连接；无可断开（用 Enter 连接）",
    no_dead_to_wipe: "没有 dead 会话；无需清理",
    dead_sessions_display: "dead 会话",
    no_dead_left: "已无 dead 会话；无需清理",
    gone_kill: "'{0}' 已消失；无需终止",
    kill_attached: "'{0}' 正被连接；拒绝终止（请先断开）",
    gone_detach: "'{0}' 已消失；无可断开",
    no_longer_attached: "'{0}' 已不再被连接（现为 {1}）；无可断开",
    wiped: "dead 会话已清理",
    action_done: "'{0}' {1}完成",
    action_failed: "{0} '{1}' 失败（退出码 {2}）{3}",
    action_failed_short: "{0}失败：{1}",
    nothing_to_do: "无可执行操作",
    attach_failed: "连接失败：{0}",
    attach_exit_code: "连接失败：screen 退出码 {0}",
    renamed_to: "会话已重命名为 '{0}'",
    rename_failed: "重命名失败（退出码 {0}）：{1}",
    rename_failed_short: "重命名失败：{0}",
    meta_alias: "别名",
    meta_note: "备注",
    meta_updated: "'{0}' 的{1}已更新",
    meta_kept_ro: "'{0}' 的{1}仅本会话生效（未写配置文件）",
    no_stale_metadata: "没有可清理的过期元数据",
    stale_entries: "{0} 条过期元数据",
    removed_stale: "已删除 {0} 条过期元数据",
    removed_stale_ro: "已删除 {0} 条过期元数据（仅本会话生效，未写配置文件）",
    restart_still_running: "'{0}' 仍在运行；重启只适用于 dead 会话",
    restart_not_managed: "'{0}' 非 stui 创建；不可重启",
    restart_unmanaged: "'{0}' 非托管会话；重启仅适用于 stui 创建的会话",
    restart_no_record: "'{0}' 没有记录命令/目录；无法重启",
    restarted: "已用记录的命令重启 '{0}'",
    restart_failed: "'{0}' 重启失败（退出码 {1}）：{2}",
    restart_failed_short: "重启失败：{0}",
    preview_not_running: "'{0}' 未在运行；没有可预览的内容",
    preview_unavailable: "预览不可用：此 screen 版本的 hardcopy 支持为 {0}（详情运行 `stui doctor`）",
    detach_hint_some: "提示：按 {0} d 断开",
    detach_hint_default: "提示：按 Ctrl-A D 断开（默认前缀 —— 若你改过前缀，请用你的前缀 + d）",
    refresh_failed: "刷新失败：{0}",
    header_self: "  ·  ● 当前在 screen: {0}",
    attach_self: "你正在 '{0}' 会话内；screen 拒绝从自身内部连接",
    desc_self_marker: "标记你所在的会话",
    list_empty: "没有 screen 会话。按 n 新建。",
    list_filter_empty: "没有会话匹配 '/{0}'。按 Esc 清除过滤。",
    header_sessions: "  {0} 个会话 · {1} 个已连接",
    header_dead: " · {0} 个 dead",
    footer_wide_1: " j/k 移动  Enter 连接  1-9 快选  n 新建  x 共享  i 详情  R 刷新",
    footer_wide_2: " / 过滤  D 断开  K 终止  r 重命名  W 清理(dead)  s 重启  X 清档  ? 帮助  q 退出",
    footer_wipe_dead: "  W 清理 dead",
    footer_mid: " n 新建  1-9 连接  i 详情  / 查找  K 终止  ? 帮助  q 退出",
    footer_wipe: "  W 清理",
    footer_tiny: " ? 帮助  q 退出",
    footer_refresh: " 每 {0}s 刷新",
    footer_manual: " 手动刷新（R）",
    error_title: " 错误 ",
    error_hint: "Enter / Esc 关闭",
    help_title: " 帮助 ",
    desc_move: " 移动选择",
    desc_attach: "     进入会话（stui 被 screen 替换，退出后回原 shell）",
    desc_quick: "        按行号快速连接",
    desc_share: "        共享连接（-x）",
    desc_preview: "        预览快照",
    desc_new: "        新建会话",
    desc_detail: "        选中会话详情",
    desc_filter: "        过滤会话",
    desc_detach: "        远程断开",
    desc_kill: "        终止（需确认）",
    desc_rename: "        重命名",
    desc_wipe: "        清理 dead（需确认）",
    desc_refresh: "        立即刷新",
    desc_quit: "  退出 / 关闭",
    version_unknown: "版本未知",
    detail_title: " 详情 ",
    detail_nothing: " 未选中会话",
    detail_hint: " a 别名 · t 备注 · Esc 关闭",
    dl_name: "名字",
    dl_address: "地址",
    dl_pid: "PID",
    dl_status: "状态",
    dl_created: "创建时间",
    dl_windows: "窗口",
    dl_cwd: "目录",
    dl_command: "命令",
    dl_alias: "别名",
    dl_note: "备注",
    dl_managed: "托管",
    dl_managed_yes: "是（stui 创建；按 s 重启）",
    meta_title: " 元数据 ",
    meta_line: " '{1}' 的{0}：",
    meta_hint: " Enter 保存 · 留空清除 · Esc 取消",
    rename_title: " 重命名 ",
    rename_new_name: " 新名字：",
    rename_hint: " Enter 重命名 · Esc 取消",
    confirm_title: " 确认：{0}",
    confirm_session: " 会话：{0}",
    confirm_command: " 命令：{0}",
    confirm_yes: " [ 是 ] ",
    confirm_cancel: " [ 取消 ] ",
    confirm_hint: " ←/→ 切换焦点 · Enter 执行焦点项 · y 确认 · Esc 取消",
    filter_title: " 过滤 ",
    filter_match: " {0}/{1} 个会话匹配",
    filter_hint: " Enter 应用 · Esc 清除过滤",
    preview_pane_hint: " 按 p 抓取选中会话的快照",
    preview_title: " 预览 ",
    preview_overlay_title: " 预览 - {0} · 抓取于 {1} ",
    preview_fetched: " 抓取于 {0} · 会话 {1}",
    preview_empty: " （空窗口）",
    new_title: " 新建会话 ",
    new_hint: " Tab/↓ 下一项 · ↑ 上一项 · Enter 创建 · Esc 取消",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn en_and_zh_dicts_are_field_complete() {
        // 结构体字面量保证字段齐全，这里只验证两份表都能取到且非空。
        let en = dict(Lang::En);
        let zh = dict(Lang::Zh);
        assert!(!en.ls_summary.is_empty());
        assert!(!zh.ls_summary.is_empty());
        assert!(!zh.header_sessions.contains("session(s)"));
    }

    #[test]
    fn detect_follows_explicit_settings_then_locale() {
        assert_eq!(Lang::detect("zh"), Lang::Zh);
        assert_eq!(Lang::detect(" ZH "), Lang::Zh);
        assert_eq!(Lang::detect("en"), Lang::En);
        // auto / 未知值走 locale：测试进程不 init，但 detect 是纯函数。
        // 无法假设运行环境的 LANG，只断言它给出两种结果之一。
        assert!(matches!(Lang::detect("auto"), Lang::En | Lang::Zh));
        assert!(matches!(Lang::detect("fr"), Lang::En | Lang::Zh));
    }

    #[test]
    fn fmt_replaces_placeholders_in_order_and_repeats() {
        assert_eq!(fmt("a{0}b{1}c{0}", &["x", "y"]), "axbycx");
        assert_eq!(fmt("no args {0}", &[]), "no args {0}");
        // 参数内容不再扫描：占位符文本出现在参数里也不受影响。
        assert_eq!(fmt("{0}", &["{1}"]), "{1}");
        assert_eq!(fmt("{}", &["x"]), "{}");
    }

    #[test]
    fn zh_messages_render_with_placeholders() {
        let zh = dict(Lang::Zh);
        assert_eq!(
            fmt(zh.ls_no_sessions, &["/tmp/.screen"]),
            "# 未发现 screen 会话（socket 目录 /tmp/.screen）"
        );
        assert_eq!(fmt(zh.stale_entries, &["3"]), "3 条过期元数据");
    }

    #[test]
    fn uninit_lang_defaults_to_english() {
        // 测试进程从不 init：全局默认必须是英文，既有英文断言才稳定。
        assert_eq!(lang(), Lang::En);
        assert_eq!(t().name_empty, dict(Lang::En).name_empty);
    }
}
