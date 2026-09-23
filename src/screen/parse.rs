//! `-ls` 解析 + `-q -ls` 退出码两级策略（FR-01 / T0.3）。
//!
//! 全部是纯函数，不碰终端、不 panic；解析不动则返回 [`Error::UnrecognizedOutput`]。
//!
//! # 本机实测修正（2026-09-23，Screen 4.00.03）
//!
//! 写实现时在 macOS 4.00.03 上实测，发现三处与设计文档描述不一致，已按**实测**实现：
//!
//! | 项 | 设计文档描述 | 实测 | 本实现 |
//! | --- | --- | --- | --- |
//! | `-ls` 输出流 | 未声明 | **stdout**（stderr 为空） | 读 stdout |
//! | 行尾 | 未声明 | **`\r\n`** | 解析前统一去 `\r` |
//! | `-q -ls` 空列表退出码 | 9（手册 4.9/5.0） | **8** | 8 视为「不可判定」，退回文本明细 |
//!
//! 第 3 条意味着 `-q -ls` 的 9/10/11+ 表在 4.00.03 上**不成立**，
//! 因此退出码只作快路径，明细一律以 `-ls` 文本为准（T0.6 服务器补测继续跟踪）。

use std::collections::BTreeMap;

use super::{Error, Result, cmd};

/// 会话状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Detached,
    Attached,
    Multi,
    Dead,
    Unreachable,
    /// 未知状态词，**原样保留**（C-5 不猜测）。
    Unknown(String),
}

impl Status {
    /// 白名单映射，大小写不敏感。
    ///
    /// 只取第一个空白分隔的词并去掉尾随逗号，因此真实输出里的
    /// `(Dead ???)`、`(Multi, attached)` 这类扩展写法也能正确归类。
    pub fn from_word(word: &str) -> Status {
        let first = word.split_whitespace().next().unwrap_or("");
        let normalized = first.trim_end_matches(',').to_ascii_lowercase();
        match normalized.as_str() {
            "detached" => Status::Detached,
            "attached" => Status::Attached,
            "multi" => Status::Multi,
            "dead" => Status::Dead,
            "unreachable" => Status::Unreachable,
            _ => Status::Unknown(word.trim().to_string()),
        }
    }

    /// 规范化标签，未知状态原样透出。
    pub fn label(&self) -> String {
        match self {
            Status::Detached => "detached".into(),
            Status::Attached => "attached".into(),
            Status::Multi => "multi".into(),
            Status::Dead => "dead".into(),
            Status::Unreachable => "unreachable".into(),
            Status::Unknown(raw) => raw.clone(),
        }
    }

    /// 能否直接 `screen -r` 接入。
    pub fn is_attachable(&self) -> bool {
        matches!(self, Status::Detached)
    }

    /// 是否允许发起连接（含共享/接管）；dead 与 unreachable 一律拒绝。
    pub fn is_connectable(&self) -> bool {
        matches!(self, Status::Detached | Status::Attached | Status::Multi)
    }
}

/// 一条会话记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    /// `<pid>.<name>` 全名 —— 名字不唯一时唯一可靠的寻址方式。
    pub full: String,
    pub pid: Option<i32>,
    /// 去掉 pid 前缀的会话名。
    pub name: String,
    /// 创建时间原文，4.6+ 才有该列；无则 `None`（FR-01：可选项，无则留空）。
    pub created: Option<String>,
    pub status: Status,
}

/// `-q -ls` 退出码的解释（手册语义，**不保证老版本成立**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outlook {
    NoSessions,
    Unconnectable,
    Available(u32),
    /// 不在手册 9/10/11+ 表内 —— 例如 4.00.03 空列表实测返回 8。
    Inconclusive(i32),
}

