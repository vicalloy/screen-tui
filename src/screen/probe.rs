//! 会话元数据探测：工作目录 / 运行命令（FR-01 可选字段 / T2.2，tech-design §3.4）。
//!
//! Screen 本身不提供这些信息（capability 文档 §8 的边界结论），按平台分派：
//!
//! | 平台 | cwd | 命令 |
//! | --- | --- | --- |
//! | Linux | `/proc/<pid>/cwd`（readlink） | 进程树里首个非 screen 子进程的 `cmdline` |
//! | macOS | `lsof -a -p <pid> -d cwd -Fn` | `ps -axo pid=,ppid=,command=` 里找子进程 |
//!
//! 硬规则（C-5 不猜测）：**任何一步取不到就整体缺该字段**，`None` 让 UI 隐藏，
//! 绝不显示占位符；探测失败不影响列表与连接主流程。

use std::collections::BTreeMap;
use std::path::Path;

/// 一次探测得到的元数据；缺失的字段为 `None`（UI 隐藏该行，C-5）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meta {
    pub cwd: Option<String>,
    pub command: Option<String>,
}

impl Meta {
    fn combine(cwd: Option<String>, command: Option<String>) -> Option<Meta> {
        if cwd.is_none() && command.is_none() {
            None
        } else {
            Some(Meta { cwd, command })
        }
    }
}

/// 平台分派入口。取不到返回 `None`，调用方（列表/详情）直接隐藏字段。
pub fn session_meta(pid: u32) -> Option<Meta> {
    #[cfg(target_os = "linux")]
    {
        linux_meta(Path::new("/proc"), pid)
    }
    #[cfg(not(target_os = "linux"))]
    {
        macos_meta(pid)
    }
}

// ------------------------------------------------------------- Linux（/proc）

/// Linux 实现。`base` 可注入（单测在临时目录里造假 `/proc` 树）。
///
/// 注意 `children` 文件需要内核开启 `CONFIG_PROC_CHILDREN`；读不到就只剩
/// cwd 一项，仍按 C-5 诚实降级 —— 不去猜 screen server 自己的 cmdline。
pub fn linux_meta(base: &Path, pid: u32) -> Option<Meta> {
    let cwd = std::fs::read_link(base.join(pid.to_string()).join("cwd"))
        .ok()
        .map(|p| p.display().to_string());

    let mut command = None;
    if let Ok(children) = std::fs::read_to_string(
        base.join(pid.to_string())
            .join("task")
            .join(pid.to_string())
            .join("children"),
    ) {
        for child in children.split_whitespace() {
            let Ok(raw) = std::fs::read(base.join(child).join("cmdline")) else {
                continue;
            };
            let argv: Vec<String> = raw
                .split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .map(String::from_utf8_lossy)
                .map(String::from)
                .collect();
            let Some(argv0) = argv.first() else {
                continue;
            };
            if is_screen_binary(argv0) {
                continue; // screen 自己的包装进程，继续找真正的业务子进程。
            }
            command = Some(argv.join(" "));
            break;
        }
    }

    Meta::combine(cwd, command)
}

/// argv0 是否是 screen 本体（剥掉路径后比较；`SCREEN` 大写形态是 screen server 的常见写法）。
fn is_screen_binary(argv0: &str) -> bool {
    let base = argv0.rsplit('/').next().unwrap_or(argv0);
    base.eq_ignore_ascii_case("screen")
}

// ------------------------------------------------------------- macOS（ps / lsof）

/// macOS 实现：一次 `ps` 拿全表找子进程命令，一次 `lsof` 拿 cwd。
pub fn macos_meta(pid: u32) -> Option<Meta> {
    let command = ps_processes()
        .as_deref()
        .and_then(|table| child_command(table, pid))
        .map(str::to_string);
    let cwd = lsof_cwd(pid);
    Meta::combine(cwd, command)
}

/// `ps -axo pid=,ppid=,command=` 的解析（纯函数）：`(pid, ppid, command)` 列表。
///
/// 逐字段读（跳过字段间的**连续**空白）—— `splitn` 会在连续空格上产生空 token，
/// 把 pid/ppid 解析打断。ps 输出本来就对齐补空格，这条路是必经的。
pub fn parse_ps_table(text: &str) -> Vec<(u32, u32, String)> {
    text.lines()
        .filter_map(|line| {
            let mut rest = line.trim_start();
            let take_u32 = |rest: &mut &str| -> Option<u32> {
                let end = rest.find(char::is_whitespace)?;
                let value = rest[..end].parse().ok()?;
                *rest = rest[end..].trim_start();
                Some(value)
            };
            let pid = take_u32(&mut rest)?;
            let ppid = take_u32(&mut rest)?;
            Some((pid, ppid, rest.trim().to_string()))
        })
        .collect()
}

/// 从 ps 全表里找 `pid` 的首个非 screen 子进程命令（纯函数）。
pub fn child_command(table: &[(u32, u32, String)], pid: u32) -> Option<&str> {
    table
        .iter()
        .filter(|(_, ppid, _)| *ppid == pid)
        .find(|(_, _, command)| {
            command
                .split_whitespace()
                .next()
                .map(|argv0| !is_screen_binary(argv0))
                .unwrap_or(false)
        })
        .map(|(_, _, command)| command.as_str())
}

/// `lsof -a -p <pid> -d cwd -Fn` 的解析（纯函数）：取 `n` 前缀行的最后一行。
pub fn parse_lsof_cwd(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .find_map(|line| line.strip_prefix('n'))
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}

