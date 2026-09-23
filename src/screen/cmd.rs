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
}
