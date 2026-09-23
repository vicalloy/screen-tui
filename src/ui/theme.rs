//! 样式与状态图标（requirements §6.3 / NFR-06）。
//!
//! 图标全部是**单字宽、非 emoji 变体**字符（emoji 在部分终端是双宽会破坏对齐）；
//! 配色只用 16 安全色（FR-30 未排期前不赌终端色彩能力）。

use ratatui::style::{Color, Modifier, Style};

use crate::screen::parse::Status;

/// 状态 → 单字符图标。未知状态显示 `?` 并在宽屏下附原文（不猜，C-5）。
pub fn status_icon(status: &Status) -> &'static str {
    match status {
        Status::Detached => "○",
        Status::Attached => "◆",
        Status::Multi => "◈",
        Status::Dead => "✕",
        Status::Unreachable => "?",
        Status::Unknown(_) => "?",
    }
}

/// 状态 → 16 安全色。
pub fn status_color(status: &Status) -> Color {
    match status {
        Status::Detached => Color::Green,
        Status::Attached => Color::Cyan,
        Status::Multi => Color::Yellow,
        Status::Dead => Color::Red,
        Status::Unreachable | Status::Unknown(_) => Color::DarkGray,
    }
}

/// 选中行样式：整行反显（16 色终端上最稳的高亮方式）。
pub fn selected_row() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

/// 弱化文本（页眉/页脚的次要信息）。
pub fn dimmed() -> Style {
    Style::default().fg(Color::DarkGray)
}
