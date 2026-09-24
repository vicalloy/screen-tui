//! 会话预览：hardcopy 只读快照（FR-15 / T2.3，tech-design §3.3）。
//!
//! 四条硬规则：
//!
//! 1. 临时文件权限 0600，`TempFile` RAII 保证**成功/失败/panic 三条路径都删**；
//! 2. 每次预览都重新抓取，**不缓存** —— 显示陈旧内容是 FR-15 明令禁止的降级方式；
//! 3. 预览是只读操作，不 attach、不注入（C-1）；
//! 4. 预览不可用时返回 `PreviewUnavailable`，由 UI 明确显示原因。

use std::ffi::OsString;
use std::process::Command;

use super::{Error, Result, cmd};
use crate::util::tmpfile::TempFile;

/// 预览行的最大显示宽度（超出按显示宽度裁剪，不触发终端换行，NFR-06）。
pub const PREVIEW_COLS: usize = 96;

/// 抓取会话可见画面。`full` 是 `<pid>.<name>` 全名。返回清洗后的快照行。
pub fn preview(full: &str) -> Result<Vec<String>> {
    let program = cmd::program()?;
    preview_with(&program, full)
}

/// [`preview`] 的注入版（替身测试不碰真实 screen）。
pub fn preview_with(program: &std::path::Path, full: &str) -> Result<Vec<String>> {
    let tmp = TempFile::create("screen-tui-preview")?;
    let path = tmp.path_string();

    let args: Vec<OsString> = vec![
        "-S".into(),
        full.into(),
        "-X".into(),
        "hardcopy".into(),
        path.clone().into(),
    ];
    let output = Command::new(program)
        .args(&args)
        .output()
        .map_err(|source| Error::Spawn {
            program: program.display().to_string(),
            source,
        })?;
    let run = cmd::Run {
        command: format!(
            "{} -S {} -X hardcopy {}",
            program.display(),
            full,
            shell_quote(&path)
        ),
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    };

    // RAII：`tmp` 在函数返回时无条件删除 —— 提前 return 的路径同样被覆盖。
    if !run.success() {
        return Err(Error::PreviewUnavailable(reason(&run.text())));
    }
    // TempFile 先建了 0 字节占位文件：hardcopy 接受命令但没写盘时它仍是 0 字节。
    // 无法区分「空窗口」与「没写盘」，按 C-5 诚实报不可用，不给一个歧义的空预览。
    if tmp.bytes() == 0 {
        return Err(Error::PreviewUnavailable(
            "screen wrote no snapshot (empty or unsupported)".into(),
        ));
    }
    // 有损解码（read_lossy）：hardcopy 字节流不保证 UTF-8，严格解码会把预览卡死。
    let text = tmp.read_lossy();
    Ok(format_preview(&text, PREVIEW_COLS))
}

/// 失败原因（取第一条非空行，没有就说明退出码）。
fn reason(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "screen exited nonzero with no diagnostic".into())
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

/// 快照清洗（纯函数）：去 `\r`、逐行按显示宽度裁剪、去掉尾部空行。
///
/// 只动空白与超宽，不改内容本身 —— 预览要忠实于画面。
pub fn format_preview(text: &str, max_cols: usize) -> Vec<String> {
    let mut lines: Vec<String> = text
        .lines()
        .map(|line| line.trim_end_matches('\r').trim_end().to_string())
        .map(|line| crate::util::width::clip_with_ellipsis(&line, max_cols))
        .collect();
    while lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 替身 screen：把「会话画面」写进 hardcopy 指定的目标文件（第 5 个参数：
    /// $1=-S $2=<full> $3=-X $4=hardcopy $5=<path>）。
    fn fake_hardcopy_screen(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let script = format!("#!/bin/sh\ncat > \"$5\" <<'EOF'\n{body}\nEOF\nexit 0\n");
        let path = dir.join("fake-screen-hardcopy");
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// 通用替身：执行后按脚本内容动作（与 cmd 测试的 fake_screen 同款）。
    fn fake_screen(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("fake-screen");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn preview_reads_snapshot_and_deletes_tmp() {
        let dir = std::env::temp_dir().join(format!("stui-preview-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = fake_hardcopy_screen(&dir, "line one\nline two\n");

        let out = preview_with(&script, "12345.work").expect("preview ok");
        assert_eq!(out, vec!["line one", "line two"]);

        // 临时文件已随 RAII 删除：目录里只剩替身脚本。
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            leftovers,
            vec![std::ffi::OsString::from("fake-screen-hardcopy")]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preview_failing_screen_maps_to_preview_unavailable() {
        let dir = std::env::temp_dir().join(format!("stui-preview-fail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = fake_screen(&dir, "echo no such session >&2; exit 1");

        let err = preview_with(&script, "99999.gone").unwrap_err();
        match err {
            Error::PreviewUnavailable(reason) => {
                assert!(reason.contains("no such session"), "{reason}");
            }
            other => panic!("expected PreviewUnavailable, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preview_zero_exit_without_file_is_unavailable() {
        let dir = std::env::temp_dir().join(format!("stui-preview-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = fake_screen(&dir, "exit 0");

        let err = preview_with(&script, "12345.work").unwrap_err();
        assert!(matches!(err, Error::PreviewUnavailable(_)), "{err:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 快照含非 UTF-8 字节（会话里跑过的程序留下的 latin-1 / 二进制残留）时，
    /// 有损解码继续预览而不是报「stream did not contain valid UTF-8」。
    #[test]
    fn preview_tolerates_non_utf8_snapshot() {
        let dir = std::env::temp_dir().join(format!("stui-preview-bin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // printf 的八进制转义写一个 latin-1 é（0xE9），不是合法 UTF-8 序列。
        let script = fake_screen(&dir, "printf 'caf\\351 menu\\n' > \"$5\"; exit 0");

        let out = preview_with(&script, "12345.work").expect("lossy preview must succeed");
        assert_eq!(out, vec![format!("caf\u{FFFD} menu")]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn format_preview_clamps_and_trims_tail() {
        let text = "short\r\n  \ntrailing spaces   \n";
        let lines = format_preview(text, 40);
        // 尾部空行被裁掉；行尾空白去掉；\r 去掉；中间的空行是画面内容，保留。
        assert_eq!(lines, vec!["short", "", "trailing spaces"]);

        // 超宽行按显示宽度裁剪（CJK 宽字符安全）。
        let wide = "你".repeat(80);
        let lines = format_preview(&wide, 20);
        assert_eq!(lines.len(), 1);
        assert!(crate::util::width::display_width(&lines[0]) <= 20);
    }

    #[test]
    fn format_preview_keeps_content_lines() {
        let lines = format_preview("> found 3 failing tests\n> patching auth\n", 96);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "> found 3 failing tests");
    }
}