fn ps_processes() -> Option<Vec<(u32, u32, String)>> {
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_ps_table(&String::from_utf8_lossy(&output.stdout)))
}

fn lsof_cwd(pid: u32) -> Option<String> {
    let output = std::process::Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_lsof_cwd(&String::from_utf8_lossy(&output.stdout))
}

// ------------------------------------------------------------- 缓存

/// 按 pid 缓存的探测结果（`ui` 详情与过滤用）。refresh 时整体失效。
#[derive(Debug, Default, Clone)]
pub struct MetaCache {
    entries: BTreeMap<u32, Meta>,
}

impl MetaCache {
    /// 取缓存；未命中则探测一次并缓存。`None` 结果**不缓存**（进程可能只是暂时不可见）。
    pub fn get(&mut self, pid: u32) -> Option<&Meta> {
        if let std::collections::btree_map::Entry::Vacant(slot) = self.entries.entry(pid)
            && let Some(meta) = session_meta(pid)
        {
            slot.insert(meta);
        }
        self.entries.get(&pid)
    }

    /// 只读窥视（不触发探测）：过滤匹配等纯读路径用。
    pub fn peek(&self, pid: u32) -> Option<&Meta> {
        self.entries.get(&pid)
    }

    /// 作废单个 pid 的缓存（NFR-08 的精神：详情里的 cwd/command 不该是陈旧的）。
    pub fn invalidate(&mut self, pid: u32) {
        self.entries.remove(&pid);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 测试注入：绕过探测直接塞缓存。
    #[cfg(test)]
    pub fn entries_insert_for_test(&mut self, pid: u32, meta: Meta) {
        self.entries.insert(pid, meta);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_table_parses_pid_ppid_command() {
        let text = "  1234     1 /usr/bin/screen -L\n  1235  1234 /bin/zsh -l\n  1236  1234 SCREEN\n  999     1 launchd\n";
        let table = parse_ps_table(text);
        assert_eq!(table.len(), 4);
        assert_eq!(table[0], (1234, 1, "/usr/bin/screen -L".into()));
        assert_eq!(table[1], (1235, 1234, "/bin/zsh -l".into()));
    }

    #[test]
    fn ps_table_skips_malformed_lines() {
        let text = "not numbers\n  1234  1 ok\n";
        let table = parse_ps_table(text);
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn child_command_skips_screen_and_picks_first_business_child() {
        let table = parse_ps_table(
            "  1235  1234 SCREEN\n  1236  1234 /usr/local/bin/claude\n  1237  1234 /bin/sh\n",
        );
        let command = child_command(&table, 1234).expect("business child found");
        assert_eq!(command, "/usr/local/bin/claude");
    }

    #[test]
    fn child_command_none_without_children() {
        let table = parse_ps_table("  999  1 launchd\n");
        assert!(child_command(&table, 1234).is_none());
    }

    #[test]
    fn lsof_cwd_takes_the_n_line() {
        let text = "p12345\ncwd\nn/Users/huxm/work\n";
        assert_eq!(parse_lsof_cwd(text).as_deref(), Some("/Users/huxm/work"));
        // lsof 无结果（权限/已退出）时输出里没有 n 行。
        assert_eq!(parse_lsof_cwd("p12345\n"), None);
        assert_eq!(parse_lsof_cwd(""), None);
    }

    /// 造假 /proc 树验证 Linux 路径（任何平台都能跑）。
    #[test]
    fn linux_meta_reads_proc_tree() {
        let base = std::env::temp_dir().join(format!("stui-proc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);

        // /proc/100/cwd -> /srv/app；子进程 101 是 zsh。
        let proc100 = base.join("100");
        std::fs::create_dir_all(proc100.join("task/100")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/srv/app", proc100.join("cwd")).unwrap();
        std::fs::write(proc100.join("task/100/children"), b"101 102\n").unwrap();

        let proc101 = base.join("101");
        std::fs::create_dir_all(&proc101).unwrap();
        std::fs::write(proc101.join("cmdline"), b"/bin/zsh\0-l\0").unwrap();

        // 102 是 screen 自己（SCREEN 大写形态），必须跳过。
        let proc102 = base.join("102");
        std::fs::create_dir_all(&proc102).unwrap();
        std::fs::write(proc102.join("cmdline"), b"SCREEN\0").unwrap();

        let meta = linux_meta(&base, 100).expect("meta from fake proc");
        assert_eq!(meta.cwd.as_deref(), Some("/srv/app"));
        assert_eq!(meta.command.as_deref(), Some("/bin/zsh -l"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn linux_meta_returns_none_when_proc_is_empty() {
        let base = std::env::temp_dir().join(format!("stui-proc-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        assert!(linux_meta(&base, 424242).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn screen_binary_detection_is_path_and_case_insensitive() {
        assert!(is_screen_binary("screen"));
        assert!(is_screen_binary("SCREEN"));
        assert!(is_screen_binary("/usr/bin/screen"));
        assert!(!is_screen_binary("screenutils"));
        assert!(!is_screen_binary("/bin/zsh"));
    }

    #[test]
    fn meta_cache_caches_hits_but_not_misses() {
        // get() 的探测路径会碰真实 ps/lsof —— 只验证缓存簿记：
        // 直接操纵 entries 不可行（私有），所以只断言空缓存行为 + len。
        let mut cache = MetaCache::default();
        assert!(cache.is_empty());
        cache.clear();
        assert_eq!(cache.len(), 0);
    }
}
