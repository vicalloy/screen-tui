//! `screen` 命令构造与执行。
//!
//! 本机实测结论（macOS 4.00.03，原始证据见 `design/screen-capabilities.md` §6）：
//!
//! 1. `-v` / `-ls` / `-Q` 的文本都走 **stdout**，不是 stderr（stderr 为空）；
//! 2. 所有输出行以 `\r\n` 结尾，解析前必须去掉 `\r`；
//! 3. **`screen -v` 即使成功也返回退出码 1**（`--version` 同样），
//!    因此版本探测绝不能拿退出码判断成败，只能解析文本。

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{Error, Result};

/// 覆盖 screen 可执行文件路径的环境变量（多版本实测与单测注入用）。
pub const PROGRAM_ENV: &str = "STUI_SCREEN";

/// 解析 screen 可执行文件：`$STUI_SCREEN` 优先，否则逐项搜 `PATH`。
pub fn program() -> Result<PathBuf> {
    if let Some(raw) = std::env::var_os(PROGRAM_ENV).filter(|v| !v.is_empty()) {
        let path = PathBuf::from(raw);
        return if is_executable(&path) {
            Ok(path)
        } else {
            Err(Error::NotExecutable { path })
        };
    }

    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("screen");
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(Error::NotFound)
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// 一次命令执行的完整留痕。
///
/// 保留退出码与两路文本，而不是判断成败 —— Screen 的成败不总能由退出码体现
/// （见模块头第 3 条），上层需要文本才能做能力判定。
#[derive(Debug, Clone)]
pub struct Run {
    pub command: String,
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    pub fn success(&self) -> bool {
        self.code == 0
    }

    /// stdout 与 stderr 合并，供解析使用。
    pub fn text(&self) -> String {
        let mut out = self.stdout.clone();
        let err = self.stderr.trim();
        if !err.is_empty() {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&self.stderr);
        }
        out
    }
}

