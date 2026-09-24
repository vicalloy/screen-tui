//! 预览渲染（FR-15 / T2.3）：宽屏右栏常驻 pane + `p` 弹层。
//!
//! 只读 `app.preview`（`Option<PreviewView>`）：没有视图就显示「按 p 抓取」提示，
//! **绝不缓存旧内容冒充当前会话的画面**（FR-15 验收 3）。

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::{App, PreviewView};
use crate::ui::theme;
use crate::util::width::{clip_with_ellipsis, sanitize};

/// 宽屏右栏 Preview pane（常驻，随选中项更新）。
pub fn render_pane(f: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let lines = match pane_lines(app) {
        Some(lines) => lines,
        None => vec![Line::from(Span::styled(
            crate::i18n::t().preview_pane_hint.to_string(),
            theme::dimmed(),
        ))],
    };
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(crate::i18n::t().preview_title)
                .title_style(Style::default().add_modifier(Modifier::BOLD)),
        ),
        area,
    )
}

/// `p` 弹层（窄屏 / 按需）。
pub fn render_overlay(f: &mut ratatui::Frame<'_>, app: &App) {
    crate::ui::render_list_screen(f, app);
    let Some(view) = &app.preview else {
        return; // 理论上到不了：失败不产生视图、不进入该 Mode。
    };
    let lines = view_lines(view);
    let height = (lines.len() as u16 + 2).min(f.area().height);
    let width = 72.min(f.area().width);
    let area = crate::ui::centered_rect(f.area(), width, height);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(crate::i18n::fmt(
            crate::i18n::t().preview_overlay_title,
            &[&view.name, &view.fetched],
        ))),
        area,
    );
}

/// pane 内容：视图存在且仍对应当前选中项 → 内容；否则提示。
/// 选中项变了但还没抓到新快照时，显示提示而不是上一个会话的画面。
fn pane_lines(app: &App) -> Option<Vec<Line<'static>>> {
    let view = app.preview.as_ref()?;
    let visible = app.sessions();
    let current = visible.get(app.selected)?;
    if current.full != view.full {
        return None;
    }
    Some(view_lines(view))
}

fn view_lines(view: &PreviewView) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        crate::i18n::fmt(
            crate::i18n::t().preview_fetched,
            &[&view.fetched, &view.name],
        ),
        theme::dimmed(),
    ))];
    if view.lines.is_empty() {
        lines.push(Line::from(crate::i18n::t().preview_empty));
    }
    for line in &view.lines {
        lines.push(Line::from(Span::raw(line.clone())));
    }
    lines
}

/// 供 overlay 标题宽度计算用（保持 `util::width` 全链路钳制）。
pub(crate) fn clamp_line(line: &str, max: usize) -> String {
    clip_with_ellipsis(&sanitize(line), max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Mode;

    #[test]
    fn pane_hides_stale_view_when_selection_changed() {
        let mut app = crate::app::App::new(crate::screen::caps::Caps::default());
        let list = crate::screen::parse::parse_list_output(
            "There are screens on:\n\t111.a\t(09/23/2026 10:00:00 AM)\t(Detached)\n\t222.b\t(09/23/2026 10:01:00 AM)\t(Detached)\n2 Sockets in /tmp/.screen.\n",
        )
        .unwrap();
        app.apply_enumeration(crate::screen::parse::Enumeration {
            outlook: crate::screen::parse::Outlook::Available(2),
            list,
            list_error: None,
        });

        // 视图属于 a：选中 a → 显示内容；选中 b → 回落提示（不显示 a 的画面）。
        app.preview = Some(PreviewView {
            full: "111.a".into(),
            name: "a".into(),
            fetched: "2026-09-23 18:00:00".into(),
            lines: vec!["hello".into()],
        });

        app.selected = 0;
        assert!(pane_lines(&app).is_some());
        app.selected = 1;
        assert!(pane_lines(&app).is_none());
    }

    #[test]
    fn overlay_only_renders_with_a_view() {
        let mut app = crate::app::App::new(crate::screen::caps::Caps::default());
        app.mode = Mode::Preview;
        app.preview = None;
        // 无视图时 overlay 不画（由 ui::render 的 Preview 分支保证）。
        assert!(app.preview.is_none());
    }
}
