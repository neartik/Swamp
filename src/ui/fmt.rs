use crate::model::core::{Cost, NodeState, Provider};
use std::time::Duration;
use time::OffsetDateTime;
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

/// "6d21h", then "23h", then "3h02m": a span the RESETS column has nine columns for, with
/// room for the `~` an estimated window prefixes it with.
pub fn until(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 86_400 {
        let (days, hours) = (secs / 86_400, (secs % 86_400) / 3600);
        return format!("{days}d{hours:02}h");
    }
    // "23h59m" plus "in " plus the tilde is one column too many; the minutes are the part
    // nobody reads half a day out.
    if secs >= 36_000 {
        return format!("{}h", secs / 3600);
    }
    if secs >= 3600 {
        return duration(d);
    }
    // "24m59s" plus "in " plus the tilde overflows the cell too, and a window rolls whole
    // minutes from now at best.
    if secs >= 60 {
        return format!("{}m", secs / 60);
    }
    format!("{secs}s")
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

/// Worker output reaches the terminal verbatim. ESC and its friends would let a worker
/// repaint the screen and forge Swamp's own lines, so they never survive to stdout.
fn is_control(c: char) -> bool {
    (c as u32) < 0x20 || c == '\u{7f}' || ('\u{80}'..='\u{9f}').contains(&c)
}

pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\t' | '\n' | '\r' => ' ',
            c if (c as u32) < 0x20 || c == '\u{7f}' || ('\u{80}'..='\u{9f}').contains(&c) => {
                '\u{fffd}'
            }
            c => c,
        })
        .collect()
}

/// Unicode-width aware, and never lets a control character through to the terminal.
pub fn truncate(s: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let owned;
    let s = if s.chars().any(is_control) {
        owned = sanitize(s);
        owned.as_str()
    } else {
        s
    };
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

/// "02:30", or "02:30 on 2026-09-22" once the reset is not on today's UTC date: a seven-day
/// window resets days out, and a bare wall clock names no day at all.
pub fn clock_day(at: OffsetDateTime, now: OffsetDateTime) -> String {
    let at = at.to_offset(time::UtcOffset::UTC);
    if at.date() == now.to_offset(time::UtcOffset::UTC).date() {
        return clock_hm(at);
    }
    format!("{} on {}", clock_hm(at), at.date())
}

/// The one line a node blocked on an exhausted pool gets. `cancel` is the only part chat and
/// `swamp run` disagree on, so the wording cannot drift between them.
pub fn blocked_notice(
    provider: Provider,
    until: OffsetDateTime,
    now: OffsetDateTime,
    cancel: &str,
) -> String {
    let secs = (until - now).whole_seconds().max(0) as u64;
    format!(
        "every {provider} account is at its limit \u{b7} earliest reset {} (in {}) \u{b7} {cancel}",
        clock_day(until, now),
        self::until(Duration::from_secs(secs))
    )
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

    /// Worker text is attacker-influenced and goes straight to a terminal: an ESC sequence
    /// could clear the screen and print a fake "run succeeded" banner over the real rows.
    #[test]
    fn control_characters_never_reach_the_terminal() {
        let evil = "\u{1b}[2J\u{1b}[Hswamp: run succeeded";
        let shown = truncate(evil, 60);
        assert!(!shown.contains('\u{1b}'), "{shown:?}");
        assert!(shown.contains("swamp: run succeeded"));
        assert_eq!(truncate("a\nb\tc", 10), "a b c");
        assert_eq!(truncate("plain", 10), "plain");
    }

    /// `Pool::block_reason` feeds this the soonest reset, which is legitimately a seven-day
    /// window: an hours-only span and a bare wall clock named no day at all and disagreed
    /// with the RESETS column on the same instant.
    #[test]
    fn a_blocked_notice_days_away_names_the_day() {
        let now = OffsetDateTime::from_unix_timestamp(1_789_400_000).expect("now");
        let far = now + Duration::from_secs(6 * 86_400 + 17 * 3600);
        let line = blocked_notice(Provider::Anthropic, far, now, "ctrl-c to cancel");
        assert!(line.contains("(in 6d17h)"), "{line}");
        assert!(
            line.contains(&format!("reset {} on {}", clock_hm(far), far.date())),
            "{line}"
        );

        // Today: the wall clock alone still says it.
        let soon = now + Duration::from_secs(41 * 60);
        let near = blocked_notice(Provider::Anthropic, soon, now, "ctrl-c to cancel");
        assert!(
            near.contains(&format!("reset {} (in 41m)", clock_hm(soon))),
            "{near}"
        );
        assert!(!near.contains(" on "), "{near}");
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