impl Outlook {
    pub fn from_code(code: i32) -> Outlook {
        match code {
            9 => Outlook::NoSessions,
            10 => Outlook::Unconnectable,
            c if c >= 11 => Outlook::Available((c - 10) as u32),
            other => Outlook::Inconclusive(other),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Outlook::NoSessions => "no sessions (exit 9)".into(),
            Outlook::Unconnectable => "sessions exist but none connectable (exit 10)".into(),
            // 不含会话数：调用方通常已经单独展示了「N session(s)」，避免读成「3 session(s) · 3 session(s)」。
            Outlook::Available(n) => format!("available (exit {})", n + 10),
            Outlook::Inconclusive(code) => {
                format!("inconclusive (exit {code}, outside the documented 9/10/11+ table)")
            }
        }
    }

    /// 退出码是否给出了可用结论。
    pub fn is_conclusive(&self) -> bool {
        !matches!(self, Outlook::Inconclusive(_))
    }
}

/// `-ls` 文本解析结果。
#[derive(Debug, Clone, Default)]
pub struct SessionList {
    pub sessions: Vec<SessionRecord>,
    /// socket 目录，从尾行提取，**绝不硬编码**（陷阱 #10）。
    pub socket_dir: Option<String>,
    /// 逐行解析过程中的可疑之处，供上层如实展示。
    pub warnings: Vec<String>,
    /// 出现重名的会话名（寻址需用全名）。
    pub duplicates: Vec<String>,
}

impl SessionList {
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// 挑一个可用于能力试探测（`-Q` / hardcopy）的会话。
    pub fn probe_target(&self) -> Option<&SessionRecord> {
        self.sessions.iter().find(|s| s.status.is_connectable())
    }

    /// 按会话名查（重名时取第一条）。
    pub fn by_name(&self, name: &str) -> Option<&SessionRecord> {
        self.sessions.iter().find(|s| s.name == name)
    }
}

/// 两级枚举结果。
#[derive(Debug, Clone)]
pub struct Enumeration {
    /// 第一级：`-q -ls` 退出码。
    pub outlook: Outlook,
    /// 第二级：`-ls` 文本明细，是明细的唯一事实来源。
    pub list: SessionList,
    /// `-ls` 解析彻底失败时的原因（列表退化为空，不 panic）。
    pub list_error: Option<String>,
}

impl Enumeration {
    pub fn count(&self) -> usize {
        self.list.len()
    }

    pub fn has_sessions(&self) -> bool {
        !self.list.is_empty()
    }

    /// 退出码与文本明细之间的交叉核对提示。
    pub fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        match self.outlook {
            Outlook::NoSessions if !self.list.is_empty() => notes.push(format!(
                "`-q -ls` said no sessions but `-ls` listed {}; trusting the listing",
                self.list.len()
            )),
            Outlook::Available(n) if n as usize != self.list.len() => notes.push(format!(
                "`-q -ls` reported {n} session(s) but `-ls` detailed {}",
                self.list.len()
            )),
            Outlook::Inconclusive(code) => notes.push(format!(
                "`-q -ls` exited {code}, outside the documented 9/10/11+ table \
                 (measured 8 on Screen 4.00.03 with no sessions); falling back to `-ls` text"
            )),
            _ => {}
        }
        notes.extend(self.list.warnings.iter().cloned());
        if let Some(err) = &self.list_error {
            notes.push(err.clone());
        }
        notes
    }
}

/// 两级枚举：先取 `-q -ls` 退出码，再解析 `-ls` 明细。
pub fn enumerate() -> Result<Enumeration> {
    let quiet = cmd::run(["-q", "-ls"])?;
    let outlook = Outlook::from_code(quiet.code);

    let listing = cmd::run(["-ls"])?;
    let (list, list_error) = match parse_list_output(&listing.text()) {
        Ok(list) => (list, None),
        Err(err) => (SessionList::default(), Some(err.to_string())),
    };

    Ok(Enumeration {
        outlook,
        list,
        list_error,
    })
}

