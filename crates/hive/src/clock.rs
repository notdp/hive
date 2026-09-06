//! UTC timestamps in the formats hive writes. Depends on nothing in the
//! crate.

use std::time::{SystemTime, UNIX_EPOCH};

/// `YYYY-MM-DDTHH:MM:SS` of *secs* in UTC, no zone suffix — callers add
/// the `Z` or fractional tail their own record format carries.
pub fn utc_iso_seconds(secs: u64) -> String {
    let secs = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::gmtime_r(&secs, &mut tm) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        tm.tm_year as i64 + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// Now as `YYYY-MM-DDTHH:MM:SS` UTC, no zone suffix.
pub fn utc_now_iso_seconds() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    utc_iso_seconds(dur.as_secs())
}

pub fn utc_timestamp_ms() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{}.{:03}Z",
        utc_iso_seconds(dur.as_secs()),
        dur.subsec_millis()
    )
}
