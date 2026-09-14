#![allow(dead_code, unused_variables)]

use crate::config::CooldownCfg;
use crate::model::failure::Failure;
use std::time::Duration;
use time::OffsetDateTime;

pub fn cooldown_for(
    f: &Failure,
    consecutive: u32,
    cfg: &CooldownCfg,
    now: OffsetDateTime,
) -> Option<Duration> {
    todo!("WP4")
}
