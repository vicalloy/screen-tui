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
}
