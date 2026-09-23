//! 能力探测 —— `screen -v` 版本解析 + `-Q` / `hardcopy` 实探测 → [`Caps`]。
//!
//! 两条硬规则：
//!
//! 1. **代码里不出现版本号字面量比较**（tech-design §2 原则 2）。版本号只用于展示与
//!    doctor 说明；所有功能开关一律来自实跑探测或显式配置。
//! 2. **「不支持」与「不知道」必须分开**（C-5 不猜测）。因此探测结果是三态
//!    [`Support`]，而不是 `bool`：没有会话可供试跑时只能是 `Unknown`。
//!
//! 关于 `hardcopy` 探测：它需要为一个**已存在的会话**真实落盘一次（`design/tech-design.md`
//! §5 明确 doctor 是唯一有副作用的检查）。因此 [`Caps::detect`] 不主动做它，
//! 而是由调用方在需要时调 [`probe_hardcopy`] 后经 [`Caps::apply_hardcopy_probe`] 回填。

use std::path::PathBuf;

use super::{Result, cmd};
use crate::util::tmpfile::TempFile;

/// 版本号。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// 能力三态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// 实测可用。
    Yes,
    /// 实测不可用（例如老版本报 `Unknown option -Q`）。
    No,
    /// 探测条件不具备（例如无会话可试跑），**不等于不支持**。
    Unknown,
}

impl Support {
    pub fn label(self) -> &'static str {
        match self {
            Support::Yes => "yes",
            Support::No => "no",
            Support::Unknown => "unknown",
        }
    }

    /// 是否可以被当作「可以用」使用 —— `Unknown` 一律按不可用处理（保守降级）。
    pub fn usable(self) -> bool {
        matches!(self, Support::Yes)
    }
}

/// 启动时探测一次的能力对象，此后全局只读（tech-design §2 原则 2）。
#[derive(Debug, Clone)]
pub struct Caps {
    pub program: PathBuf,
    /// `screen -v` 的首行原文，用于 doctor 展示。
    pub version_line: String,
    pub version: Option<Version>,
    /// 匹配到的版本号 token 原文（保留 `4.00.03` 这种前导零写法）。
    pub version_text: Option<String>,
    /// `-Q` 远程查询可用性（4.6+）。
    pub query: Support,
    /// `-X hardcopy` 可用性。
    pub hardcopy: Support,
    /// `-X hardcopy -h`（含回滚缓冲）可用性。
    pub hardcopy_history: Support,
    /// SGR 1006 鼠标。
    ///
    /// **M0 恒为 `false`**：鼠标需要 4.7+，但按原则 1 不允许做版本号比较，而鼠标没有
    /// 外部探测手段。故留作 M3 的显式 opt-in 开关（FR-37 本身也仅列为桌面可选），
    /// 不由版本号推导 —— 宁可功能关着，也不伪造一个「我猜它支持」的结论。
    pub mouse_sgr: bool,
    /// 返回提示用的转义前缀。M0 恒为默认值，`.screenrc` 探测属 FR-18（M2）。
    pub escape_prefix: String,
    /// 探测过程中的说明与原因，doctor 直接转述。
    pub probe_notes: Vec<String>,
}

impl Default for Caps {
    /// 「什么都没探测过」的中性默认值：全部能力 `Unknown`、无版本信息。
    ///
    /// 只用于单测构造与「探测不可用但需要继续降级运行」的场景；
    /// 正常入口一律走 [`Caps::detect`]。
    fn default() -> Self {
        Self {
            program: PathBuf::from("screen"),
            version_line: String::new(),
            version: None,
            version_text: None,
            query: Support::Unknown,
            hardcopy: Support::Unknown,
            hardcopy_history: Support::Unknown,
            mouse_sgr: false,
            escape_prefix: "C-a".to_string(),
            probe_notes: Vec::new(),
        }
    }
}

impl Caps {
    /// 探测版本，并在 `target` 可用时试跑一次 `-Q` 确认查询能力。
    ///
    /// 注意 `screen -v` **即使成功也返回退出码 1**（本机 4.00.03 实测），
    /// 因此这里只看文本、不看退出码。
    pub fn detect(target: Option<&str>) -> Result<Caps> {
        let program = cmd::program()?;
        let version_run = cmd::run(["-v"])?;
        let version_line = version_run
            .stdout
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        let version = parse_version(&version_run.text());
        let version_text = find_version_token(&version_run.text());

        let mut caps = Caps {
            program,
            version_line,
            version,
            version_text,
            query: Support::Unknown,
            hardcopy: Support::Unknown,
            hardcopy_history: Support::Unknown,
            mouse_sgr: false,
            escape_prefix: "C-a".to_string(),
            probe_notes: Vec::new(),
        };

        match version {
            Some(v) => caps
                .probe_notes
                .push(format!("parsed version {v} from `screen -v`")),
            None => caps.probe_notes.push(
                "could not parse a version number from `screen -v` output; \
                 capability flags fall back to probes"
                    .to_string(),
            ),
        }

        match target {
            Some(full) => caps.probe_query(full)?,
            None => caps
                .probe_notes
                .push("no session available to test `-Q`, query support left unknown".to_string()),
        }

        Ok(caps)
    }

