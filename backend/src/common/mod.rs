//! Common utilities and shared functionality for MCPMate
//!
//! This module provides shared functionality used across different parts of MCPMate,
//! helping to eliminate code duplication and improve maintainability.

pub mod capability;
pub mod checker;
pub mod connection;
pub mod constants;
pub mod database;
pub mod env;
pub mod http;
pub mod json;
pub mod json5_config;
pub mod path;
pub mod paths;
pub mod profile;
pub mod server;
pub mod startup_diagnostics;
pub mod status;
pub mod sync;
pub mod types;
pub mod validation;

// Re-export commonly used items for convenience
pub use checker::{ConfigCheckResult, ConfigChecker};
pub use database::{count_records, fetch_all_ordered, fetch_optional, fetch_scalar, fetch_where, record_exists};
pub use env::{EnvironmentManager, create_runtime_environment, prepare_command_environment};
pub use http::{extract_bearer_token, trim_bearer_prefix};
pub use json::strip_comments;
pub use paths::{MCPMatePaths, get_bridge_path, global_paths, set_global_paths};
pub use sync::{SyncContext, SyncHelper, SyncResult};
pub use types::{ClientCategory, RuntimeError, RuntimeType};
pub use validation::{FieldValidation, ValidationBuilder, ValidationResult, Validator};
