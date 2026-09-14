#![allow(dead_code, unused_variables)]

use crate::model::core::{Cost, NodeState};
use std::time::Duration;

/// "4m12s"
pub fn duration(d: Duration) -> String {
    todo!("WP7")
}

/// "1.2M"
pub fn tokens(n: u64) -> String {
    todo!("WP7")
}

/// None -> "-", never "$0.00". Reported and Estimated both render with a leading "~",
/// because reported cost on a subscription is list-price equivalence, not money billed.
pub fn cost(c: Option<Cost>) -> String {
    todo!("WP7")
}

pub fn glyph(s: &NodeState) -> &'static str {
    todo!("WP7")
}

/// Unicode-width aware.
pub fn truncate(s: &str, n: usize) -> String {
    todo!("WP7")
}