/// 解析 `screen -ls` 的完整输出。
///
/// 识别三种骨架：表头（单复数）、尾行（`N Socket(s) in <dir>.` 与
/// `No Sockets found in <dir>.`）、以及 `\t<full>\t(<date>)?\t(<state>)` 明细行。
/// 无法识别整段输出时返回 [`Error::UnrecognizedOutput`]，绝不 panic。
pub fn parse_list_output(text: &str) -> Result<SessionList> {
    let mut list = SessionList::default();
    let mut saw_structure = false;
    let mut rejected: Vec<String> = Vec::new();

    for raw_line in text.lines() {
        // 实测：screen 的输出以 `\r\n` 结尾，不去掉 `\r` 会让状态词匹配失败。
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.trim().is_empty() {
            continue;
        }

        let trimmed = line.trim_start();

        if is_header_line(trimmed) {
            saw_structure = true;
            continue;
        }
        if let Some(dir) = parse_socket_summary_line(trimmed) {
            saw_structure = true;
            list.socket_dir = Some(dir);
            continue;
        }

        if trimmed.contains('(') {
            match parse_entry_line(trimmed) {
                Some(record) => {
                    if let Status::Unknown(raw) = &record.status {
                        list.warnings.push(format!(
                            "unknown state `{raw}` on {}; showing it as-is",
                            record.full
                        ));
                    }
                    list.sessions.push(record);
                    saw_structure = true;
                }
                None => rejected.push(summarize(trimmed)),
            }
            continue;
        }

        rejected.push(summarize(trimmed));
    }

    if !saw_structure && !rejected.is_empty() {
        return Err(Error::UnrecognizedOutput(rejected.join(" | ")));
    }

    for line in &rejected {
        list.warnings
            .push(format!("skipped unrecognized line: {line}"));
    }

    finish(&mut list);
    Ok(list)
}

/// 稳定排序 + 重名检测。
fn finish(list: &mut SessionList) {
    // `.screenrc` 的 `sort` 会改变行序，展示顺序不依赖 screen（tech-design §3.2 规则 4）。
    list.sessions
        .sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.full.cmp(&b.full)));

    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for session in &list.sessions {
        *counts.entry(session.name.as_str()).or_insert(0) += 1;
    }
    list.duplicates = counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(name, _)| name.to_string())
        .collect();

    for name in &list.duplicates {
        list.warnings.push(format!(
            "duplicate session name `{name}`; address it by `<pid>.<name>`"
        ));
    }
}

fn is_header_line(line: &str) -> bool {
    line.starts_with("There is a screen on") || line.starts_with("There are screens on")
}

/// 尾行：`1 Socket in <dir>.` / `2 Sockets in <dir>.` / `No Sockets found in <dir>.`
fn parse_socket_summary_line(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix("No Sockets found") {
        return extract_socket_dir(rest);
    }

    let idx = line.find("Socket")?;
    let count = line[..idx].trim();
    if count.is_empty() || !count.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    extract_socket_dir(&line[idx..])
}

fn extract_socket_dir(rest: &str) -> Option<String> {
    let pos = rest.find(" in ")?;
    let tail = rest[pos + 4..].trim();
    // 句末的那个 `.` 是标点，不是路径的一部分（实测 `.../T/.screen.`）。
    let dir = tail.strip_suffix('.').unwrap_or(tail).trim();
    if dir.is_empty() {
        None
    } else {
        Some(dir.to_string())
    }
}

/// 明细行：`<full>` 可选日期 状态。
///
/// 用「首个点号前的分量必须是纯数字 pid」作为准入判据 —— 这条与版本无关，
/// 且能把 `-X` 报错时 dump 的 usage（里面有 `-d (-r) ...`、`-D (-r) ...` 这类行）
/// 干净地挡在门外，避免把选项说明误当会话。
fn parse_entry_line(line: &str) -> Option<SessionRecord> {
    let open = line.find('(')?;
    let head = line[..open].trim();
    let full = head.split_whitespace().next()?;
    let (pid, name) = split_full(full)?;

    let groups = parenthesized_groups(&line[open..]);
    if groups.is_empty() {
        return None;
    }

    let created = groups
        .iter()
        .find(|group| looks_like_date(group))
        .map(|group| group.trim().to_string());
    let state = groups
        .iter()
        .rev()
        .find(|group| !looks_like_date(group))?
        .trim()
        .to_string();

    Some(SessionRecord {
        full: full.to_string(),
        pid: Some(pid),
        name,
        created,
        status: Status::from_word(&state),
    })
}

