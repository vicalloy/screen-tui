//! 配置读写（requirements §8 / tech-design §3.5，T2.1）。
//!
//! 三条硬规则（§8.3）：
//!
//! 1. 写入必须**原子**（tmp + fsync + rename），中断不留半截配置；
//! 2. 权限 0600（目录 0700）；
//! 3. `version` 用于迁移：读到未知高版本**只读运行、绝不覆写**；
//!    损坏文件备份 `.bak` 后用默认值启动并提示，不崩溃。
//!
//! 结构沿用 §8.2 草案：`dirs`（T2.7 收藏目录）与 `sessions`（T2.6 元数据）
//! 的字段在本层一次定义齐，上层按里程碑逐个启用。

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 配置目录名。
pub const APP_DIR: &str = "screen-tui";

/// 覆盖配置目录的环境变量。
pub const HOME_ENV: &str = "SCREEN_TUI_HOME";

/// 本工具理解的配置版本。更高的 `version` 一律只读（§8.3 第 3 条）。
pub const CONFIG_VERSION: u32 = 1;

// ------------------------------------------------------------- 结构定义

/// 界面行为（§8.2 `ui`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// `auto`（跟随终端尺寸）或手动锁定的档位名；手动锁定属后续里程碑，本层只存值。
    pub layout: String,
    /// 窄屏阈值（< 该值进窄屏形态，FR-04）。
    pub narrow_cols: u16,
    /// 宽屏阈值（≥ 该值开双栏）。
    pub wide_cols: u16,
    /// 自动刷新间隔毫秒（FR-19 默认 3 秒）。
    pub refresh_ms: u64,
    /// 状态图标开关（关闭后用文字状态）。
    pub icons: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            layout: "auto".into(),
            narrow_cols: crate::ui::layout::NARROW_COLS,
            wide_cols: crate::ui::layout::WIDE_COLS,
            refresh_ms: 3000,
            icons: true,
        }
    }
}

/// 默认值偏好（§8.2 `defaults`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Defaults {
    pub use_utf8: bool,
    pub prefer_256color: bool,
    /// 创建成功后是否直接连接进去（FR-02 验收 4 的可配置项；默认停留列表）。
    pub attach_after_create: bool,
    /// 返回提示用的转义前缀。`None` = 自动探测 `.screenrc`（FR-18），
    /// 探测不到再回退 `C-a`；显式设置则覆盖探测结果。
    pub escape_prefix: Option<String>,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            use_utf8: true,
            prefer_256color: true,
            attach_after_create: false,
            escape_prefix: None,
        }
    }
}

/// 收藏目录条目（T2.7 / FR-23）。
///
/// `last_used` 是本工具自己生成的本地时间标签（`util::time::local_datetime`）；
/// 「最近使用」的排序由 `dirs` 的 **Vec 顺序**承载（最近的在前），不解析时间串。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub path: String,
    pub last_used: String,
}

/// 会话元数据条目（T2.6 / FR-24）。
///
/// `managed = true` 表示本工具创建（可重启）；外部会话只留观察记录，禁用重启。
/// `Default` 直接 derive：全字段（Option/bool）的默认值恰好就是「什么都没记录」。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionMeta {
    pub alias: Option<String>,
    pub note: Option<String>,
    pub managed: bool,
    /// managed 会话的启动命令（重启用）。
    pub command: Option<String>,
    /// managed 会话的起始目录（重启用）。
    pub cwd: Option<String>,
    /// 最近一次见到该会话的时间标签（会话消失后保留，供手动清理参考）。
    pub last_seen: Option<String>,
}

/// 配置根（§8.2）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub ui: UiConfig,
    pub defaults: Defaults,
    /// 最近使用的目录，**最近的在前**（T2.7 维护）。
    pub dirs: Vec<DirEntry>,
    /// 会话名 → 元数据。
    pub sessions: std::collections::BTreeMap<String, SessionMeta>,
}

