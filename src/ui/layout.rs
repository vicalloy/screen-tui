//! 布局档位（FR-04 / T1.3）。
//!
//! 阈值取自 requirements §4.1 FR-04：72 列是 `tscreen` 实测的手机 SSH 常见宽度；
//! 100 列以上开双栏。阈值常量公开，配置化属 T2.1。

use crate::screen::parse::SessionRecord;

/// 宽屏档位阈值（≥ 100 列开双栏）。
pub const WIDE_COLS: u16 = 100;
/// 窄屏阈值（< 72 列进窄屏形态）。
pub const NARROW_COLS: u16 = 72;
/// 极小屏宽度阈值。
pub const TINY_COLS: u16 = 50;
/// 极小屏高度阈值（< 15 行视为极小）。
pub const TINY_ROWS: u16 = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// ≥ 100 列：左右双栏（列表 + 详情），页脚 2 行完整快捷键。
    Wide,
    /// 72–99 列：单栏列表 + 底部详情区，页脚 1 行。
    Mid,
    /// < 72 列：单栏列表，详情按 `i` 弹层，页脚 1 行极简。
    Narrow,
    /// < 50 列或 < 15 行：只留「序号 + 状态图标 + 名字截断」。
    Tiny,
}

impl Tier {
    /// 档位判定（纯函数，供 resize 自动重排与单测使用）。
    /// 阈值取 `ui::layout` 常量；配置化版本见 [`Tier::from_size_with`]。
    pub fn from_size(width: u16, height: u16) -> Tier {
        Self::from_size_with(width, height, NARROW_COLS, WIDE_COLS)
    }

    /// 阈值来自配置的档位判定（T2.1 / FR-04「阈值可配置」）。
    ///
    /// 非法配置按边界夹紧：阈值永远不会把 ≥50 列的终端判进 Tiny 之外的方向，
    /// `narrow ≥ wide` 的退化配置最多让 Mid 档消失，不会让判定区间交叉。
    pub fn from_size_with(width: u16, height: u16, narrow_cols: u16, wide_cols: u16) -> Tier {
        if width < TINY_COLS || height < TINY_ROWS {
            return Tier::Tiny;
        }
        let wide = wide_cols.max(TINY_COLS);
        let narrow = narrow_cols.clamp(TINY_COLS, wide.saturating_sub(1));
        if width >= wide {
            Tier::Wide
        } else if width >= narrow {
            Tier::Mid
        } else {
            Tier::Narrow
        }
    }

    /// 页脚行数：宽屏 2 行完整快捷键，其余 1 行。
    pub fn footer_rows(self) -> u16 {
        match self {
            Tier::Wide => 2,
            _ => 1,
        }
    }
}

/// 某档位下列表行暴露哪些列（FR-05 信息分级）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowCols {
    /// 状态文字（仅宽屏；其余档位只留图标）。
    pub status_text: bool,
    /// PID（宽屏 + 中屏）。
    pub pid: bool,
    /// 创建时间（仅宽屏）。
    pub created: bool,
}

impl RowCols {
    pub fn for_tier(tier: Tier) -> RowCols {
        match tier {
            Tier::Wide => RowCols {
                status_text: true,
                pid: true,
                created: true,
            },
            Tier::Mid => RowCols {
                status_text: false,
                pid: true,
                created: false,
            },
            Tier::Narrow | Tier::Tiny => RowCols {
                status_text: false,
                pid: false,
                created: false,
            },
        }
    }
}

/// 一条会话在给定档位下可展示的全部字段（详情面板与 `i` 弹层共用）。
///
/// 取不到的字段直接不出现该行 —— 显示「无」就是编造（C-5）。
pub fn detail_lines(session: &SessionRecord) -> Vec<String> {
    let mut lines = vec![format!("name      {}", session.name)];
    lines.push(format!("address   {}", session.full));
    if let Some(pid) = session.pid {
        lines.push(format!("pid       {pid}"));
    }
    lines.push(format!("status    {}", session.status.label()));
    if let Some(created) = &session.created {
        lines.push(format!("created   {created}"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_boundaries_match_the_spec() {
        // 宽度分档。注意 FR-05 验收 1 把 40×20 称为「窄屏极端尺寸」，
        // 但按 FR-04 的阈值定义（<50 列即 Tiny），40×20 落在 Tiny 档 ——
        // 两档的行结构一致（序号+图标+名字），≥5 条可见的验收在 Tiny 下同样成立。
        assert_eq!(Tier::from_size(120, 40), Tier::Wide);
        assert_eq!(Tier::from_size(100, 24), Tier::Wide);
        assert_eq!(Tier::from_size(99, 24), Tier::Mid);
        assert_eq!(Tier::from_size(72, 24), Tier::Mid);
        assert_eq!(Tier::from_size(71, 24), Tier::Narrow);
        assert_eq!(Tier::from_size(50, 24), Tier::Narrow);
        assert_eq!(Tier::from_size(49, 24), Tier::Tiny);
        assert_eq!(Tier::from_size(40, 20), Tier::Tiny);
        // 高度兜底：高度不足即 Tiny（与宽度无关）。
        assert_eq!(Tier::from_size(40, 14), Tier::Tiny);
        assert_eq!(Tier::from_size(120, 14), Tier::Tiny);
    }

    #[test]
    fn row_columns_follow_the_info_tiering_table() {
        // FR-05：状态文字仅宽屏；pid 到中屏为止；创建时间仅宽屏。
        assert!(RowCols::for_tier(Tier::Wide).status_text);
        assert!(RowCols::for_tier(Tier::Wide).pid);
        assert!(RowCols::for_tier(Tier::Wide).created);

        assert!(!RowCols::for_tier(Tier::Mid).status_text);
        assert!(RowCols::for_tier(Tier::Mid).pid);
        assert!(!RowCols::for_tier(Tier::Mid).created);

        assert!(!RowCols::for_tier(Tier::Narrow).pid);
        assert!(!RowCols::for_tier(Tier::Tiny).pid);
    }

    #[test]
    fn detail_lines_skip_missing_fields() {
        let full = SessionRecord {
            full: "12345.work".into(),
            pid: Some(12345),
            name: "work".into(),
            created: Some("08/09/2026 10:23:45 AM".into()),
            status: crate::screen::parse::Status::Detached,
        };
        let lines = detail_lines(&full);
        assert!(lines.iter().any(|l| l.contains("created")));

        let bare = SessionRecord {
            full: "67890.llm".into(),
            pid: None,
            name: "llm".into(),
            created: None,
            status: crate::screen::parse::Status::Attached,
        };
        let lines = detail_lines(&bare);
        assert!(!lines.iter().any(|l| l.starts_with("pid")));
        assert!(!lines.iter().any(|l| l.starts_with("created")));
    }

    #[test]
    fn wide_footer_has_two_rows() {
        assert_eq!(Tier::Wide.footer_rows(), 2);
        assert_eq!(Tier::Narrow.footer_rows(), 1);
    }
}
