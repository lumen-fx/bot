//! The lumenfx bot's decisions, with no I/O: config parsing and validation,
//! command parsing, the pull request state machine, comment templates, and
//! `evaluate(snapshot) -> Plan`.

pub mod cla;
pub mod command;
pub mod config;
pub mod evaluate;
pub mod plan;
pub mod snapshot;
pub mod state;
pub mod templates;

pub use evaluate::evaluate;