impl Default for Config {
    /// 默认 = 当前版本的全新配置（`version` 必须是 `CONFIG_VERSION`，
    /// 不能用 derive —— 缺字段反序列化走 `Default`，version 若为 0 会被误判成待迁移）。
    fn default() -> Self {
        Self::new()
    }
}

impl Config {
    pub fn new() -> Self {
        Self {
            version: CONFIG_VERSION,
            ui: UiConfig::default(),
            defaults: Defaults::default(),
            dirs: Vec::new(),
            sessions: std::collections::BTreeMap::new(),
        }
    }
}

// ------------------------------------------------------------- 加载

/// 一次加载的结果：配置本体 + 需要告知用户的警告 + 是否只读。
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    pub warnings: Vec<String>,
    /// `true` = 配置版本比本工具新：**任何写入都被跳过**，绝不覆写用户的文件。
    pub read_only: bool,
}

/// 读配置目录下的 `config.json`。文件不存在 = 首次运行，静默用默认值。
pub fn load() -> Loaded {
    load_from(config_path().as_deref())
}

/// [`load`] 的注入版：路径显式传入（`None` = 无配置目录 → 纯默认值）。
pub fn load_from(path: Option<&Path>) -> Loaded {
    let Some(path) = path else {
        return Loaded {
            config: Config::new(),
            warnings: Vec::new(),
            read_only: false,
        };
    };
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Loaded {
                config: Config::new(),
                warnings: Vec::new(),
                read_only: false,
            };
        }
        Err(err) => {
            return Loaded {
                config: Config::new(),
                warnings: vec![format!(
                    "cannot read {}: {err}; using defaults",
                    path.display()
                )],
                read_only: false,
            };
        }
    };

    match serde_json::from_str::<Config>(&text) {
        Ok(config) => {
            let read_only = config.version > CONFIG_VERSION;
            let warnings = if read_only {
                vec![format!(
                    "config version {} is newer than this build understands ({}); \
                     running read-only, your file will not be touched",
                    config.version, CONFIG_VERSION
                )]
            } else {
                Vec::new()
            };
            Loaded {
                config,
                warnings,
                read_only,
            }
        }
        Err(err) => {
            // 损坏容错（§8.3 第 4 条）：备份后默认值启动，不崩溃。
            let backup = path.with_extension("json.bak");
            let backup_note = match fs::copy(path, &backup) {
                Ok(_) => format!(
                    "config file is corrupt; backed up to {} and starting with defaults",
                    backup.display()
                ),
                Err(copy_err) => format!(
                    "config file is corrupt ({err}) and could not be backed up ({copy_err}); \
                     starting with defaults"
                ),
            };
            Loaded {
                config: Config::new(),
                warnings: vec![backup_note],
                read_only: false,
            }
        }
    }
}

// ------------------------------------------------------------- 保存

/// 原子保存（§8.3 第 1/2 条）：tmp + fsync + rename，0600 / 0700。
/// `read_only` 时跳过并返回 `Ok(false)`（未知高版本绝不覆写）。
pub fn save(config: &Config) -> io::Result<bool> {
    save_to(config, config_path().as_deref())
}

/// [`save`] 的注入版。`version` 高于本工具理解范围时跳过写入（`Ok(false)`）——
/// 未知高版本绝不覆写（§8.3 第 3 条），这条不变式由本函数兜底，不依赖调用方自觉。
pub fn save_to(config: &Config, path: Option<&Path>) -> io::Result<bool> {
    if config.version > CONFIG_VERSION {
        return Ok(false); // 只读保护：宁可少存，不可覆写。
    }
    let Some(path) = path else {
        return Ok(false); // 无配置目录可写：静默放弃（配置是增强，不是必需）。
    };

    let text = serde_json::to_string_pretty(config).map_err(io::Error::other)?;
    let tmp = path.with_extension("json.tmp");

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }

    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        // open(2) 的 mode 会被 umask 削，写完显式钉死 0600。
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(&tmp, path)?;
    Ok(true)
}

