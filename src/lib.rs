//! stabilizatores — Windows internet optimization.
//!
//! Boosts the Windows network stack with configurable tweak groups (TCP
//! tuning, low-latency DNS, network throttling removal, MTU normalization),
//! keeps them self-healed while running and exposes an interactive terminal
//! menu plus non-interactive `--apply` / `--revert` / `--status` modes.
//!
//! All modules are re-exported here so tests can target the library without
//! going through the binary entry point.

pub mod app;
pub mod cli;
pub mod config;
pub mod connections;
pub mod dns;
pub mod interfaces;
pub mod log;
pub mod mtu;
pub mod netsh;
pub mod power;
pub mod probe;
pub mod registry;
pub mod restore;
pub mod system;
pub mod tcp;
pub mod traffic;
pub mod win;
pub mod wlan;