/// 拆 `<pid>.<name>`；首段非数字或名字为空则判为不可识别。
fn split_full(full: &str) -> Option<(i32, String)> {
    let (pid_text, name) = full.split_once('.')?;
    let pid: i32 = pid_text.parse().ok()?;
    if name.is_empty() {
        return None;
    }
    Some((pid, name.to_string()))
}

fn looks_like_date(group: &str) -> bool {
    group.contains('/') && group.chars().any(|c| c.is_ascii_digit())
}

/// 取出文本里所有顶层括号分组的内容（不带括号本身）。
fn parenthesized_groups(text: &str) -> Vec<&str> {
    let mut groups = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;

    for (idx, ch) in text.char_indices() {
        match ch {
            '(' => {
                if depth == 0 {
                    start = idx + 1;
                }
                depth += 1;
            }
            ')' => {
                if depth > 0 {
                    depth -= 1;
                    if depth == 0 {
                        groups.push(&text[start..idx]);
                    }
                }
            }
            _ => {}
        }
    }
    groups
}

fn summarize(line: &str) -> String {
    const MAX: usize = 60;
    if line.chars().count() <= MAX {
        return line.to_string();
    }
    let head: String = line.chars().take(MAX).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MACOS_SINGLE: &str = include_str!("../../tests/fixtures/ls-400c03-single.txt");
    const WITH_DATE: &str = include_str!("../../tests/fixtures/ls-46-with-date.txt");
    const EMPTY: &str = include_str!("../../tests/fixtures/ls-empty.txt");
    const DEAD_UNREACHABLE: &str = include_str!("../../tests/fixtures/ls-dead-unreachable.txt");
    const MALFORMED: &str = include_str!("../../tests/fixtures/ls-malformed.txt");
    const USAGE_DUMP: &str = include_str!("../../tests/fixtures/ls-usage-dump.txt");

    #[test]
    fn parses_macos_400c03_without_date_column() {
        let list = parse_list_output(MACOS_SINGLE).unwrap();
        assert_eq!(list.len(), 1);
        let s = &list.sessions[0];
        assert_eq!(s.full, "11121.ttys002.MacBook-Pro-3");
        assert_eq!(s.pid, Some(11121));
        assert_eq!(s.name, "ttys002.MacBook-Pro-3");
        assert_eq!(s.status, Status::Detached);
        assert_eq!(s.created, None, "4.00.03 has no date column");
        assert_eq!(
            list.socket_dir.as_deref(),
            Some("/var/folders/vp/hvhc4_b90s92slrx__l_42kw0000gn/T/.screen"),
            "socket dir must come from the tail line, with the sentence period stripped"
        );
        assert!(list.warnings.is_empty(), "warnings: {:?}", list.warnings);
    }

    #[test]
    fn parses_46_with_date_column_and_sorts() {
        let list = parse_list_output(WITH_DATE).unwrap();
        let names: Vec<_> = list.sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["llm", "work", "zebra"], "must be name-sorted");

        let work = list.by_name("work").unwrap();
        assert_eq!(work.created.as_deref(), Some("08/09/2026 10:23:45 AM"));
        assert_eq!(work.status, Status::Detached);

        assert_eq!(list.by_name("llm").unwrap().status, Status::Attached);
        assert_eq!(list.by_name("zebra").unwrap().status, Status::Multi);
        assert_eq!(list.socket_dir.as_deref(), Some("/run/screen/S-huxm"));
    }

    #[test]
    fn parses_empty_list() {
        let list = parse_list_output(EMPTY).unwrap();
        assert!(list.is_empty());
        assert_eq!(
            list.socket_dir.as_deref(),
            Some("/var/folders/vp/hvhc4_b90s92slrx__l_42kw0000gn/T/.screen")
        );
        assert!(list.warnings.is_empty());
    }

    #[test]
    fn parses_dead_and_unreachable_without_guessing() {
        let list = parse_list_output(DEAD_UNREACHABLE).unwrap();
        assert_eq!(list.len(), 4);
        assert_eq!(list.by_name("legacy").unwrap().status, Status::Dead);
        assert_eq!(list.by_name("remote").unwrap().status, Status::Unreachable);
        assert_eq!(list.by_name("phone").unwrap().status, Status::Multi);
        assert_eq!(list.by_name("idle").unwrap().status, Status::Detached);

        assert!(!list.by_name("legacy").unwrap().status.is_connectable());
        assert!(!list.by_name("remote").unwrap().status.is_connectable());
        assert!(list.by_name("phone").unwrap().status.is_connectable());

        // 探针只挑可连接的会话。
        assert_eq!(list.probe_target().unwrap().name, "idle");
    }

    #[test]
    fn tolerates_crlf_line_endings() {
        let crlf = MACOS_SINGLE.replace('\n', "\r\n");
        let list = parse_list_output(&crlf).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list.sessions[0].status, Status::Detached);
        assert_eq!(list.sessions[0].created, None);
    }

    #[test]
    fn malformed_input_yields_partial_result_and_warnings() {
        let list = parse_list_output(MALFORMED).unwrap();
        let long_name = "a".repeat(64);
        let names: Vec<_> = list.sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec![long_name.as_str(), "dup", "dup", "malformed"],
            "long names must survive; duplicates stay, ordered by full name"
        );
        assert_eq!(list.duplicates, vec!["dup"]);
        assert!(
            list.warnings.iter().any(|w| w.contains("duplicate")),
            "warnings: {:?}",
            list.warnings
        );
        assert!(
            list.warnings.iter().any(|w| w.contains("Weird State")),
            "unknown state must be surfaced, warnings: {:?}",
            list.warnings
        );
        assert!(
            list.warnings.iter().any(|w| w.contains("skipped")),
            "rejected lines must be surfaced, warnings: {:?}",
            list.warnings
        );
    }

    #[test]
    fn unknown_state_is_preserved_verbatim() {
        let list = parse_list_output(MALFORMED).unwrap();
        let weird = list.by_name("malformed").unwrap();
        assert_eq!(weird.status, Status::Unknown("Weird State".into()));
        assert_eq!(weird.status.label(), "Weird State");
        assert!(!weird.status.is_connectable());
    }

    #[test]
    fn usage_dump_is_rejected_not_misparsed() {
        let err = parse_list_output(USAGE_DUMP).unwrap_err();
        assert!(matches!(err, Error::UnrecognizedOutput(_)), "{err}");
    }

    #[test]
    fn empty_input_is_not_an_error() {
        assert!(parse_list_output("").unwrap().is_empty());
        assert!(parse_list_output("   \n\r\n").unwrap().is_empty());
    }

    #[test]
    fn outlook_codes_follow_manual_but_expose_the_measured_outlier() {
        assert_eq!(Outlook::from_code(9), Outlook::NoSessions);
        assert_eq!(Outlook::from_code(10), Outlook::Unconnectable);
        assert_eq!(Outlook::from_code(13), Outlook::Available(3));
        // 本机 4.00.03 空列表实测 8：不可判定，必须暴露而不是猜。
        assert_eq!(Outlook::from_code(8), Outlook::Inconclusive(8));
        assert!(!Outlook::from_code(8).is_conclusive());
        assert_eq!(Outlook::from_code(0), Outlook::Inconclusive(0));
    }

    #[test]
    fn state_words_are_normalized_conservatively() {
        assert_eq!(Status::from_word("Detached"), Status::Detached);
        assert_eq!(
            Status::from_word("(detached)"),
            Status::Unknown("(detached)".into())
        );
        assert_eq!(Status::from_word("Dead ???"), Status::Dead);
        assert_eq!(Status::from_word("Multi, attached"), Status::Multi);
        assert_eq!(Status::from_word(""), Status::Unknown(String::new()));
    }
}
