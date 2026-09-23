//! 详情面板与详情弹层（FR-05 详情面板 / T1.3）。
//!
//! M1 只展示 `-ls` 已有字段；`-Q` 窗口数、工作目录等增强属 T2.2/T2.5。
//! 字段缺失就整行不出现 —— 显示「无」就是编造（C-5）。

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::app::App;

/// 常驻详情面板（宽屏右栏 / 中屏底部区）。
pub fn render_panel(f: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let lines: Vec<Line<'static>> = match app.sessions().get(app.selected) {
        None => vec![Line::from(Span::styled(
            " nothing selected".to_string(),
            Style::default(),
        ))],
        Some(session) => crate::ui::layout::detail_lines(session, app.meta.as_ref())
            .into_iter()
            .map(Line::from)
            .collect(),
    };
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" Detail ")
                .title_style(Style::default().add_modifier(Modifier::BOLD)),
        ),
        area,
    )
}

pub use crate::ui::layout::detail_lines;