    /// 对一个会话试跑 `screen -Q windows`，据结果判定 `query`。
    fn probe_query(&mut self, full: &str) -> Result<()> {
        let run = cmd::run(["-S", full, "-Q", "windows"])?;
        self.query = classify_query(&run);
        self.probe_notes.push(format!(
            "`-Q windows` on {full}: exit {} → query={}",
            run.code,
            self.query.label()
        ));
        Ok(())
    }

    /// 回填 [`probe_hardcopy`] 的结果。
    pub fn apply_hardcopy_probe(&mut self, probe: &HardcopyProbe) {
        self.hardcopy = probe.hardcopy;
        self.hardcopy_history = probe.history;
        self.probe_notes.push(probe.detail.clone());
    }

    /// 供界面展示的版本字符串。
    pub fn version_display(&self) -> &str {
        self.version_text.as_deref().unwrap_or("unknown")
    }
}

/// `hardcopy` 试探测结果。
#[derive(Debug, Clone)]
pub struct HardcopyProbe {
    pub hardcopy: Support,
    pub history: Support,
    /// 人话说明，doctor 直接打印原因。
    pub detail: String,
}

/// 对一个会话做一次真实 hardcopy 试写（**有副作用**：临时文件落盘后立即删除）。
///
/// 这是 `doctor` 第 11 项与后续预览功能的共同底座。临时文件 0600 且由 RAII 保证删除，
/// 成功/失败/panic 三条路径都不留残留。
pub fn probe_hardcopy(full: &str) -> Result<HardcopyProbe> {
    let plain = hardcopy_once(full, false)?;
    let history = hardcopy_once(full, true)?;

    let (hardcopy, detail) = match plain {
        HardcopyOutcome::Wrote(bytes) => (Support::Yes, format!("hardcopy wrote {bytes} bytes")),
        HardcopyOutcome::Unsupported(text) => (
            Support::No,
            format!("hardcopy reported unsupported by this screen build: {text}"),
        ),
        HardcopyOutcome::Inconclusive(text) => (
            Support::Unknown,
            format!("hardcopy could not be verified: {text}"),
        ),
    };

    let history_support = match history {
        HardcopyOutcome::Wrote(_) => Support::Yes,
        HardcopyOutcome::Unsupported(_) => Support::No,
        HardcopyOutcome::Inconclusive(_) => Support::Unknown,
    };

    Ok(HardcopyProbe {
        hardcopy,
        history: history_support,
        detail,
    })
}

enum HardcopyOutcome {
    Wrote(u64),
    Unsupported(String),
    Inconclusive(String),
}

fn hardcopy_once(full: &str, history: bool) -> Result<HardcopyOutcome> {
    let tmp = TempFile::create("screen-tui-probe")?;
    let path = tmp.path().to_string_lossy().into_owned();

    let mut args = vec!["-S".to_string(), full.to_string(), "-X".to_string()];
    args.push("hardcopy".to_string());
    if history {
        args.push("-h".to_string());
    }
    args.push(path);

    let run = cmd::run(&args)?;
    let written = std::fs::metadata(tmp.path()).map(|m| m.len()).unwrap_or(0);

    // 临时文件在 `tmp` 离开作用域时无条件删除（含下面提前 return 的路径）。
    if written > 0 {
        return Ok(HardcopyOutcome::Wrote(written));
    }
    let text = first_meaningful_line(&run.text());
    if looks_like_usage_dump(&run.text()) || run.text().contains("Unknown option") {
        return Ok(HardcopyOutcome::Unsupported(text));
    }
    Ok(HardcopyOutcome::Inconclusive(if text.is_empty() {
        format!("no output, exit {}", run.code)
    } else {
        text
    }))
}

/// 判定 `-Q` 探测结果。
///
/// 判据来自本机实测：4.00.03 报 `Error: Unknown option -Q` 并把 usage 打到 **stdout**。
pub fn classify_query(run: &crate::screen::cmd::Run) -> Support {
    let text = run.text();
    if text.contains("Unknown option") || looks_like_usage_dump(&text) {
        return Support::No;
    }
    if text.contains("No screen session found") || text.contains("No Sockets found") {
        return Support::Unknown;
    }
    if run.success() {
        return Support::Yes;
    }
    Support::Unknown
}

fn looks_like_usage_dump(text: &str) -> bool {
    let head: String = text.lines().take(3).collect::<Vec<_>>().join("\n");
    head.contains("Use: screen") || head.contains("or: screen -r")
}

