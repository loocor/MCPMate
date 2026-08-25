pub mod api;
pub mod audit;
pub mod clients;
pub mod common;
pub mod config;
pub mod core;
pub mod inspector;
pub mod interop;
pub mod macros;
pub mod mcper;
pub mod runtime;
pub mod system;

#[cfg(test)]
pub mod helpers;

// Re-export FFI types for easier access
#[cfg(feature = "interop")]
pub use interop::{MCPMateEngine, ServiceInfo, ServiceStatus, StartupProgress};
