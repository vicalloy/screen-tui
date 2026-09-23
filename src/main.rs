//! stui — GNU Screen 的 TUI 前端（crate 名 `screen-tui`）。
//!
//! 分层见 `design/tech-design.md` §2：
//! `cli` → `app`/`ui` → `screen`（适配层）／`config`／`doctor`／`util`。
//! M0 只交付「只读地基 + 构建链」：`caps` / `parse` / `ls` / `doctor` 与 Makefile。
//!
//! 其中一部分已设计好的接口（状态判定、会话查找等）要到 M1/M2 才会被界面层消费，
//! 故在此显式豁免 dead_code —— 有意保留的接口，不是遗忘的死代码。
#![allow(dead_code)]

mod cli;
mod config;
mod doctor;
mod screen;
mod util;

fn main() -> std::process::ExitCode {
    cli::run()
}
