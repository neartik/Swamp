use crate::ui::chat::theme::Theme;
use std::time::Duration;

const BRAIN: [&str; 6] = ["·", "✢", "✳", "✶", "✻", "✽"];
const BRAIN_ASCII: [&str; 6] = [".", "o", "O", "0", "@", "*"];
const WORKER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const WORKER_ASCII: [&str; 4] = [".", "o", "O", "o"];

pub const VERBS: [&str; 10] = [
    "Thinking",
    "Pondering",
    "Brewing",
    "Orchestrating",
    "Wading",
    "Dredging",
    "Marshalling",
    "Surveying",
    "Herding",
    "Deliberating",
];

const VERB_PERIOD_S: u64 = 4;

/// Half speed: the brain's own frame is a heartbeat, not a progress bar.
pub fn brain_frame(theme: &Theme, tick: u64) -> &'static str {
    let frames: &[&str] = if theme.ascii { &BRAIN_ASCII } else { &BRAIN };
    frames[((tick / 2) as usize) % frames.len()]
}

/// The per-row offset is what makes a board shimmer instead of pulsing in lockstep.
pub fn worker_frame(theme: &Theme, tick: u64, row: usize) -> &'static str {
    let frames: &[&str] = if theme.ascii { &WORKER_ASCII } else { &WORKER };
    frames[((tick as usize).wrapping_add(row)) % frames.len()]
}

/// Seeded from the run so a recording replays with the same words.
pub fn verb(seed: u64, elapsed: Duration, orchestrating: bool) -> &'static str {
    if orchestrating {
        return "Orchestrating";
    }
    let step = elapsed.as_secs() / VERB_PERIOD_S;
    VERBS[((seed.wrapping_add(step)) % VERBS.len() as u64) as usize]
}

/// A ULID's low bits: stable per run, and cheap.
pub fn seed_of(run: crate::ids::RunId) -> u64 {
    (run.0.0 & 0xffff) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbs_rotate_every_four_seconds_and_a_batch_pins_them() {
        let a = verb(0, Duration::from_secs(0), false);
        assert_eq!(a, verb(0, Duration::from_secs(3), false));
        assert_ne!(a, verb(0, Duration::from_secs(4), false));
        assert_eq!(verb(7, Duration::from_secs(120), true), "Orchestrating");
    }

    #[test]
    fn the_brain_spinner_advances_at_half_the_worker_rate() {
        let t = Theme::default();
        assert_eq!(brain_frame(&t, 0), brain_frame(&t, 1));
        assert_ne!(brain_frame(&t, 0), brain_frame(&t, 2));
        assert_ne!(worker_frame(&t, 4, 0), worker_frame(&t, 4, 1));
    }
}