/// 执行 `screen <args...>` 并捕获输出（不继承 tty、不注入 stdin）。
///
/// 这里不做 `-q` 之外的任何参数加工 —— 命令语义由调用方决定，本函数只管跑。
pub fn run<I, S>(args: I) -> Result<Run>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args
        .into_iter()
        .map(|a| a.as_ref().to_os_string())
        .collect();
    let program = program()?;

    let output = Command::new(&program)
        .args(&args)
        .output()
        .map_err(|source| Error::Spawn {
            program: program.display().to_string(),
            source,
        })?;

    Ok(Run {
        command: display_command(&program, &args),
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// 新建会话的参数拼装（纯函数，供替身测试断言）。
///
/// `-U`（UTF-8）+ `-dmS <name> <command>`：detached 启动、不接管当前终端、
/// 显式命名。命令与名字各占一个 argv 项，不经 shell，天然无注入面。
pub fn create_args(name: &str, command: &str) -> Vec<OsString> {
    vec![
        OsString::from("-U"),
        OsString::from("-dmS"),
        OsString::from(name),
        OsString::from(command),
    ]
}

/// 会话内子进程应使用的 `TERM`：探测到 `screen-256color` 时启用，
/// 否则 `None`（沿用继承值，让 screen 自己降级 —— doctor 第 10 项的口径一致）。
pub fn term_for_session() -> Option<&'static str> {
    let status = Command::new("infocmp")
        .arg("screen-256color")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    status.success().then_some("screen-256color")
}

/// 新建 detached 会话。`dir` 作为子进程工作目录（显式 cwd，FR-02 验收 3）。
pub fn create(name: &str, dir: &Path, command: &str) -> Result<Run> {
    let program = program()?;
    create_with(&program, name, dir, command, term_for_session())
}

/// [`create`] 的注入版：program 与 TERM 都显式传入，替身测试不碰进程环境。
pub fn create_with(
    program: &Path,
    name: &str,
    dir: &Path,
    command: &str,
    term: Option<&str>,
) -> Result<Run> {
    let args = create_args(name, command);
    let mut command_builder = Command::new(program);
    command_builder
        .args(&args)
        .current_dir(dir) // 目录不存在/不可进入时在 spawn 阶段即失败，报错点最靠近根因。
        .env_remove("STY"); // 在 screen 会话里嵌套创建时必须摘掉 STY，否则 screen 拒绝。
    if let Some(term) = term {
        command_builder.env("TERM", term);
    }

    let output = command_builder.output().map_err(|source| Error::Spawn {
        program: program.display().to_string(),
        source,
    })?;

    Ok(Run {
        command: display_command_with_env(program, &args, term),
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// 同 [`display_command`]，附加 TERM 设定与工作目录，创建动作的留痕要能复盘环境。
fn display_command_with_env(program: &Path, args: &[OsString], term: Option<&str>) -> String {
    let mut line = display_command(program, args);
    if let Some(term) = term {
        line.push_str(&format!(" [TERM={term}]"));
    }
    line
}

/// 拼一条可读的命令行（用于报错与 doctor 展示）。
fn display_command(program: &Path, args: &[OsString]) -> String {
    let mut parts = vec![program.display().to_string()];
    for arg in args {
        parts.push(shell_quote(&arg.to_string_lossy()));
    }
    parts.join(" ")
}

fn shell_quote(raw: &str) -> String {
    if !raw.is_empty()
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@+".contains(c))
    {
        raw.to_string()
    } else {
        format!("'{}'", raw.replace('\'', "'\\''"))
    }
}

/// 连接方式（FR-03）。`Takeover` 用 `-d -r`——**绝不用 `-D -r`**（会把其它
/// 显示器全部踢下电，需求明令禁止）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachKind {
    /// `-r`：连接 detached 会话。
    Resume,
    /// `-x`：共享连接（会话已被 attach 时）。
    Share,
    /// `-d -r`：先 detach 再连接（接管）。
    Takeover,
}

impl AttachKind {
    pub fn label(self) -> &'static str {
        match self {
            AttachKind::Resume => "resume",
            AttachKind::Share => "share (-x)",
            AttachKind::Takeover => "takeover (-d -r)",
        }
    }
}

/// 连接参数拼装（纯函数）。三种方式都带 `-U`（UTF-8）。
pub fn attach_args(kind: AttachKind, target: &str) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["-U".into()];
    match kind {
        AttachKind::Resume => args.push("-r".into()),
        AttachKind::Share => args.push("-x".into()),
        AttachKind::Takeover => {
            args.push("-d".into());
            args.push("-r".into());
        }
    }
    args.push(target.into());
    args
}

/// 前台连接（FR-03 v0.2 四修）：**exec 替换进程**进入 screen，不用 spawn 子进程。
///
/// exec 链：`stui --exec--> sh --子进程--> screen 客户端`。
/// - screen 正常退出（detach / 会话内 exit）→ sh 退出 → 回到启动 stui 的 shell；
/// - screen 非零退出（attach 失败等）→ wrapper 立即 `exec stui --attach-failed <code>`
///   重启 TUI 并弹错误框 —— 连接失败不把用户丢回 shell。
///
/// 对 stui 而言全程只有 exec、没有 spawn+wait；sh 与 screen 的父子关系
/// 是 wrapper 的实现细节。参数全部经位置变量传给 sh（`$0`/`$1`/`"$@"`），
/// 不拼进脚本字符串，无注入面。只在 exec 本身失败（极罕见）返回 `io::Error`。
///
/// 不覆写 `TERM`：screen 客户端需要**外层真实终端**的 TERM 绘制界面，
/// 会话内 TERM 由 screen 自行设置；`STY` 必须摘掉（嵌套 screen 会被拒绝）。
pub fn exec_attach(kind: AttachKind, target: &str) -> std::io::Error {
    use std::os::unix::process::CommandExt;

    let script = "screen_bin=$1; shift; \"$screen_bin\" \"$@\"; code=$?; \
                  [ \"$code\" -eq 0 ] || exec \"$0\" --attach-failed \"$code\"";
    // program() 失败也照常 exec：sh 会因找不到 screen 以 127 退出，
    // 回环重启 stui 后由错误框如实报告 —— 不在这里静默降级。
    let screen = program().unwrap_or_else(|_| std::path::PathBuf::from("screen"));
    let stui = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("stui"));

    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(script)
        .arg(&stui) // $0：失败回环时的重启目标
        .arg(&screen) // $1：screen 客户端
        .args(attach_args(kind, target))
        .env_remove("STY");
    command.exec()
}

// ------------------------------------------------------------- 会话动作（T2.4）

/// 会话级管理动作（FR-12/13/20）。全部走 `-X` / `-wipe`，**没有任何 `stuff` 路径**
/// （需求 §1.3 非目标 #1：绝不向用户会话注入按键）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAction {
    /// 远程断开：`-S <full> -X detach`（不进入会话，FR-12）。
    Detach,
    /// 终止会话：`-S <full> -X quit`（FR-13）。
    Kill,
    /// 清理 dead 会话：`screen -wipe`（全局动作，无目标，FR-20）。
    Wipe,
}

