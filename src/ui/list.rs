//! 会话列表渲染（FR-01 / T1.2）。
//!
//! 宽度预算规则（FR-01 验收 3）：**先牺牲名字长度，绝不牺牲序号与状态** ——
//! 光标/序号/图标是固定列，名字拿剩余预算，超出按显示宽度裁剪并补 `…`
//! （CJK/emoji 安全，走 `util::width` 全链路钳制）。
//!
//! 渲染只读 `App`；行装配是纯函数，直接可单测。

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::screen::parse::SessionRecord;
use crate::ui::theme;
use crate::util::width::{clip_with_ellipsis, pad_right, sanitize};

/// 行前缀固定开销（列）：光标 1 + 空格 1 + 序号 2 + 空格 1 + 图标 1 + 空格 1。
const PREFIX_COLS: usize = 7;
/// 状态文字（含前导空格）预留的最大显示宽度。
const STATUS_SUFFIX_COLS: usize = 13;

/// 是否显示状态文字（T1.2 的过渡策略；T1.3 换成正式的四档信息分级）。
fn show_status_text(area_width: u16) -> bool {
    area_width as usize >= 40
}

/// 名字列的显示宽度预算（保证整行不溢出）。
fn name_budget(area_width: u16, with_status: bool) -> usize {
    let mut budget = (area_width as usize).saturating_sub(PREFIX_COLS);
    if with_status {
        budget = budget.saturating_sub(STATUS_SUFFIX_COLS);
    }
    budget
}

/// 装配一行。`name_budget` 由调用方按宽度算好；此处不做溢出保护之外的布局决策。
fn row_line(
    idx: usize,
    session: &SessionRecord,
    selected: bool,
    name_budget: usize,
    with_status: bool,
) -> Line<'static> {
    let base: Style = if selected {
        theme::selected_row()
    } else {
        Style::default()
    };
    let icon_style = if selected {
        base
    } else {
        Style::default().fg(theme::status_color(&session.status))
    };

    let cursor = if selected { ">" } else { " " };
    let mut spans = vec![
        Span::styled(format!("{cursor} "), base),
        Span::styled(format!("{:2} ", idx + 1), base),
        Span::styled(
            format!("{} ", theme::status_icon(&session.status)),
            icon_style,
        ),
    ];

    // 名字：控制字符消毒 → 按显示宽度裁剪 → 补齐到预算（对齐右侧状态列）。
    let name = clip_with_ellipsis(&sanitize(&session.name), name_budget);
    spans.push(Span::raw(pad_right(&name, name_budget)));

    if with_status {
        let label = clip_with_ellipsis(&session.status.label(), STATUS_SUFFIX_COLS - 1);
        spans.push(Span::styled(format!(" {label}"), base));
    }

    Line::from(spans)
}

