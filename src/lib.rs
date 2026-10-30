//! MPP Autopilot: automation for the MPP (Mon Petit Prono) app.
//!
//! The crate exposes the pure building blocks used by the `mpp-autopilot`
//! binary.

#[cfg(feature = "backtest")]
pub mod backtest;
pub mod cli;
pub mod config;
pub mod elo;
pub mod model;
pub mod mpp;
pub mod odds;
pub mod report;
pub mod retry;
pub mod run;
pub mod scheduler;
pub mod service;
pub mod state;
pub mod teams;
