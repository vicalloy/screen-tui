//! 会话列表渲染（FR-01 / T1.2 / T1.3）。
//!
//! 宽度预算规则（FR-01 验收 3）：**先牺牲名字长度，绝不牺牲序号与状态** ——
//! 光标/序号/图标是固定列，名字拿剩余预算，超出按显示宽度裁剪并补 `…`
//! （CJK/emoji 安全，走 `util::width` 全链路钳制）。
//!
//! 暴露哪些列由 [`Tier`] 决定（FR-05 信息分级，见 `ui::layout::RowCols`）。
//! 渲染只读 `App`；行装配是纯函数，直接可单测。

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::screen::parse::SessionRecord;
use crate::ui::layout::{RowCols, Tier};
use crate::ui::theme;
use crate::util::width::{clip_with_ellipsis, pad_right, sanitize};

/// 行前缀固定开销（列）：光标 1 + 空格 1 + 序号 2 + 空格 1 + 图标 1 + 空格 1。
const PREFIX_COLS: usize = 7;
/// 状态文字（含前导空格）预留的最大显示宽度。
const STATUS_SUFFIX_COLS: usize = 13;
/// PID 列（含前导空格，按 7 位数字预留）。
const PID_COLS: usize = 8;
/// 创建时间列（含前导空格，`MM/DD/YYYY HH:MM`）。
const CREATED_COLS: usize = 17;

/// 名字列的显示宽度预算（保证整行不溢出）。
fn name_budget(area_width: u16, cols: RowCols) -> usize {
    let mut budget = (area_width as usize).saturating_sub(PREFIX_COLS);
    if cols.status_text {
        budget = budget.saturating_sub(STATUS_SUFFIX_COLS);
    }
    if cols.pid {
        budget = budget.saturating_sub(PID_COLS);
    }
    if cols.created {
        budget = budget.saturating_sub(CREATED_COLS);
    }
    budget
}

/// 4.6+ 的创建时间原文（`08/09/2026 10:23:45 AM`）截到分钟。
fn created_display(created: &str) -> String {
    sanitize(created).chars().take(16).collect()
}

/// 装配一行。`name_budget` 由调用方按宽度算好；此处不做溢出保护之外的布局决策。
/// `alias` 是 T2.6 的自定义别名（宽/中屏显示，窄屏随信息分级隐藏）。
fn row_line(
    idx: usize,
    session: &SessionRecord,
    selected: bool,
    name_budget: usize,
    cols: RowCols,
    alias: Option<&str>,
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

    // 名字：控制字符消毒 → 按显示宽度裁剪 → 补齐到预算（对齐右侧各列）。
    // 别名跟在名字后面（同一名字预算内截断，绝不挤占序号与状态列）。
    let name = match alias {
        Some(a) if !a.is_empty() => format!("{} · {}", sanitize(&session.name), sanitize(a)),
        _ => sanitize(&session.name),
    };
    let name = clip_with_ellipsis(&name, name_budget);
    spans.push(Span::raw(pad_right(&name, name_budget)));

    if cols.status_text {
        let label = clip_with_ellipsis(&session.status.label(), STATUS_SUFFIX_COLS - 1);
        spans.push(Span::styled(format!(" {label}"), base));
    }
    if cols.pid {
        let pid = session.pid.map(|p| p.to_string()).unwrap_or_default();
        let clipped = clip_with_ellipsis(&pid, PID_COLS - 1);
        spans.push(Span::raw(pad_right(&clipped, PID_COLS - 1)));
        spans.push(Span::raw(" ".to_string()));
    }
    if cols.created {
        let text = session
            .created
            .as_deref()
            .map(created_display)
            .unwrap_or_default();
        spans.push(Span::styled(
            format!(" {}", clip_with_ellipsis(&text, CREATED_COLS - 1)),
            base,
        ));
    }

    Line::from(spans)
}

