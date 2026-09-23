//! 新建会话三步向导的渲染（FR-02）。
//!
//! 纯函数绘制：只读 `NewDraft`，输入回显加块状光标 `▏`；
//! 错误（阻断）红色、提示（重名等非阻断）黄色，多行错误按行数撑高弹层。

use ratatui::Frame;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::{NewDraft, NewStep};
use crate::ui::{centered_rect, theme};

const BOX_WIDTH: u16 = 58;
/// 固定内容：步骤指示 + 空行 + 输入行 + 状态行 + 空行 + 按键提示。
const BASE_CONTENT: u16 = 6;

pub fn render_overlay(f: &mut Frame<'_>, draft: &NewDraft) {
    let message_lines = draft
        .error
        .as_ref()
        .map(|e| e.lines().count())
        .or_else(|| draft.note.as_ref().map(|n| n.lines().count()))
        .unwrap_or(0) as u16;
    let height = BASE_CONTENT + message_lines + 2; // + 边框
    let area = centered_rect(f.area(), BOX_WIDTH, height);

    let mut lines = vec![steps_indicator(draft), Line::from("")];
    lines.push(input_line(draft));
    match (&draft.error, &draft.note) {
        (Some(error), _) => {
            for line in error.lines() {
                lines.push(Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(ratatui::style::Color::Red),
                )));
            }
        }
        (None, Some(note)) => {
            for line in note.lines() {
                lines.push(Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(ratatui::style::Color::Yellow),
                )));
            }
        }
        (None, None) => lines.push(Line::from("")),
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Enter next/commit · Esc cancel".to_string(),
        theme::dimmed(),
    )));

    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" New session ")),
        area,
    );
}

/// `[1 Name]  2 Directory  3 Command` —— 当前步加粗，其余暗色。
fn steps_indicator(draft: &NewDraft) -> Line<'static> {
    let all = [NewStep::Name, NewStep::Dir, NewStep::Command];
    let mut spans = Vec::with_capacity(all.len());
    for step in all {
        let text = format!("{} {}", step.index() + 1, step.label());
        if step == draft.step {
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

/// 当前步的输入回显 + 块状光标。
fn input_line(draft: &NewDraft) -> Line<'static> {
    let (label, value) = match draft.step {
        NewStep::Name => ("Name", &draft.name),
        NewStep::Dir => ("Directory", &draft.dir),
        NewStep::Command => ("Command", &draft.command),
    };
    Line::from(vec![
        Span::styled(
            format!(" {label}: "),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(value.clone()),
        Span::styled("▏", Style::default().add_modifier(Modifier::BOLD)),
    ])
}
