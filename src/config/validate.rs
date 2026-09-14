#![allow(dead_code, unused_variables)]

use crate::config::Config;
use crate::error::SwampError;

/// One validation failure, named by the offending config key.
#[derive(Debug, Clone)]
pub struct Problem {
    pub key: String,
    pub detail: String,
}

/// Reports ALL problems at once; a single bad key must not hide the next one.
pub fn validate(cfg: &mut Config) -> Result<(), SwampError> {
    todo!("WP1")
}

pub fn problems(cfg: &Config) -> Vec<Problem> {
    todo!("WP1")
}
