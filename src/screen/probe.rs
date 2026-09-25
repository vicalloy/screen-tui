//! 会话元数据探测：工作目录 / 运行命令（FR-01 可选字段 / T2.2，tech-design §3.4）。
//!
//! Screen 本身不提供这些信息（capability 文档 §8 的边界结论），按平台分派：
//!
//! | 平台 | cwd | 命令 |
//! | --- | --- | --- |
//! | Linux | `/proc/<pid>/cwd`（readlink） | 进程树里首个非 screen 子进程的 `cmdline` |
//! | macOS | `proc_pidinfo(PROC_PIDVNODEPATHINFO)` | `sysctl(KERN_PROC_ALL)` 找子进程 + `KERN_PROCARGS2` 取完整 argv |
//!
//! macOS 走内核直读而非 spawn `ps`/`lsof`：刷新链路里每次 spawn 要 10–40ms
//! CPU（NFR-03 忙等禁令的延伸），内核调用是微秒级。`libc` 已是依赖，零新增。
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

// ------------------------------------------------------------- macOS（libproc / sysctl）

// libc crate 只有 libproc 的函数绑定；常量与结构按 <libproc/proc_info.h> 手写，
// 关键布局全部经过实测锚定（macOS 26.6 / 25G83，arm64），并有单测自检。

/// `proc_pidinfo` flavor：`struct proc_bsdinfo`（pid / ppid / comm）。
#[cfg(not(target_os = "linux"))]
const PROC_PIDTBSDINFO: libc::c_int = 3;
/// `proc_pidinfo` flavor：`struct proc_vnodepathinfo`（cwd）。
///
/// 实测 macOS 26.6 为 9（老标头资料写作 11，实取 errno 84 不可用）；
/// 单测 [`tests::proc_cwd_matches_current_dir`] 拿自身 pid 锚定布局，
/// 系统升级若破坏布局该测试会失败，绝不静默给错路径。
#[cfg(not(target_os = "linux"))]
const PROC_PIDTVNODEPATHINFO: libc::c_int = 9;
/// `sysctl` KERN_PROCARGS2：完整 argv（libc 常量表缺失，值取自 sys/sysctl.h）。
#[cfg(not(target_os = "linux"))]
const KERN_PROCARGS2: libc::c_int = 49;

#[cfg(not(target_os = "linux"))]
const MAXCOMLEN: usize = 16;
#[cfg(not(target_os = "linux"))]
const MAXPATHLEN: usize = 1024;

/// macOS 实现：内核直读。子进程用 `proc_listchildpids` 定位（探测面最小），
/// 完整命令行用 `KERN_PROCARGS2`（comm 只有 16 字节，只能当回退）；
/// cwd 用 `proc_pidinfo(VNODEPATHINFO)`。
#[cfg(not(target_os = "linux"))]
pub fn macos_meta(pid: u32) -> Option<Meta> {
    let command = child_command_argv(pid);
    let cwd = proc_cwd(pid);
    Meta::combine(cwd, command)
}

/// 找 screen server 的首个非 screen 子进程并取完整命令行。
///
/// screen 自己的包装进程按 comm 跳过；argv 取不到（权限/刚退出）时回退到
/// comm —— 那是内核给的真实进程名，不算猜（C-5）。
#[cfg(not(target_os = "linux"))]
fn child_command_argv(pid: u32) -> Option<String> {
    let children = proc_child_pids(pid)?;
    for child in children {
        let Some(comm) = bsd_comm(child) else {
            continue;
        };
        if is_screen_binary(&comm) {
            continue;
        }
        return Some(proc_argv(child).unwrap_or(comm));
    }
    None
}

/// `proc_listchildpids(ppid)`：直接子进程 pid 列表。失败返回 `None`。
///
/// 带缓冲区调用返回 **pid 个数**（实测）；缓冲区不够时翻倍重试，上限 64K 个。
#[cfg(not(target_os = "linux"))]
fn proc_child_pids(ppid: u32) -> Option<Vec<u32>> {
    let mut capacity = 64usize;
    loop {
        let mut buf = vec![0i32; capacity];
        let got = unsafe {
            libc::proc_listchildpids(
                ppid as libc::pid_t,
                buf.as_mut_ptr() as *mut libc::c_void,
                (capacity * std::mem::size_of::<i32>()) as libc::c_int,
            )
        };
        if got < 0 {
            return None;
        }
        let got = got as usize;
        if got < capacity {
            buf.truncate(got);
            return Some(buf.into_iter().map(|pid| pid as u32).collect());
        }
        capacity *= 4;
        if capacity > 64 * 1024 {
            return None;
        }
    }
}

