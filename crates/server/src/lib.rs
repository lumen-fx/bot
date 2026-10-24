//! The lumenfx bot server: webhook receiver, workers, executor, scheduler,
//! and the read-only `/status` and `/help` pages.

pub mod cla;
pub mod db;
pub mod executor;
pub mod fake;
pub mod github;
pub mod scheduler;
pub mod snapshot;
pub mod web;
pub mod webhook;
pub mod worker;