impl SessionAction {
    pub fn label(self) -> &'static str {
        match self {
            SessionAction::Detach => "detach",
            SessionAction::Kill => "kill",
            SessionAction::Wipe => "wipe",
        }
    }
}

/// 动作参数拼装（纯函数，供替身测试断言）。`target` 是 `<pid>.<name>` 全名。
pub fn action_args(action: SessionAction, target: &str) -> Vec<OsString> {
    match action {
        SessionAction::Detach => vec![
            OsString::from("-S"),
            OsString::from(target),
            OsString::from("-X"),
            OsString::from("detach"),
        ],
        SessionAction::Kill => vec![
            OsString::from("-S"),
            OsString::from(target),
            OsString::from("-X"),
            OsString::from("quit"),
        ],
        SessionAction::Wipe => vec![OsString::from("-wipe")],
    }
}

/// 重命名参数拼装（纯函数，FR-14）：`-S <full> -X sessionname <new>`。
pub fn rename_args(target: &str, new_name: &str) -> Vec<OsString> {
    vec![
        OsString::from("-S"),
        OsString::from(target),
        OsString::from("-X"),
        OsString::from("sessionname"),
        OsString::from(new_name),
    ]
}

/// 执行会话动作（捕获输出，便于失败时给出可读原因）。
pub fn action(action: SessionAction, target: &str) -> Result<Run> {
    let program = program()?;
    action_with(&program, action, target)
}

