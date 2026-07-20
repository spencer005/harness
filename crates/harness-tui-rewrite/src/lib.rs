//! Terminal user interface for interactive harness sessions.
//!
//! The crate owns a rewrite-specific application model. Values from
//! `harness-core` cross one adapter boundary before reaching that model, and
//! arbitrary text reaches Ratatui only through the display typestate pipeline.

mod app;
mod control;

mod display;
pub mod domain;
mod input;
pub mod runtime;
mod terminal;

mod transcript;
mod view;

pub use runtime::run_with_runtime;
