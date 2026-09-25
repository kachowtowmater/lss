//! `lss` - terminal monitor + agent CLI for LLM SERVER STATUS. Reads the collector's `/status`
//! document and its history API (`/series`, `/hist`, `/gateway`, `/rules`, `/tokens`,
//! `/loadouts`, `/bench`, `/advice`); never talks to the model server itself.

pub mod client;
pub mod constraints;
pub mod data;
pub mod demo;
pub mod money;
pub mod plain;
pub mod prefs;
pub mod report;
pub mod ui;

/// The collector on this machine. Anything else comes from `--url`, `$LSS_URL` or
/// `~/.config/lss/lss.toml` (`lss_core::config::resolve_url`).
pub const DEFAULT_URL: &str = lss_core::config::DEFAULT_COLLECTOR_URL;