/// 把列表画进给定区域（空态给「按 n 新建」引导，FR-01 验收 1）。
pub fn render(f: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let sessions = app.sessions();
    if sessions.is_empty() {
        let hint = "No screen sessions. Press n to create one.";
        f.render_widget(Paragraph::new(hint).style(theme::dimmed()), area);
        return;
    }

    let with_status = show_status_text(area.width);
    let budget = name_budget(area.width, with_status);
    let lines: Vec<Line<'static>> = sessions
        .iter()
        .enumerate()
        .map(|(idx, session)| row_line(idx, session, idx == app.selected, budget, with_status))
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::caps::Caps;
    use crate::screen::parse::{self, Enumeration, Outlook};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ls-cjk-emoji.txt"
    );

    fn app_with_fixture() -> App {
        let text = std::fs::read_to_string(FIXTURE).unwrap();
        let list = parse::parse_list_output(&text).unwrap();
        let mut app = App::new(Caps::default());
        app.apply_enumeration(Enumeration {
            outlook: Outlook::Available(4),
            list,
            list_error: None,
        });
        app
    }

    /// 画到 TestBackend 并抽取第 `y` 行的完整文本。
    fn line_at(terminal: &Terminal<TestBackend>, y: u16) -> String {
        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        (0..area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>()
    }

    /// 去掉缓冲区里的占位空格。宽字符（CJK/emoji）在 ratatui 缓冲区占两格、
    /// 尾格为空 —— 抽行文本时会出现 `会 话` 这样的间隔，对比前先压掉。
    fn squeezed(line: &str) -> String {
        line.replace(' ', "")
    }

    fn draw(app: &App, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| render(f, app, f.area())).unwrap();
        terminal
    }

    #[test]
    fn cjk_and_emoji_names_stay_in_their_columns() {
        let app = app_with_fixture();
        let terminal = draw(&app, 80, 12);

        // 解析器按名称排序 —— 期望值从 app 会话表动态取，不假设 fixture 行序。
        for (idx, session) in app.sessions().iter().enumerate() {
            let y = idx as u16;
            let chars: Vec<char> = line_at(&terminal, y).chars().collect();
            let want = theme::status_icon(&session.status).chars().next().unwrap();
            assert_eq!(chars[5], want, "row {y} icon column: {chars:?}");

            let name = squeezed(&line_at(&terminal, y));
            assert!(name.contains(&session.name), "row {y}: {name:?}");
        }
    }

    #[test]
    fn truncation_sacrifices_name_only() {
        let app = app_with_fixture();
        let terminal = draw(&app, 40, 12);

        // 超长名会话：预算 40-7-13=20，必被截断；但序号、图标、状态文字一样不少
        // （FR-01 验收 3 的牺牲顺序）。
        let (y, session) = app
            .sessions()
            .iter()
            .enumerate()
            .find(|(_, s)| s.name == "very-long-session-name-for-truncation-testing")
            .unwrap();
        let line = line_at(&terminal, y as u16);
        assert!(line.contains('…'), "long name should be clipped: {line:?}");
        assert!(!line.contains("truncation-testing"), "tail must be gone");
        assert!(line.contains("multi"));
        let chars: Vec<char> = line.chars().collect();
        assert_eq!(chars[5], '◈');
        let _ = session;
    }

    #[test]
    fn narrow_width_hides_status_text_but_keeps_icon() {
        let app = app_with_fixture();
        let terminal = draw(&app, 30, 12);

        for (idx, session) in app.sessions().iter().enumerate() {
            let line = line_at(&terminal, idx as u16);
            let chars: Vec<char> = line.chars().collect();
            let want = theme::status_icon(&session.status).chars().next().unwrap();
            assert_eq!(chars[5], want, "icon must survive at 30 cols");
            // 状态文字一概不出现（未知状态词除外——fixture 里没有）。
            assert!(!line.contains("multi"), "status text must be hidden");
            assert!(!line.contains("attached"));
        }
        // 30 列下超长名仍应截断。
        assert!(line_at(&terminal, 0).contains('…'));
    }

    #[test]
    fn wide_width_fits_every_fixture_name() {
        let app = app_with_fixture();
        let terminal = draw(&app, 80, 12);
        for idx in 0..app.sessions().len() {
            let line = squeezed(&line_at(&terminal, idx as u16));
            assert!(!line.contains('…'), "80 cols fits every fixture name");
        }
    }

    #[test]
    fn empty_state_shows_create_hint() {
        let mut app = App::new(Caps::default());
        app.apply_enumeration(Enumeration {
            outlook: Outlook::NoSessions,
            list: parse::parse_list_output("No Sockets found in /tmp/.screen.\n").unwrap(),
            list_error: None,
        });
        let terminal = draw(&app, 60, 10);
        let body = squeezed(&line_at(&terminal, 0));
        assert!(body.contains("Noscreensessions"));
        assert!(body.contains("Pressn"), "must point at the create key");
    }

    #[test]
    fn selected_row_is_highlighted_and_cursor_marks_it() {
        let mut app = app_with_fixture();
        app.selected = 1;
        let terminal = draw(&app, 60, 12);

        let line = line_at(&terminal, 1);
        assert!(line.starts_with('>'), "cursor marks selection: {line:?}");
        assert!(!line_at(&terminal, 0).starts_with('>'));
        // 选中行样式为 REVERSED：抽一个单元格验证 modifier。
        let buffer = terminal.backend().buffer();
        assert!(
            buffer[(0, 1)]
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
    }

    #[test]
    fn display_width_helper_matches_unicode_width() {
        // 守门：本模块所有对齐假设建立在 display_width 上。
        use crate::util::width::display_width;
        assert_eq!(display_width("会话列表"), 8);
        assert_eq!(display_width("🚀"), 2);
    }
}
