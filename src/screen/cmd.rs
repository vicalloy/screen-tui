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

/// 前台连接：继承 stdio 阻塞到子进程退出（1.5d）。调用方负责先 suspend 终端。
pub fn attach(kind: AttachKind, target: &str) -> Result<Run> {
    let program = program()?;
    attach_with(&program, kind, target)
}

/// [`attach`] 的注入版：替身测试不碰真实 screen。
///
/// 用 `.status()` 而非 `.output()` —— 连接是交互式的，stdio 必须直通终端；
/// 因此拿不到子进程输出文本，退出码是唯一可信凭据。
pub fn attach_with(program: &Path, kind: AttachKind, target: &str) -> Result<Run> {
    let args = attach_args(kind, target);
    let status = Command::new(program)
        .args(&args)
        .env_remove("STY") // 与 create 同理：嵌套时 screen 拒绝。
        .status()
        .map_err(|source| Error::Spawn {
            program: program.display().to_string(),
            source,
        })?;

    Ok(Run {
        command: display_command(program, &args),
        code: status.code().unwrap_or(-1),
        stdout: String::new(),
        stderr: String::new(),
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

    /// 替身 attach：把收到的参数与 $STY 写进文件，按需返回退出码。
    /// `.status()` 继承 stdio，所以断言走文件而不是 stdout。
    #[test]
    fn attach_with_propagates_exit_code() {
        let tmp = std::env::temp_dir().join(format!("stui-attach-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let record = tmp.join("record.txt");
        let record_path = record.display().to_string();
        let script = fake_screen(
            &tmp,
            &format!("echo \"$@\" > {record_path}; echo \"STY=$STY\" >> {record_path}; exit 7"),
        );

        let run = attach_with(&script, AttachKind::Takeover, "12345.work").unwrap();
        // 退出码任意 → 原样带回（1.5d 替身契约：退出码不会吞掉，TUI 必能据实报告）。
        assert_eq!(run.code, 7);
        assert!(!run.success());

        let recorded = std::fs::read_to_string(&record).unwrap();
        assert!(recorded.contains("-U -d -r 12345.work"), "{recorded}");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `env_remove("STY")` 的行为验证：父进程设 STY，子进程必须看不到。
    /// 环境变量是进程全局的，用互斥锁隔离（其余测试不读 STY）。
    #[test]
    fn attach_with_strips_sty_from_child_env() {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let tmp = std::env::temp_dir().join(format!("stui-attach-sty-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let record = tmp.join("record.txt");
        let record_path = record.display().to_string();
        let script = fake_screen(&tmp, &format!("echo \"STY=$STY\" > {record_path}; exit 0"));

        // 2024 edition 起 set_var/remove_var 标记为 unsafe（进程全局状态）；
        // 互斥锁已保证唯一访问者，此处安全性由 ENV_LOCK 承担。
        unsafe { std::env::set_var("STY", "12345.tty1.marker") };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            attach_with(&script, AttachKind::Resume, "work").unwrap()
        }));
        unsafe { std::env::remove_var("STY") };
        drop(lock);

        result.expect("attach_with should not panic");
        let recorded = std::fs::read_to_string(&record).unwrap();
        assert_eq!(recorded.trim(), "STY=", "STY must be stripped: {recorded}");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
