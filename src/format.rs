//! Numbers, sizes and times as the window says them. Every number is the measured one: counts
//! exact (grouped with a no-break space, "3 412"), sizes as VibeFTP writes them (`812 B`,
//! `2.1 MB`, binary units as Explorer counts them), times local.

/// Thousands grouped by a no-break space: "3 412", "1 204 000".
pub fn count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3 * 2);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('\u{a0}');
        }
        out.push(c);
    }
    out
}

/// `812 B`, `14 KB`, `2.1 MB`, `140 GB`: one decimal below 10, whole numbers above.
pub fn bytes(n: u64) -> String {
    let n = n as f64;
    if n < 1023.5 {
        return format!("{} B", n.round() as u64);
    }
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    let mut v = n / 1024.0;
    let mut i = 0;
    // 1023.6 KB would read "1024 KB": the next unit instead.
    while v >= 1023.5 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v < 9.95 { format!("{v:.1} {}", UNITS[i]) } else { format!("{} {}", v.round() as u64, UNITS[i]) }
}

/// A share as a percentage: "43 %", "4.3 %", "< 0.1 %" (something, but less than that), "0 %".
pub fn pct(part: u64, whole: u64) -> String {
    if whole == 0 || part == 0 {
        return "0 %".into();
    }
    let p = part as f64 / whole as f64 * 100.0;
    if p < 0.05 {
        "< 0.1 %".into()
    } else if p < 9.95 {
        format!("{p:.1} %")
    } else {
        format!("{} %", p.round().min(100.0) as u64)
    }
}

/// "1 file", "3 412 files".
pub fn plural(n: u64, one: &str, many: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{} {many}", count(n)) }
}

/// How long something took: "0.4 s", "12 s", "3 min 05 s".
pub fn took(ms: u128) -> String {
    if ms < 10_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else if ms < 60_000 {
        format!("{} s", (ms as f64 / 1000.0).round() as u64)
    } else {
        let s = (ms / 1000) as u64;
        format!("{} min {:02} s", s / 60, s % 60)
    }
}

/// Local wall-clock time of unix seconds (as Explorer shows it), or None for 0.
pub fn local(unix: u32) -> Option<(u16, u8, u8, u8, u8)> {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    if unix == 0 {
        return None;
    }
    let ft = unix as u64 * 10_000_000 + 116_444_736_000_000_000;
    unsafe {
        let f = FILETIME { dwLowDateTime: ft as u32, dwHighDateTime: (ft >> 32) as u32 };
        let mut utc: SYSTEMTIME = std::mem::zeroed();
        let mut l: SYSTEMTIME = std::mem::zeroed();
        if FileTimeToSystemTime(&f, &mut utc) == 0 || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut l) == 0 {
            return None;
        }
        Some((l.wYear, l.wMonth as u8, l.wDay as u8, l.wHour as u8, l.wMinute as u8))
    }
}

/// "2026-10-19", empty when unknown.
pub fn date(unix: u32) -> String {
    local(unix).map(|(y, m, d, _, _)| format!("{y:04}-{m:02}-{d:02}")).unwrap_or_default()
}

/// "2026-10-19 14:05", empty when unknown.
pub fn stamp(unix: u32) -> String {
    local(unix).map(|(y, m, d, h, mi)| format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}")).unwrap_or_default()
}

/// "14:05" (the status line's "when").
pub fn clock(unix: u32) -> String {
    local(unix).map(|(_, _, _, h, mi)| format!("{h:02}:{mi:02}")).unwrap_or_default()
}

pub fn now_unix() -> u32 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs().min(u32::MAX as u64) as u32).unwrap_or(0)
}

/// A path shortened in the middle to at most `max` characters, keeping its start (the drive)
/// and as much of its end (the folder itself) as fits: `C:\Users\…\Pictures\Holiday`. The end
/// starts after a separator when one is near.
pub fn middle(s: &str, max: usize) -> String {
    let c: Vec<char> = s.chars().collect();
    if c.len() <= max || max < 8 {
        return s.to_string();
    }
    let head = (max / 3).clamp(3, 24);
    let tail = max - head - 1;
    let mut t = c.len() - tail;
    if let Some(k) = c[t..(t + 12).min(c.len())].iter().position(|&x| x == '\\') {
        t += k;
    }
    let mut h = head;
    if let Some(k) = c[..head].iter().rposition(|&x| x == '\\')
        && k + 1 >= head / 2
    {
        h = k + 1;
    }
    format!("{}…{}", c[..h].iter().collect::<String>(), c[t..].iter().collect::<String>())
}
