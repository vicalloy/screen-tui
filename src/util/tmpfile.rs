//! 0600 临时文件 + RAII 删除（C-1 / NFR-07 / FR-15 验收 1）。
//!
//! 不引入 `tempfile` crate 的理由见 tech-design §4：这里需要精确控制**权限必须是 0600**
//! 与**删除时机**（成功、失败、panic 三条路径都要删），自绘 40 行比适配通用库更直接。

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// 一个权限 0600 的临时文件，离开作用域即删除。
#[derive(Debug)]
pub struct TempFile {
    path: PathBuf,
}

impl TempFile {
    /// 在临时目录里创建一个唯一命名的空文件，权限 0600。
    ///
    /// 先以 `create_new` 占位再交给外部（通常是 screen 的 hardcopy）写入，好处有二：
    /// 文件名唯一（不会误删别人的文件）、权限由我们决定而不是由 umask 决定。
    pub fn create(prefix: &str) -> io::Result<Self> {
        let dir = temp_dir();
        let mut last_err = None;

        for _ in 0..64 {
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!("{prefix}-{}-{seq}", std::process::id()));
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => {
                    drop(file); // 句柄不留存：后续由 screen 按路径写入
                    // open(2) 的 mode 会被 umask 削，这里显式钉死 0600。
                    let perms = fs::Permissions::from_mode(0o600);
                    if let Err(err) = fs::set_permissions(&path, perms) {
                        let _ = fs::remove_file(&path);
                        return Err(err);
                    }
                    return Ok(Self { path });
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    last_err = Some(err);
                    continue;
                }
                Err(err) => return Err(err),
            }
        }

        Err(last_err.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not create a unique temporary file",
            )
        }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn path_string(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }

    /// 当前文件字节数；不存在或取不到视为 0。
    pub fn bytes(&self) -> u64 {
        fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }

    pub fn read_to_string(&self) -> io::Result<String> {
        fs::read_to_string(&self.path)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// `$TMPDIR`（非空）优先，否则 `/tmp` —— 与 Screen 的 socket 目录假设无关，独立判定。
pub fn temp_dir() -> PathBuf {
    temp_dir_from(std::env::var_os("TMPDIR"))
}

/// [`temp_dir`] 的纯函数内核：**空字符串按未设置处理**。
///
/// 这条规则不是洁癖 —— capability 文档 §6 记录过一次 `SCREENDIR` 被导出为空值后
/// 所有 screen 调用报 `Cannot access` 的事故，同样的判空在这里也必须有。
fn temp_dir_from(value: Option<std::ffi::OsString>) -> PathBuf {
    match value.filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from("/tmp"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_0600_and_removes_on_drop() {
        let path;
        {
            let tmp = TempFile::create("stui-test").expect("create temp file");
            path = tmp.path().to_path_buf();
            assert!(path.exists());

            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "temp file must be 0600, got {mode:o}");
            assert_eq!(tmp.bytes(), 0);
        }
        assert!(!path.exists(), "temp file must be removed on drop");
    }

    #[test]
    fn names_are_unique_within_a_run() {
        let a = TempFile::create("stui-test").unwrap();
        let b = TempFile::create("stui-test").unwrap();
        assert_ne!(a.path(), b.path());
    }

    #[test]
    fn empty_tmpdir_env_falls_back_to_tmp() {
        use std::ffi::OsString;
        assert_eq!(temp_dir_from(None), PathBuf::from("/tmp"));
        assert_eq!(
            temp_dir_from(Some(OsString::from(""))),
            PathBuf::from("/tmp"),
            "an exported-but-empty TMPDIR must be treated as unset"
        );
        assert_eq!(
            temp_dir_from(Some(OsString::from("/var/tmp"))),
            PathBuf::from("/var/tmp")
        );
    }
}
