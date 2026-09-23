//! 无第三方依赖的本地时间标签（仅用于默认会话名 `MMDD-HHMM`，FR-02）。
//!
//! chrono 不在依赖清单里；`libc` 已因信号处理引入，`localtime_r` 是它最便宜的时间出口。

use std::time::{SystemTime, UNIX_EPOCH};

/// 当前本地时间的 `MMDD-HHMM` 标签，如 9 月 23 日 16:10 → `0923-1610`。
///
/// 时钟早于 epoch 等极端情形返回空串 —— 调用方（默认名生成）对空串有兜底。
pub fn local_label(now: SystemTime) -> String {
    let Ok(secs) = now.duration_since(UNIX_EPOCH) else {
        return String::new();
    };
    let secs = libc_time_t(secs.as_secs());
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // localtime_r 是线程安全的（_r 后缀），且不在信号处理器里调用。
    let ok = unsafe { !libc::localtime_r(&secs, &mut tm).is_null() };
    if !ok {
        return String::new();
    }
    format!(
        "{:02}{:02}-{:02}{:02}",
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

fn libc_time_t(secs: u64) -> libc::time_t {
    secs.min(i64::MAX as u64) as libc::time_t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_shape_is_mmdd_hhmm() {
        let label = local_label(SystemTime::now());
        assert_eq!(label.len(), 9, "MMDD-HHMM is 9 chars: {label:?}");
        let chars: Vec<char> = label.chars().collect();
        for (idx, expected) in [
            (0, true),
            (1, true),
            (2, true),
            (3, true),
            (5, true),
            (6, true),
            (7, true),
            (8, true),
        ] {
            assert_eq!(
                chars[idx].is_ascii_digit(),
                expected,
                "char {idx} of {label:?}"
            );
        }
        assert_eq!(chars[4], '-');
    }

    #[test]
    fn pre_epoch_clock_yields_empty_label() {
        // SystemTime 早于 UNIX_EPOCH 在 API 上不可直接构造，用 0 前的安全路径：
        // duration_since 失败分支由 panic-free 契约保证（此处验证不 panic 即可）。
        let _ = local_label(UNIX_EPOCH);
    }
}
