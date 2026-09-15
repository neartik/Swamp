use crate::model::core::{Cost, NodeState};
use std::time::Duration;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// "4m12s"
pub fn duration(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// "1.2M"
pub fn tokens(n: u64) -> String {
    let (value, unit) = match n {
        0..=999 => return n.to_string(),
        1_000..=999_999 => (n as f64 / 1_000.0, "k"),
        _ => (n as f64 / 1_000_000.0, "M"),
    };
    if value >= 100.0 {
        return format!("{value:.0}{unit}");
    }
    let one = format!("{value:.1}");
    format!("{}{unit}", one.strip_suffix(".0").unwrap_or(&one))
}

/// None -> "-", never "$0.00". Reported and Estimated both render with a leading "~",
/// because reported cost on a subscription is list-price equivalence, not money billed.
pub fn cost(c: Option<Cost>) -> String {
    match c {
        None => "-".to_owned(),
        Some(c) => format!("~${:.2}", c.usd),
    }
}

pub fn glyph(s: &NodeState) -> &'static str {
    match s {
        NodeState::Queued => "·",
        NodeState::Blocked { .. } => "~",
        NodeState::Leased { .. } => "◦",
        NodeState::Running { .. } => "▶",
        NodeState::Orphaned { .. } => "?",
        NodeState::Succeeded => "✔",
        NodeState::Failed { .. } => "✘",
        NodeState::Cancelled { .. } => "⊘",
    }
}

/// The word `swamp trace` prints in the status column.
pub fn state_word(s: &NodeState) -> &'static str {
    match s {
        NodeState::Queued => "queued",
        NodeState::Blocked { .. } => "blocked",
        NodeState::Leased { .. } => "leased",
        NodeState::Running { .. } => "running",
        NodeState::Orphaned { .. } => "orphaned",
        NodeState::Succeeded => "ok",
        NodeState::Failed { .. } => "failed",
        NodeState::Cancelled { .. } => "cancelled",
    }
}

/// Unicode-width aware.
pub fn truncate(s: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    if s.width() <= n {
        return s.to_owned();
    }
    let mut out = String::new();
    let mut width = 0usize;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if width + w > n.saturating_sub(1) {
            break;
        }
        width += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// Width-aware left padding, so a column stays a column when a title holds CJK.
pub fn pad(s: &str, n: usize) -> String {
    let cell = truncate(s, n);
    let fill = n.saturating_sub(cell.width());
    format!("{cell}{}", " ".repeat(fill))
}

/// UTC on purpose: a trace rendered in Berlin and in Tokyo must be the same bytes.
pub fn clock(at: time::OffsetDateTime) -> String {
    let t = at.to_offset(time::UtcOffset::UTC).time();
    format!("{:02}:{:02}:{:02}", t.hour(), t.minute(), t.second())
}

/// Wall-clock reset times are minute-precision: the seconds are noise.
pub fn clock_hm(at: time::OffsetDateTime) -> String {
    let t = at.to_offset(time::UtcOffset::UTC).time();
    format!("{:02}:{:02}", t.hour(), t.minute())
}

pub fn short_sha(s: &str) -> String {
    s.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::core::CostBasis;

    #[test]
    fn durations_at_the_boundaries() {
        let cases = [
            (0u64, "0s"),
            (59, "59s"),
            (60, "1m00s"),
            (252, "4m12s"),
            (3599, "59m59s"),
            (3600, "1h00m"),
            (90_061, "25h01m"),
        ];
        for (secs, want) in cases {
            assert_eq!(duration(Duration::from_secs(secs)), want, "{secs}s");
        }
    }

    #[test]
    fn token_counts_keep_one_significant_decimal() {
        let cases = [
            (0u64, "0"),
            (999, "999"),
            (1_000, "1k"),
            (14_000, "14k"),
            (84_100, "84.1k"),
            (214_000, "214k"),
            (999_999, "1000k"),
            (1_200_000, "1.2M"),
            (9_400_000, "9.4M"),
        ];
        for (n, want) in cases {
            assert_eq!(tokens(n), want, "{n}");
        }
    }

    #[test]
    fn absent_cost_is_a_dash_never_zero() {
        assert_eq!(cost(None), "-");
        assert_eq!(
            cost(Some(Cost {
                usd: 1.84,
                basis: CostBasis::Reported
            })),
            "~$1.84"
        );
        assert_eq!(
            cost(Some(Cost {
                usd: 0.0,
                basis: CostBasis::Estimated
            })),
            "~$0.00"
        );
    }

    #[test]
    fn truncation_counts_display_width() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello", 4), "hel…");
        assert_eq!(truncate("", 4), "");
        assert_eq!(truncate("abc", 0), "");
        // Two-column glyphs cannot overflow the cell.
        assert_eq!(truncate("日本語です", 5), "日本…");
        assert_eq!(pad("日本", 6).width(), 6);
    }
}