/// [`action`] 的注入版。
pub fn action_with(program: &Path, action: SessionAction, target: &str) -> Result<Run> {
    let args = action_args(action, target);
    let output = Command::new(program)
        .args(&args)
        .output()
        .map_err(|source| Error::Spawn {
            program: program.display().to_string(),
            source,
        })?;
    Ok(Run {
        command: display_command(program, &args),
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// 重命名运行中的会话（FR-14）。
pub fn rename(target: &str, new_name: &str) -> Result<Run> {
    let program = program()?;
    rename_with(&program, target, new_name)
}

/// [`rename`] 的注入版。
pub fn rename_with(program: &Path, target: &str, new_name: &str) -> Result<Run> {
    let args = rename_args(target, new_name);
    let output = Command::new(program)
        .args(&args)
        .output()
        .map_err(|source| Error::Spawn {
            program: program.display().to_string(),
            source,
        })?;
    Ok(Run {
        command: display_command(program, &args),
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_only_when_needed() {
        assert_eq!(shell_quote("-ls"), "-ls");
        assert_eq!(shell_quote("/tmp/a b"), "'/tmp/a b'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn create_args_are_exactly_four_items() {
        let args = create_args("work", "vim notes.md");
        let flat: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(flat, vec!["-U", "-dmS", "work", "vim notes.md"]);
        // 命令整体占一个 argv 项 —— 不经 shell 拆词，无注入面。
        assert_eq!(args.len(), 4);
    }

    /// 替身 screen：回显收到的参数、cwd 与 TERM。1.4c 验收的「参数拼装正确、不真创建」。
    fn fake_screen(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("fake-screen");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn create_with_assembles_args_cwd_and_term() {
        let tmp = std::env::temp_dir().join(format!("stui-create-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let script = fake_screen(
            &tmp,
            "echo \"ARGS:$@\"; echo \"CWD:$PWD\"; echo \"TERM:$TERM\"; exit 0",
        );
        let workdir = std::env::temp_dir(); // 已存在的目录

        let run = create_with(&script, "demo", &workdir, "top", Some("screen-256color"))
            .expect("fake screen exits 0");

        assert!(run.success());
        assert!(
            run.stdout.contains("ARGS:-U -dmS demo top"),
            "{}",
            run.stdout
        );
        // macOS 的 /tmp 是 /private/tmp 的符号链接：$PWD 是解析后的真实路径。
        let real = std::fs::canonicalize(&workdir).unwrap();
        assert!(
            run.stdout.contains(&format!("CWD:{}", real.display())),
            "{}",
            run.stdout
        );
        assert!(
            run.stdout.contains("TERM:screen-256color"),
            "{}",
            run.stdout
        );
        // 留痕里带上 TERM 设定，可复盘。
        assert!(
            run.command.contains("TERM=screen-256color"),
            "{}",
            run.command
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn create_with_without_term_detection_keeps_inherited_env() {
        let tmp = std::env::temp_dir().join(format!("stui-create-nt-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let script = fake_screen(&tmp, "echo \"TERM:$TERM\"; exit 0");
        let workdir = std::env::temp_dir();

        let run = create_with(&script, "demo", &workdir, "top", None).unwrap();
        assert!(!run.command.contains("TERM="), "{}", run.command);
        assert!(
            !run.stdout.contains("TERM:screen-256color"),
            "{}",
            run.stdout
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn create_with_into_missing_dir_fails_at_spawn() {
        let tmp = std::env::temp_dir().join(format!("stui-create-bad-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let script = fake_screen(&tmp, "exit 0");
        let missing = tmp.join("no-such-dir");

        let err =
            create_with(&script, "demo", &missing, "top", None).expect_err("missing cwd must fail");

        assert!(matches!(err, Error::Spawn { .. }), "{err:?}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn create_with_failing_screen_surfaces_exit_code_and_text() {
        let tmp = std::env::temp_dir().join(format!("stui-create-fail-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let script = fake_screen(&tmp, "echo boom >&2; exit 1");
        let workdir = std::env::temp_dir();

        let run = create_with(&script, "demo", &workdir, "top", None).unwrap();
        assert!(!run.success());
        assert_eq!(run.code, 1);
        assert!(run.stderr.contains("boom"), "{}", run.stderr);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn attach_args_cover_the_three_kinds() {
        let flat = |kind| {
            attach_args(kind, "work")
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(flat(AttachKind::Resume), vec!["-U", "-r", "work"]);
        assert_eq!(flat(AttachKind::Share), vec!["-U", "-x", "work"]);
        // 接管是 `-d -r` 两个独立参数；绝不能出现 `-D`。
        assert_eq!(flat(AttachKind::Takeover), vec!["-U", "-d", "-r", "work"]);
        for kind in [AttachKind::Resume, AttachKind::Share, AttachKind::Takeover] {
            assert!(
                !flat(kind).iter().any(|a| a.contains("-D")),
                "no -D allowed"
            );
        }
    }

    // ------------------------------------------------------------- T2.4 会话动作

    #[test]
    fn action_args_match_the_documented_commands() {
        let flat = |args: &[OsString]| {
            args.iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            flat(&action_args(SessionAction::Detach, "12345.work")),
            vec!["-S", "12345.work", "-X", "detach"]
        );
        assert_eq!(
            flat(&action_args(SessionAction::Kill, "12345.work")),
            vec!["-S", "12345.work", "-X", "quit"]
        );
        // wipe 是全局动作：不带 -S 目标。
        assert_eq!(flat(&action_args(SessionAction::Wipe, "")), vec!["-wipe"]);
    }

    #[test]
    fn rename_args_carry_sessionname_subcommand() {
        let flat: Vec<String> = rename_args("12345.work", "new-name")
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            flat,
            vec!["-S", "12345.work", "-X", "sessionname", "new-name"]
        );
    }

    #[test]
    fn actions_run_through_the_fake_screen() {
        let tmp = std::env::temp_dir().join(format!("stui-action-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let script = fake_screen(&tmp, "echo \"ARGS:$@\"; exit 0");

        let run = action_with(&script, SessionAction::Detach, "12345.work").unwrap();
        assert!(run.success());
        assert!(run.stdout.contains("ARGS:-S 12345.work -X detach"));

        let run = action_with(&script, SessionAction::Kill, "12346.llm").unwrap();
        assert!(run.stdout.contains("ARGS:-S 12346.llm -X quit"));

        let run = action_with(&script, SessionAction::Wipe, "").unwrap();
        assert!(run.stdout.contains("ARGS:-wipe"));

        let run = rename_with(&script, "12345.work", "renamed").unwrap();
        assert!(
            run.stdout
                .contains("ARGS:-S 12345.work -X sessionname renamed")
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn failing_action_surfaces_exit_code_and_text() {
        let tmp = std::env::temp_dir().join(format!("stui-action-fail-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let script = fake_screen(&tmp, "echo no such session >&2; exit 1");

        let run = action_with(&script, SessionAction::Detach, "99999.gone").unwrap();
        assert!(!run.success());
        assert_eq!(run.code, 1);
        assert!(run.stderr.contains("no such session"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

}
