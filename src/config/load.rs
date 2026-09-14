#![allow(dead_code, unused_variables)]

use crate::config::schema::Schema;
use crate::error::SwampError;
use camino::Utf8Path;

/// One configuration layer plus where it came from, for `config show --effective`.
#[derive(Debug, Clone)]
pub struct Layer {
    pub origin: String,
    pub schema: Schema,
}

pub fn user_config_path() -> Option<camino::Utf8PathBuf> {
    todo!("WP1")
}

pub fn repo_config_path(repo: &Utf8Path) -> camino::Utf8PathBuf {
    todo!("WP1")
}

pub fn read_layer(path: &Utf8Path) -> Result<Layer, SwampError> {
    todo!("WP1")
}

pub fn env_layer() -> Result<Layer, SwampError> {
    todo!("WP1")
}

pub fn merge(layers: Vec<Layer>) -> Schema {
    todo!("WP1")
}

pub fn apply_profile(s: &mut Schema, profile: &str) -> Result<(), SwampError> {
    todo!("WP1")
}
