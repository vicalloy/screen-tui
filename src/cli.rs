//! CLI 形态与子命令分发（tech-design §5）。
//!
//! ```
//! stui            # TUI（M1 交付，当前给出明确提示）
//! stui ls         # 纯文本会话列表（无 ANSI，可管道）
//! stui doctor     # 环境自检（11 项）
//! stui version    # 打印版本号
//! stui --version
//! ```
//!
//! 退出码约定（FR-22 验收 2）：`0` 有会话 / `1` 业务失败（无会话）/ `2` 环境异常。

use std::io::{IsTerminal, Write};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use crate::doctor;
use crate::screen::parse::{self, Enumeration, SessionRecord};
use crate::util::width::{clip_with_ellipsis, display_width, pad_right};

/// 有会话。
pub const EXIT_OK: u8 = 0;
/// 业务失败（例如没有会话可列）。
pub const EXIT_FAILURE: u8 = 1;
/// 环境异常（screen 缺失、`-ls` 跑不通等）。
pub const EXIT_ENV: u8 = 2;

#[derive(Debug, Parser)]
#[command(
    name = "stui",
    version,
    about = "A TUI for managing GNU Screen sessions, optimized for phone-sized SSH terminals",
    long_about = "stui is a front-end for GNU Screen. It lists your sessions, tells you what each \
                  one is doing, and gets you in and out of them without memorising `screen -r` \
                  incantations.\n\n\
                  `stui ls` and `stui doctor` are non-interactive and safe to pipe; they never \
                  emit ANSI escape sequences."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print the session list as plain text (no ANSI, safe to pipe)
    Ls(LsArgs),
    /// Check the environment and print a report
    Doctor,
    /// Print version information
    Version,
}

#[derive(Debug, Args)]
pub struct LsArgs {
    /// Print data rows only: no header line, tab-separated fields
    ///
    /// Implied automatically when stdout is not a TTY.
    #[arg(long)]
    pub no_header: bool,

    /// Append the `<pid>.<name>` address as the last column
    #[arg(long)]
    pub full: bool,
}

/// 入口：解析参数并分发。
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    // 语言（FR-25）：`$STUI_LANG` 覆盖 > 配置 `language`（auto 时探测 locale），
    // 进程内初始化一次。配置读失败/损坏已由 config 层降级为默认值，此处只取语言，
    // 不关心 warnings（TUI 路径的 app::run 会再次加载并展示它们）。
    let language = std::env::var(crate::i18n::LANG_ENV)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| crate::config::load().config.language);
    crate::i18n::init(crate::i18n::Lang::detect(&language));
    match cli.command {
        Some(Command::Ls(args)) => ls(&args),
        Some(Command::Doctor) => doctor_command(),
        Some(Command::Version) => version_command(),
        None => {
            // TUI（M1）：需要交互终端；非 TTY 下 app::run 会自行给出明确报错。
            ExitCode::from(crate::app::run())
        }
    }
}

fn version_command() -> ExitCode {
    println!("stui {}", env!("CARGO_PKG_VERSION"));
    ExitCode::from(EXIT_OK)
}

fn doctor_command() -> ExitCode {
    let report = doctor::run();
    report.print();
    ExitCode::from(report.exit_code())
}

fn ls(args: &LsArgs) -> ExitCode {
    let enumeration = match parse::enumerate() {
        Ok(enumeration) => enumeration,
        Err(err) => {
            eprintln!("stui: {err}");
            return ExitCode::from(EXIT_ENV);
        }
    };

    // 诊断信息一律走 stderr —— stdout 是给管道的（FR-22 验收 1）。
    for note in enumeration.notes() {
        eprintln!("stui: {}: {note}", crate::i18n::t().note_label);
    }

    let machine_mode = !std::io::stdout().is_terminal() || args.no_header;
    if machine_mode {
        write_machine_rows(&enumeration, args);
    } else {
        write_pretty_rows(&enumeration, args);
    }

    if enumeration.has_sessions() {
        ExitCode::from(EXIT_OK)
    } else {
        ExitCode::from(EXIT_FAILURE)
    }
}

