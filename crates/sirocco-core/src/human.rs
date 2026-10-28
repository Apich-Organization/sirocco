//! Human-friendly formatting and size parsing.

/// Format a byte count with binary units.
#[must_use]
pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[i])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[i])
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

/// Format a throughput.
#[must_use]
pub fn format_rate(bytes_per_sec: f64) -> String {
    if bytes_per_sec <= 0.0 {
        return "0 B/s".into();
    }
    format!("{}/s", format_bytes(bytes_per_sec as u64))
}

/// Format a duration in seconds.
#[must_use]
pub fn format_duration(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "--".into();
    }
    let s = secs.round() as u64;
    if s < 60 {
        return format!("{s}s");
    }
    let m = s / 60;
    let s = s % 60;
    if m < 60 {
        return format!("{m}m{s:02}s");
    }
    let h = m / 60;
    let m = m % 60;
    if h < 24 {
        return format!("{h}h{m:02}m");
    }
    format!("{}d{:02}h", h / 24, h % 24)
}

/// Parse a size such as `64K`, `1M`, `2GiB`, `1.5g`, or a plain byte count.
///
/// `K`/`M`/`G`/`T` are binary (1024-based), matching how block sizes are usually
/// quoted in this domain.
#[must_use]
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let bytes = s.as_bytes();
    let mut end = bytes.len();
    while end > 0 && !bytes[end - 1].is_ascii_digit() && bytes[end - 1] != b'.' {
        end -= 1;
    }
    let (num, unit) = s.split_at(end);
    let num: f64 = num.trim().parse().ok()?;
    if num < 0.0 {
        return None;
    }
    let mult: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        | "" | "b" => 1.0,
        | "k" | "kb" | "kib" => 1024.0,
        | "m" | "mb" | "mib" => 1024.0 * 1024.0,
        | "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        | "t" | "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        | _ => return None,
    };
    let v = num * mult;
    if v > u64::MAX as f64 {
        return None;
    }
    Some(v.round() as u64)
}

/// `12345` -> `12,345`
#[must_use]
pub fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.00 MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.00 GiB");
    }

    #[test]
    fn parse() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("1K"), Some(1024));
        assert_eq!(parse_size("1k"), Some(1024));
        assert_eq!(parse_size("64K"), Some(65536));
        assert_eq!(parse_size("1M"), Some(1048576));
        assert_eq!(parse_size("1.5M"), Some(1572864));
        assert_eq!(parse_size("2GiB"), Some(2 << 30));
        assert_eq!(parse_size("abc"), None);
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(5.0), "5s");
        assert_eq!(format_duration(65.0), "1m05s");
        assert_eq!(format_duration(3700.0), "1h01m");
    }

    #[test]
    fn digits() {
        assert_eq!(group_digits(1), "1");
        assert_eq!(group_digits(1234), "1,234");
        assert_eq!(group_digits(1234567), "1,234,567");
    }
}