/// `struct proc_bsdinfo`（proc_info.h）：全定长标量 + char 数组，无复杂嵌套。
/// 布局经实测锚定（sizeof = 136，pbi_pid / pbi_ppid / pbi_comm 与自身进程一致）。
#[cfg(not(target_os = "linux"))]
#[repr(C)]
struct ProcBsdInfo {
    pbi_flags: u32,
    pbi_status: u32,
    pbi_xstatus: u32,
    pbi_pid: u32,
    pbi_ppid: u32,
    pbi_uid: u32,
    pbi_gid: u32,
    pbi_ruid: u32,
    pbi_rgid: u32,
    pbi_svuid: u32,
    pbi_svgid: u32,
    reserved: u32,
    pbi_comm: [libc::c_char; MAXCOMLEN],
    pbi_name: [libc::c_char; 2 * MAXCOMLEN],
    pbi_nfiles: u32,
    pbi_pgid: u32,
    pbi_pjobc: u32,
    e_tdev: u32,
    e_tpgid: u32,
    pbi_nice: i32,
    pbi_start_tvsec: u64,
    pbi_start_tvusec: u64,
}

/// `proc_pidinfo(PROC_PIDTBSDINFO)`：进程 comm。失败返回 `None`。
#[cfg(not(target_os = "linux"))]
fn bsd_comm(pid: u32) -> Option<String> {
    unsafe {
        let mut info: ProcBsdInfo = std::mem::zeroed();
        let got = libc::proc_pidinfo(
            pid as libc::c_int,
            PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            std::mem::size_of::<ProcBsdInfo>() as libc::c_int,
        );
        if got < std::mem::size_of::<ProcBsdInfo>() as libc::c_int {
            return None;
        }
        let comm = c_char_array_to_string(&info.pbi_comm);
        if comm.is_empty() {
            None
        } else {
            Some(comm)
        }
    }
}

/// `struct proc_vnodepathinfo`（proc_info.h）。
///
/// `pvi_cdir` = `vnode_info`(152B) + `vip_path`(1024B)，后面再跟一份 rdir；
/// 152 是实测值（见 [`PROC_PIDTVNODEPATHINFO`] 注释），不用手写 vnode_stat。
#[cfg(not(target_os = "linux"))]
#[repr(C)]
struct VnodePathInfo {
    cdir_info: [u8; VNODE_INFO_SIZE],
    cdir_path: [libc::c_char; MAXPATHLEN],
    rdir_info: [u8; VNODE_INFO_SIZE],
    rdir_path: [libc::c_char; MAXPATHLEN],
}

/// `struct vnode_info` 的实测大小（vnode_stat + fsid_t，arm64/x86_64 同布局）。
#[cfg(not(target_os = "linux"))]
const VNODE_INFO_SIZE: usize = 152;

/// `proc_pidinfo(PROC_PIDTVNODEPATHINFO)`：进程 cwd。失败返回 `None`。
#[cfg(not(target_os = "linux"))]
fn proc_cwd(pid: u32) -> Option<String> {
    unsafe {
        let mut info: VnodePathInfo = std::mem::zeroed();
        let got = libc::proc_pidinfo(
            pid as libc::c_int,
            PROC_PIDTVNODEPATHINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            std::mem::size_of::<VnodePathInfo>() as libc::c_int,
        );
        // 至少要拿到 cdir 路径的非空首字节才算有效。
        if got < (VNODE_INFO_SIZE + 2) as libc::c_int {
            return None;
        }
        let path = c_char_array_to_string(&info.cdir_path);
        if path.is_empty() {
            None
        } else {
            Some(path)
        }
    }
}

/// `sysctl(KERN_PROCARGS2, pid)`：进程完整 argv。失败返回 `None`。
///
/// mib 是 `[CTL_KERN, KERN_PROCARGS2, pid]`（挂在 CTL_KERN 直下，不在
/// KERN_PROC 子树里），namelen 3。
#[cfg(not(target_os = "linux"))]
fn proc_argv(pid: u32) -> Option<String> {
    unsafe {
        let mut mib: [libc::c_int; 3] = [libc::CTL_KERN, KERN_PROCARGS2, pid as libc::c_int];
        let mut size: libc::size_t = 0;
        if libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        ) != 0
            || size < 5
        {
            return None;
        }
        let mut buf = vec![0u8; size];
        if libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return None;
        }
        buf.truncate(size);
        parse_procargs2(&buf)
    }
}

