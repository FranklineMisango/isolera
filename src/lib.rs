//! # Isolera
//!
//! Deterministic Linux CPU-core isolation for low-latency trading nodes.
//!
//! Isolera turns a declarative plan (`isolera.toml`) into a kernel command line,
//! an IRQ routing table, an SMT and CPU-governor state, and a real-time pinning
//! recipe - then proves the result with counters and `cyclictest`.
//!
//! ## Design rules
//!
//! * **No third-party crates.** The standard library is the entire supply chain
//!   of a tool that runs as root on a production node.
//! * **Every host interaction goes through [`sysroot::SysRoot`].** Paths are
//!   logical (`/proc/cmdline`); a root prefix is applied on access, so the whole
//!   suite is exercisable against `tests/fixtures/sysroot` on a laptop.
//! * **Plan before apply.** `--dry-run` records intended writes instead of
//!   performing them, using the exact code path that `apply` uses.
//! * **Everything is reversible or explicitly declared irreversible.** The write
//!   journal carries the previous value of each path.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]
#![deny(clippy::todo, clippy::unimplemented)]

pub mod apply;
pub mod audit;
pub mod bench;
pub mod cli;
pub mod config;
pub mod cpulist;
pub mod error;
pub mod grub;
pub mod irq;
pub mod journal;
pub mod kernel;
pub mod log;
pub mod pin;
pub mod plan;
pub mod render;
pub mod report;
pub mod rollback;
pub mod sysroot;
pub mod table;
pub mod tempdir;
#[cfg(test)]
pub mod testing;
pub mod toml;
pub mod topology;
pub mod verify;

/// Suite version, kept in step with `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