/// 把列表画进给定区域（空态给「按 n 新建」引导，FR-01 验收 1）。
///
/// `tier` 必须由**整屏**尺寸算出后传入（body 区高度少 2–3 行，会误判 Tiny）。
pub fn render(f: &mut ratatui::Frame<'_>, app: &App, area: Rect, tier: Tier) {
    let sessions = app.sessions();
    if sessions.is_empty() {
        let hint: String = if app.filter.is_empty() {
            crate::i18n::t().list_empty.into()
        } else {
            // 过滤后无命中：给出口（Esc 清空），不误报「没有会话」。
            crate::i18n::fmt(crate::i18n::t().list_filter_empty, &[&app.filter])
        };
        f.render_widget(Paragraph::new(hint).style(theme::dimmed()), area);
        return;
    }

    let cols = RowCols::for_tier(tier);
    let budget = name_budget(area.width, cols);
    // 别名只在宽/中屏暴露（FR-05 信息分级；窄屏名字空间太宝贵）。
    let show_alias = tier == Tier::Wide || tier == Tier::Mid;
    let lines: Vec<Line<'static>> = sessions
        .iter()
        .enumerate()
        .map(|(idx, session)| {
            let alias = if show_alias {
                app.config
                    .sessions
                    .get(&session.name)
                    .and_then(|m| m.alias.as_deref())
            } else {
                None
            };
            row_line(idx, session, idx == app.selected, budget, cols, alias)
        })
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
        let tier = Tier::from_size(width, height);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| {
                let area = ratatui::layout::Rect::new(0, 0, width, height);
                render(f, app, area, tier)
            })
            .unwrap();
        terminal
    }

    #[test]
    fn cjk_and_emoji_names_stay_in_their_columns() {
        let app = app_with_fixture();
        let terminal = draw(&app, 80, 20);

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
        let terminal = draw(&app, 40, 20);

        // 超长名会话：Narrow 档无附加列，预算 40-7=33，必被截断；但序号、图标
        // 一样不少（FR-01 验收 3 的牺牲顺序）。
        let (y, _) = app
            .sessions()
            .iter()
            .enumerate()
            .find(|(_, s)| s.name == "very-long-session-name-for-truncation-testing")
            .unwrap();
        let line = line_at(&terminal, y as u16);
        assert!(line.contains('…'), "long name should be clipped: {line:?}");
        assert!(!line.contains("truncation-testing"), "tail must be gone");
        let chars: Vec<char> = line.chars().collect();
        assert_eq!(chars[5], '◈');
    }

    #[test]
    fn pid_and_created_follow_the_tier() {
        let app = app_with_fixture();

        // Mid 档（80×20 列）：显示 pid，不显示创建时间。
        let mid = draw(&app, 80, 20);
        let first = &app.sessions()[0];
        let row = squeezed(&line_at(&mid, 0));
        if let Some(pid) = first.pid {
            assert!(row.contains(&pid.to_string()), "mid shows pid: {row:?}");
        }
        if let Some(created) = &first.created {
            assert!(
                !row.contains(&created_display(created)),
                "mid hides created: {row:?}"
            );
        }

        // Wide 档（120×30）：两者都显示。
        let wide = draw(&app, 120, 30);
        let row = squeezed(&line_at(&wide, 0));
        if let Some(pid) = first.pid {
            assert!(row.contains(&pid.to_string()), "wide shows pid: {row:?}");
        }
        if let Some(created) = &first.created {
            let want = squeezed(&created_display(created));
            assert!(row.contains(&want), "wide shows created: {row:?}");
        }

        // Narrow 档（71×20）：都不显示。
        let narrow = draw(&app, 71, 20);
        let row = squeezed(&line_at(&narrow, 0));
        if let Some(pid) = first.pid {
            assert!(!row.contains(&pid.to_string()), "narrow hides pid: {row:?}");
        }
    }

    #[test]
    fn wide_width_fits_every_fixture_name() {
        let app = app_with_fixture();
        let terminal = draw(&app, 80, 20);
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
        let terminal = draw(&app, 60, 20);
        let body = squeezed(&line_at(&terminal, 0));
        assert!(body.contains("Noscreensessions"));
        assert!(body.contains("Pressn"), "must point at the create key");
    }

    #[test]
    fn selected_row_is_highlighted_and_cursor_marks_it() {
        let mut app = app_with_fixture();
        app.selected = 1;
        let terminal = draw(&app, 60, 20);

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
}
