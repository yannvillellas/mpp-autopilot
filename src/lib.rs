//! Mon Petit Prono prediction bot.
//!
//! The crate exposes the pure building blocks used by the `mpp` binary.

pub mod cli;
pub mod config;
pub mod elo;
pub mod model;
pub mod mpp;
pub mod retry;
pub mod run;
pub mod scheduler;
pub mod service;
pub mod state;
pub mod teams;
