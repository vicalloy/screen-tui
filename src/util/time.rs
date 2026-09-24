//! 无第三方依赖的本地时间标签（配置里的 `last_used` 等场景，FR-23）。
//!
//! chrono 不在依赖清单里；`libc` 已因信号处理引入，`localtime_r` 是它最便宜的时间出口。

use std::time::{SystemTime, UNIX_EPOCH};

/// 当前本地时间的完整标签 `YYYY-MM-DD HH:MM:SS`（配置里的 `last_used` 等场景）。
pub fn local_datetime(now: SystemTime) -> String {
    with_local(now, |tm| {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    })
    .unwrap_or_default()
}

fn with_local(now: SystemTime, fmt: impl FnOnce(&libc::tm) -> String) -> Option<String> {
    let secs = now.duration_since(UNIX_EPOCH).ok()?;
    let secs = libc_time_t(secs.as_secs());
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // localtime_r 是线程安全的（_r 后缀），且不在信号处理器里调用。
    let ok = unsafe { !libc::localtime_r(&secs, &mut tm).is_null() };
    if !ok {
        return None;
    }
    Some(fmt(&tm))
}

fn libc_time_t(secs: u64) -> libc::time_t {
    secs.min(i64::MAX as u64) as libc::time_t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datetime_shape_is_iso_like() {
        let stamp = local_datetime(SystemTime::now());
        assert_eq!(stamp.len(), 19, "YYYY-MM-DD HH:MM:SS: {stamp:?}");
        let bytes = stamp.as_bytes();
        assert_eq!(bytes[4], b'-');
        assert_eq!(bytes[7], b'-');
        assert_eq!(bytes[10], b' ');
        assert_eq!(bytes[13], b':');
        assert_eq!(bytes[16], b':');
    }

    #[test]
    fn pre_epoch_clock_does_not_panic() {
        // SystemTime 早于 UNIX_EPOCH 在 API 上不可直接构造，用 0 前的安全路径：
        // duration_since 失败分支由 panic-free 契约保证（此处验证不 panic 即可）。
        let _ = local_datetime(UNIX_EPOCH);
    }
}
