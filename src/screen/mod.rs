//! Screen 适配层 —— 能力探测、会话解析、命令构造与执行。
//!
//! 设计依据：`design/tech-design.md` §2/§3.2、`design/screen-capabilities.md` §2–§5。
//! 本层**不接触终端**，全部可单测（NFR-10）。

pub mod caps;
pub mod cmd;
pub mod parse;
pub mod probe;

/// 状态类型在适配层根上再导出一份，供上层免于深路径引用。
pub use parse::Status;

use std::path::PathBuf;

/// 适配层错误。
///
/// 用 `thiserror` 做类型化错误，上层按变体决定「降级展示」还是「直接失败」——
/// 这是 tech-design §4 明确不引入 `anyhow` 的原因。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("screen executable not found (checked $STUI_SCREEN and every PATH entry)")]
    NotFound,

    #[error("`{path}` is not an executable file")]
    NotExecutable { path: PathBuf },

    #[error("failed to spawn `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },

    #[error("`{command}` exited with code {code}: {stderr}")]
    CommandFailed {
        command: String,
        code: i32,
        stderr: String,
    },

    #[error("unrecognized `screen -ls` output: {0}")]
    UnrecognizedOutput(String),

    /// 临时文件（hardcopy 试写 / 预览）相关的 IO 错误。
    #[error("temporary file error: {0}")]
    Temp(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