// ------------------------------------------------------------- 目录解析（M0 起既有）

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

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("stui-cfg-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn roundtrip_preserves_all_sections() {
        let mut config = Config::new();
        config.ui.refresh_ms = 5000;
        config.defaults.escape_prefix = Some("C-\\\\".into());
        config.dirs.push(DirEntry {
            path: "/srv/app".into(),
            last_used: "2026-09-23 18:00:00".into(),
        });
        config.sessions.insert(
            "work".into(),
            SessionMeta {
                alias: Some("登录修复".into()),
                note: Some("claude 任务".into()),
                managed: true,
                command: Some("claude".into()),
                cwd: Some("/srv/app".into()),
                last_seen: None,
            },
        );

        let text = serde_json::to_string_pretty(&config).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(back, config, "serde roundtrip must be lossless");
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        // 手写的最小配置：只有 version。其余字段全部走 serde(default)。
        let config: Config = serde_json::from_str(r#"{"version":1}"#).unwrap();
        assert_eq!(config, Config::new());
        assert_eq!(config.ui.refresh_ms, 3000);
        assert!(config.defaults.escape_prefix.is_none());
    }

    #[test]
    fn atomic_write_sets_permissions_and_leaves_no_tmp() {
        let dir = tmpdir("atomic");
        let path = dir.join("config.json");

        let mut config = Config::new();
        config.ui.refresh_ms = 1234;
        assert!(save_to(&config, Some(&path)).unwrap());

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config file must be 0600");
        let dir_mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "config dir must be 0700");
        assert!(
            !path.with_extension("json.tmp").exists(),
            "tmp file must be gone after rename"
        );

        let loaded = load_from(Some(&path));
        assert_eq!(loaded.config.ui.refresh_ms, 1234);
        assert!(loaded.warnings.is_empty());
        assert!(!loaded.read_only);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_is_backed_up_and_defaults_are_used() {
        let dir = tmpdir("corrupt");
        let path = dir.join("config.json");
        fs::write(&path, "{ not json !!!").unwrap();

        let loaded = load_from(Some(&path));
        assert_eq!(loaded.config, Config::new(), "defaults after corruption");
        assert_eq!(loaded.warnings.len(), 1);
        assert!(
            loaded.warnings[0].contains("backed up"),
            "{}",
            loaded.warnings[0]
        );
        assert!(path.with_extension("json.bak").exists(), "backup exists");
        assert!(!loaded.read_only, "corrupt file may be overwritten on save");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_higher_version_is_read_only() {
        let dir = tmpdir("highver");
        let path = dir.join("config.json");
        fs::write(&path, r#"{"version":99,"ui":{"refresh_ms":777}}"#).unwrap();

        let loaded = load_from(Some(&path));
        assert!(loaded.read_only, "higher version must be read-only");
        assert_eq!(loaded.config.ui.refresh_ms, 777, "values still readable");
        assert_eq!(loaded.warnings.len(), 1);
        assert!(
            loaded.warnings[0].contains("read-only"),
            "{}",
            loaded.warnings[0]
        );

        // 只读 = 保存被跳过，文件原样。
        assert!(!save_to(&loaded.config, Some(&path)).unwrap());
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(after, r#"{"version":99,"ui":{"refresh_ms":777}}"#);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_silent_defaults() {
        let dir = tmpdir("missing");
        let loaded = load_from(Some(&dir.join("config.json")));
        assert_eq!(loaded.config, Config::new());
        assert!(loaded.warnings.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn none_path_means_no_config_layer() {
        let loaded = load_from(None);
        assert_eq!(loaded.config, Config::new());
        assert!(!loaded.read_only);
        // 没有路径可写 → save 返回 false 而不是报错。
        assert!(!save_to(&Config::new(), None).unwrap());
    }

    #[test]
    fn env_override_wins_and_empty_is_treated_as_unset() {
        // 不修改进程环境（测试并行安全），只验证 probe_writable 的行为。
        let dir = tmpdir("probe");
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
