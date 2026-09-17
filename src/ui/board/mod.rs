//! `swamp board`: the read-only dispatch board, `docs/BOARD.md`.
//!
//! `model` is pure and holds everything a frame draws; `sources` is the only half that
//! touches the filesystem. Nothing here talks to a supervisor.

pub mod app;
pub mod model;
pub mod render;
#[cfg(test)]
mod screens;
pub mod sources;
#[cfg(test)]
mod tests;
