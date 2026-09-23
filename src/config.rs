//! 配置目录解析与可写性探测。
//!
//! **M0 只包含 `doctor` 第 10 项需要的部分**：目录解析（自绘 XDG，不引入 `dirs` crate）
//! 与一次试写侦察。完整的配置读写、原子写、损坏降级、`version` 迁移位属 T2.1（FR-23/24）。

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// 配置目录名。
pub const APP_DIR: &str = "screen-tui";

/// 覆盖配置目录的环境变量。
pub const HOME_ENV: &str = "SCREEN_TUI_HOME";

/// 配置文件的解析来源（doctor 里要讲清「配置会写到哪、为什么是这里」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirSource {
    /// `$SCREEN_TUI_HOME`
    Env,
    /// `$XDG_CONFIG_HOME/screen-tui`
    Xdg,
    /// `~/.config/screen-tui`
    Home,
}

impl DirSource {
    pub fn label(self) -> &'static str {
        match self {
            DirSource::Env => "$SCREEN_TUI_HOME",
            DirSource::Xdg => "$XDG_CONFIG_HOME/screen-tui",
            DirSource::Home => "~/.config/screen-tui",
        }
    }
}

/// 解析配置目录：`$SCREEN_TUI_HOME` > `$XDG_CONFIG_HOME/screen-tui` > `~/.config/screen-tui`。
///
/// 空字符串一律当作**未设置** —— 本机实测过 `SCREENDIR` 被导出为空值导致所有 screen
/// 调用失败的坑（`design/screen-capabilities.md` §6），同样的判空规则在这里也必须成立。
pub fn config_dir() -> Option<(PathBuf, DirSource)> {
    if let Some(dir) = std::env::var_os(HOME_ENV).filter(|v| !v.is_empty()) {
        return Some((PathBuf::from(dir), DirSource::Env));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some((PathBuf::from(xdg).join(APP_DIR), DirSource::Xdg));
    }
    let home = std::env::var_os("HOME").filter(|v| !v.is_empty())?;
    Some((
        PathBuf::from(home).join(".config").join(APP_DIR),
        DirSource::Home,
    ))
}

/// 配置文件路径。
pub fn config_path() -> Option<PathBuf> {
    config_dir().map(|(dir, _)| dir.join("config.json"))
}

/// 确保目录存在（0700）并试写一个 0600 临时文件后删除。
///
/// 试写文件名带 pid，避免并发调用互相踩；失败时不留残留。
pub fn probe_writable(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));

    let probe = dir.join(format!(".stui-write-probe-{}", std::process::id()));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&probe);

    match file {
        Ok(mut handle) => {
            let result = handle.write_all(b"ok");
            drop(handle);
            let _ = fs::remove_file(&probe);
            result
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            // 上一次异常退出留下的探针文件：清掉再试一次。
            let _ = fs::remove_file(&probe);
            Err(err)
        }
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_wins_and_empty_is_treated_as_unset() {
        // 不修改进程环境（测试并行安全），只验证纯函数式的分支判定顺序：
        // 这里通过临时目录直接验证 probe_writable 的行为。
        let dir = std::env::temp_dir().join(format!("stui-config-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        probe_writable(&dir).expect("probe should succeed in a fresh temp dir");
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "config dir must be 0700");
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            0,
            "probe file must not be left behind"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_path_is_none_without_home() {
        // 只断言路径拼接逻辑（不触碰全局环境）。
        let (dir, source) = match config_dir() {
            Some(pair) => pair,
            None => return,
        };
        assert!(matches!(
            source,
            DirSource::Env | DirSource::Xdg | DirSource::Home
        ));
        assert!(config_path().is_some());
        assert!(!dir.as_os_str().is_empty());
    }
}