/// 机器模式：制表符分隔，固定字段顺序，绝不含 ANSI 或控制字符。
fn write_machine_rows(enumeration: &Enumeration, args: &LsArgs) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for (idx, session) in enumeration.list.sessions.iter().enumerate() {
        let mut fields = vec![
            (idx + 1).to_string(),
            sanitize(&session.name),
            sanitize(&session.status.label()),
            session.pid.map(|p| p.to_string()).unwrap_or_default(),
            session.created.clone().unwrap_or_default(),
        ];
        if args.full {
            fields.push(sanitize(&session.full));
        }
        let _ = writeln!(out, "{}", fields.join("\t"));
    }
    let _ = out.flush();
}

/// 人类模式：对齐列 + 表头；只在 TTY 下使用。
fn write_pretty_rows(enumeration: &Enumeration, args: &LsArgs) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let count = enumeration.count();
    let socket_dir = enumeration
        .list
        .socket_dir
        .clone()
        .unwrap_or_else(|| "unknown".to_string());

    if count == 0 {
        let _ = writeln!(
            out,
            "{}",
            crate::i18n::fmt(crate::i18n::t().ls_no_sessions, &[&socket_dir])
        );
        let _ = out.flush();
        return;
    }

    let _ = writeln!(
        out,
        "{}",
        crate::i18n::fmt(
            crate::i18n::t().ls_summary,
            &[
                &count.to_string(),
                &socket_dir,
                &enumeration.outlook.label()
            ]
        )
    );

    // 名字列宽自适应，上限 32 列（超出按显示宽度裁剪，CJK/emoji 安全）。
    let name_width = enumeration
        .list
        .sessions
        .iter()
        .map(|s| display_width(&s.name))
        .max()
        .unwrap_or(8)
        .clamp(8, 32);

    let with_created = enumeration
        .list
        .sessions
        .iter()
        .any(|s| s.created.is_some());

    // 表头：各列沿用行渲染的列宽；标签按显示宽度补齐（中文列头 CJK 安全）。
    let t = crate::i18n::t();
    let mut header = String::new();
    header.push_str(&pad_right(t.ls_col_idx, 3));
    header.push(' ');
    header.push_str(&pad_right(t.ls_col_name, name_width));
    header.push(' ');
    header.push_str(&pad_right(t.ls_col_status, 11));
    header.push(' ');
    header.push_str(&pad_right(t.ls_col_pid, 7));
    if with_created {
        header.push(' ');
        header.push_str(t.ls_col_created);
    }
    let _ = writeln!(out, "{}", header.trim_end());

    for (idx, session) in enumeration.list.sessions.iter().enumerate() {
        let name = clip_with_ellipsis(&sanitize(&session.name), name_width);
        let mut line = format!("{:<3} {} ", idx + 1, pad_right(&name, name_width),);
        line.push_str(&pad_right(&sanitize(&session.status.label()), 11));
        line.push(' ');
        line.push_str(&pad_right(
            &session
                .pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".into()),
            7,
        ));
        if with_created {
            line.push(' ');
            line.push_str(session.created.as_deref().unwrap_or("-"));
        }
        if args.full {
            line.push_str("  ");
            line.push_str(&sanitize(&session.full));
        }
        let _ = writeln!(out, "{}", line.trim_end());
    }
    let _ = out.flush();
}

/// 剥离控制字符，避免破坏列对齐或向终端注入转义序列（FR-22：无 ANSI 输出）。
fn sanitize(text: &str) -> String {
    crate::util::width::sanitize(text)
}

/// 供 `ls` 与后续 TUI 复用的排序后会话切片。
pub fn sessions_of(enumeration: &Enumeration) -> &[SessionRecord] {
    &enumeration.list.sessions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_expected_shapes() {
        let cli = Cli::try_parse_from(["stui", "ls", "--no-header", "--full"]).unwrap();
        match cli.command {
            Some(Command::Ls(args)) => {
                assert!(args.no_header);
                assert!(args.full);
            }
            other => panic!("expected ls, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["stui", "doctor"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Doctor)));

        let cli = Cli::try_parse_from(["stui", "version"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Version)));

        let cli = Cli::try_parse_from(["stui"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn exit_codes_match_the_documented_contract() {
        assert_eq!((EXIT_OK, EXIT_FAILURE, EXIT_ENV), (0, 1, 2));
    }

    #[test]
    fn sanitize_removes_escape_sequences_from_names() {
        assert_eq!(sanitize("a\tb"), "a?b");
        assert!(!sanitize("\x1b[31mred").contains('\x1b'));
    }
}
