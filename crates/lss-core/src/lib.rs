//! lss-core: pure logic for LLM SERVER STATUS - parsers, the status schema, the incident
//! tracker and the alert rule engine. No I/O and no clocks in here: every function takes
//! `now` as an argument so the whole thing is testable with a fake clock.

pub mod advice;
pub mod bench;
pub mod compare;
pub mod config;
pub mod cost_setup;
pub mod cost_tables;
pub mod docker;
pub mod engine;
pub mod gate;
pub mod gatelog;
pub mod gpu;
pub mod headroom;
pub mod hist;
pub mod history;
pub mod incidents;
pub mod loadout;
pub mod maintenance;
pub mod model;
pub mod omp;
pub mod probe;
pub mod readings;
pub mod prom;
pub mod promout;
pub mod rates;
pub mod rules;
pub mod series;
pub mod setup;
pub mod targets;
pub mod timeutil;
pub mod tokens;
pub mod units;
pub mod users;
pub mod watch;
pub mod xid;

pub const STATUS_SCHEMA_VERSION: u32 = 1;
