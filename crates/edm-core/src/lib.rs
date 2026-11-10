//! EngineDM core. Everything the old `edm_api` + `config` crates and the C FFI layer did, as
//! plain Rust. Modules that only use `std` (config, account, otp, desktop, layout, ipc) have
//! unit tests that run without any external crate.

pub mod account;
pub mod auth;
pub mod config;
pub mod desktop;
pub mod greetd;
pub mod handoff;
pub mod ipc;
pub mod launch;
pub mod layout;
pub mod otp;
pub mod restart;
pub mod state;
pub mod supervise;
pub mod theme;
pub mod unidata;

pub use config::EDMConfig;