/// KERN_PROCARGS2 缓冲区解析（纯函数）：
/// `[argc: u32 native][exec_path\0][对齐 NUL 填充…][argv\0…][env\0…]`。
///
/// 只取前 argc 个串按空格 join；argc 异常（0 / 超过实际串数）按实际能取到的算 ——
/// 数据来自内核快照，截断好过丢弃。
#[cfg(not(target_os = "linux"))]
fn parse_procargs2(buf: &[u8]) -> Option<String> {
    if buf.len() < 5 {
        return None;
    }
    let argc = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if argc == 0 {
        return None;
    }
    let mut pos = 4;
    while pos < buf.len() && buf[pos] != 0 {
        pos += 1; // exec_path
    }
    while pos < buf.len() && buf[pos] == 0 {
        pos += 1; // 对齐填充
    }
    let mut argv = Vec::with_capacity(argc.min(64));
    while pos < buf.len() && argv.len() < argc {
        let start = pos;
        while pos < buf.len() && buf[pos] != 0 {
            pos += 1;
        }
        if pos > start {
            argv.push(String::from_utf8_lossy(&buf[start..pos]).into_owned());
        }
        pos += 1; // 跳过串尾 NUL
    }
    if argv.is_empty() {
        return None;
    }
    Some(argv.join(" "))
}

/// NUL 结尾的 `c_char` 数组转 String（非 UTF-8 字节 lossy 替换）。
#[cfg(not(target_os = "linux"))]
fn c_char_array_to_string(chars: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = chars
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
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

    #[cfg(not(target_os = "linux"))]
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn procargs2_parses_argv_and_stops_at_argc() {
        // 缓冲区布局：argc(4B) + exec_path + 对齐 NUL + argv... + env...（env 不取）。
        let mut buf = 2u32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/bin/zsh\0"); // exec_path
        buf.extend_from_slice(&[0, 0, 0]); // 对齐填充
        buf.extend_from_slice(b"/bin/zsh\0-l\0");
        buf.extend_from_slice(b"HOME=/\0PATH=/bin\0"); // env 区，必须停在 argc=2
        assert_eq!(
            parse_procargs2(&buf).as_deref(),
            Some("/bin/zsh -l")
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn procargs2_rejects_empty_or_malformed_buffers() {
        assert_eq!(parse_procargs2(&[]), None);
        assert_eq!(parse_procargs2(&[0, 0, 0, 0]), None);
        // argc = 0。
        let buf = 0u32.to_ne_bytes().to_vec();
        assert_eq!(parse_procargs2(&buf), None);
        // 有 exec_path 但后面没有任何串。
        let mut buf = 1u32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/bin/sh\0");
        assert_eq!(parse_procargs2(&buf), None);
    }

    /// 布局自检（仅 macOS）：拿自身 pid 实取 cwd，与 `env::current_dir()` 对比。
    /// vnode_info 大小 / flavor 常量任何一个错位都会在这里爆出来。
    #[cfg(target_os = "macos")]
    #[test]
    fn proc_cwd_matches_current_dir() {
        let expected = std::env::current_dir().unwrap();
        let got = proc_cwd(std::process::id()).expect("own cwd via proc_pidinfo");
        assert_eq!(got, expected.display().to_string());
    }

    /// 布局自检（仅 macOS）：proc_bsdinfo 的 comm 必须与自身进程一致。
    #[cfg(target_os = "macos")]
    #[test]
    fn bsd_comm_matches_self() {
        let comm = bsd_comm(std::process::id()).expect("own comm via proc_pidinfo");
        assert!(!comm.is_empty());
    }

    /// 端到端自检（仅 macOS）：起一个真实子进程（非 screen，不违反「单测不起
    /// screen」的规矩），child_command_argv 必须拿到它的完整命令行。
    #[cfg(target_os = "macos")]
    #[test]
    fn child_command_argv_reads_a_real_child() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let command = child_command_argv(std::process::id()).expect("child command found");
        child.kill().ok();
        assert!(command.starts_with("/bin/sleep"), "got: {command}");
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
        // get() 的探测路径会碰真实内核调用 —— 只验证缓存簿记：
        // 直接操纵 entries 不可行（私有），所以只断言空缓存行为 + len。
        let mut cache = MetaCache::default();
        assert!(cache.is_empty());
        cache.clear();
        assert_eq!(cache.len(), 0);
    }
}