fn first_meaningful_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// 从 `screen -v` 输出里解析版本号。
pub fn parse_version(text: &str) -> Option<Version> {
    find_version_token(text).and_then(|t| parse_version_token(&t))
}

/// 找到版本号 token 原文（保留前导零，如 `4.00.03`）。
///
/// 优先取 `version` 一词之后紧跟的 token，避免误采纳输出里其它位置的数字串；
/// 取不到再退回全文扫描。
pub fn find_version_token(text: &str) -> Option<String> {
    let mut tokens: Vec<&str> = Vec::new();
    for line in text.lines() {
        for token in line.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',') {
            if !token.is_empty() {
                tokens.push(token);
            }
        }
    }

    for (i, token) in tokens.iter().enumerate() {
        if token.eq_ignore_ascii_case("version")
            && let Some(next) = tokens.get(i + 1)
            && parse_version_token(next).is_some()
        {
            return Some((*next).to_string());
        }
    }

    tokens
        .iter()
        .find(|t| parse_version_token(t).is_some())
        .map(|t| (*t).to_string())
}

/// 解析形如 `4.00.03` / `4.6.1` / `5.0.2` 的单个 token；`4.0.3.1` 这类多段串拒绝。
pub fn parse_version_token(token: &str) -> Option<Version> {
    let mut parts = token.split('.');
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts.next()?.parse::<u32>().ok()?;
    let patch = match parts.next() {
        Some(p) => p.parse::<u32>().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some(Version {
        major,
        minor,
        patch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const V_400C03: &str = include_str!("../../tests/fixtures/version-400c03.txt");
    const V_40602: &str = include_str!("../../tests/fixtures/version-40602.txt");
    const V_502: &str = include_str!("../../tests/fixtures/version-502.txt");
    const V_NONSTANDARD: &str = include_str!("../../tests/fixtures/version-nonstandard.txt");

    #[test]
    fn version_parsing_covers_four_samples() {
        // fixture 是 `screen -v` 的原文样本（4.00.03 那份保留实测的 CRLF 行尾）。
        let cases: [(&str, Option<Version>, Option<&str>); 4] = [
            (
                V_400C03,
                Some(Version {
                    major: 4,
                    minor: 0,
                    patch: 3,
                }),
                Some("4.00.03"),
            ),
            (
                V_40602,
                Some(Version {
                    major: 4,
                    minor: 6,
                    patch: 2,
                }),
                Some("4.06.02"),
            ),
            (
                V_502,
                Some(Version {
                    major: 5,
                    minor: 0,
                    patch: 2,
                }),
                Some("5.0.2"),
            ),
            (V_NONSTANDARD, None, None),
        ];

        for (input, want_version, want_text) in cases {
            assert_eq!(parse_version(input), want_version, "input: {input:?}");
            assert_eq!(
                find_version_token(input).as_deref(),
                want_text,
                "input: {input:?}"
            );
        }
    }

    #[test]
    fn version_token_rejects_non_versions() {
        assert!(parse_version_token("23-Oct-06").is_none());
        assert!(parse_version_token("11121.ttys002.MacBook-Pro-3").is_none());
        assert!(parse_version_token("4.0.3.1").is_none());
        assert!(parse_version_token("screen").is_none());
        assert_eq!(
            parse_version_token("4.9").unwrap(),
            Version {
                major: 4,
                minor: 9,
                patch: 0
            }
        );
    }

    #[test]
    fn unknown_version_does_not_panic() {
        assert!(parse_version("").is_none());
        assert!(parse_version("Screen version unknown").is_none());
        assert!(find_version_token("Screen version unknown").is_none());
    }

    #[test]
    fn query_classification_matches_measured_400c03_output() {
        // 本机 4.00.03 实测：usage 打到 stdout，退出码 1。
        let run = crate::screen::cmd::Run {
            command: "screen -Q windows".into(),
            code: 1,
            stdout: "Use: screen [-opts] [cmd [args]]\n or: screen -r [host.tty]\n".into(),
            stderr: String::new(),
        };
        assert_eq!(classify_query(&run), Support::No);

        let unsupported = crate::screen::cmd::Run {
            command: "screen -Q windows".into(),
            code: 1,
            stdout: String::new(),
            stderr: "Error: Unknown option -Q".into(),
        };
        assert_eq!(classify_query(&unsupported), Support::No);

        let no_session = crate::screen::cmd::Run {
            command: "screen -Q windows".into(),
            code: 1,
            stdout: "No screen session found.".into(),
            stderr: String::new(),
        };
        assert_eq!(classify_query(&no_session), Support::Unknown);

        let ok = crate::screen::cmd::Run {
            command: "screen -Q windows".into(),
            code: 0,
            stdout: "0$ bash\n".into(),
            stderr: String::new(),
        };
        assert_eq!(classify_query(&ok), Support::Yes);
    }
}
