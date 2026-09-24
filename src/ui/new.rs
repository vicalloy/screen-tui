//! 新建会话表单的渲染（FR-02）。
//!
//! 纯函数绘制：只读 `NewDraft`。三字段同屏 —— 焦点字段回显块状光标 `▏`，
//! 其余字段暗色整行；错误（阻断）红色、提示（重名等非阻断）黄色，多行按行数撑高弹层。

use ratatui::Frame;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::{NewDraft, NewField};
use crate::ui::{centered_rect, theme};
use crate::util::width::clip_with_ellipsis;

const BOX_WIDTH: u16 = 58;
/// 边框内的可用列数（`Block::bordered()` 左右各占一列）。
const INNER_WIDTH: usize = BOX_WIDTH as usize - 2;
/// 固定内容：焦点指示 + 空行 + 三字段行 + 状态行 + 空行 + 按键提示。
const BASE_CONTENT: u16 = 8;

pub fn render_overlay(f: &mut Frame<'_>, draft: &NewDraft) {
    let message_lines = draft
        .error
        .as_ref()
        .map(|e| e.lines().count())
        .or_else(|| draft.note.as_ref().map(|n| n.lines().count()))
        .unwrap_or(0) as u16;
    // 目录字段聚焦时展示收藏列表（T2.7）：标题行 + 每条目录一行。
    let recent_lines = if draft.focus == NewField::Dir && !draft.recent.is_empty() {
        draft.recent.len() as u16 + 1
    } else {
        0
    };
    let height = BASE_CONTENT + message_lines + recent_lines + 2; // + 边框
    let area = centered_rect(f.area(), BOX_WIDTH, height);

    let mut lines = vec![focus_indicator(draft), Line::from("")];
    for field in [NewField::Name, NewField::Dir, NewField::Command] {
        lines.push(field_line(draft, field));
    }
    match (&draft.error, &draft.note) {
        (Some(error), _) => {
            for line in error.lines() {
                lines.push(Line::from(Span::styled(
                    clip_with_ellipsis(line, INNER_WIDTH),
                    Style::default().fg(ratatui::style::Color::Red),
                )));
            }
        }
        (None, Some(note)) => {
            for line in note.lines() {
                lines.push(Line::from(Span::styled(
                    clip_with_ellipsis(line, INNER_WIDTH),
                    Style::default().fg(ratatui::style::Color::Yellow),
                )));
            }
        }
        (None, None) => lines.push(Line::from("")),
    }
    lines.push(Line::from(""));
    lines.push(dimmed_line(crate::i18n::t().new_hint));

    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(crate::i18n::t().new_title)),
        area,
    );
}

/// 焦点指示：`[1 Name]  2 Directory  3 Command` —— 焦点字段加粗，其余暗色。
fn focus_indicator(draft: &NewDraft) -> Line<'static> {
    let all = [NewField::Name, NewField::Dir, NewField::Command];
    let mut spans = Vec::with_capacity(all.len());
    for field in all {
        let text = format!("{} {}", field.index() + 1, field.label());
        if field == draft.focus {
            spans.push(Span::styled(
                format!("[{text}]"),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(format!("{text} "), theme::dimmed()));
        }
    }
    Line::from(spans)
}

/// 字段行：标签右对齐到同一列；焦点字段加粗并带块状光标，其余整行暗色。
///
/// 标签文案走 i18n（FR-25），对齐宽度按**显示宽度**取三个标签的最大值
/// （中文 `目录` 只有 4 列，不能沿用英文 Directory 的 9）。
fn field_line(draft: &NewDraft, field: NewField) -> Line<'static> {
    use crate::util::width::display_width;

    let label = field.label();
    let value = match field {
        NewField::Name => &draft.name,
        NewField::Dir => &draft.dir,
        NewField::Command => &draft.command,
    };
    // ` label: ` 前缀按最宽标签对齐，块状光标再占一列。
    let label_width = [NewField::Name, NewField::Dir, NewField::Command]
        .iter()
        .map(|f| display_width(f.label()))
        .max()
        .unwrap_or(9);
    let prefix_cols = 1 + label_width + 2;
    let value_cols = INNER_WIDTH.saturating_sub(prefix_cols + 1);
    let pad = " ".repeat(label_width.saturating_sub(display_width(label)));
    let label = format!("{pad}{label}:");
    if field == draft.focus {
        Line::from(vec![
            Span::styled(
                format!(" {label} "),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(clip_with_ellipsis(value, value_cols)),
            Span::styled("▏", Style::default().add_modifier(Modifier::BOLD)),
        ])
    } else {
        Line::from(Span::styled(
            clip_with_ellipsis(&format!(" {label} {value}"), INNER_WIDTH),
            theme::dimmed(),
        ))
    }
}

/// 按弹层内宽裁剪的一行暗色文本（NFR-06：行长钳制，不交给终端换行）。
fn dimmed_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        clip_with_ellipsis(text, INNER_WIDTH),
        theme::dimmed(),
    ))
}
