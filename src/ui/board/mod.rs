//! `swamp board` (`docs/BOARD.md`): read-only except a confirmed cancel; only `sources` does IO.

pub mod app;
pub mod model;
pub mod render;
#[cfg(test)]
mod screens;
pub mod sources;
#[cfg(test)]
mod tests;
