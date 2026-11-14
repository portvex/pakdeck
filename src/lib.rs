//! pakdeck: a universal TUI package manager. The binary (`src/main.rs`) is a thin
//! wrapper; everything lives here so tests and fuzz targets can reach it.

// Tests may use plain arithmetic: an overflow there is a failed test.
#![cfg_attr(test, allow(clippy::arithmetic_side_effects))]

pub mod app;
pub mod backend;
pub mod config;
pub mod export;
pub mod fuzzy;
pub mod history;
pub mod term;
pub mod theme;
pub mod ui;
pub mod util;

#[doc(hidden)]
pub mod testing;
