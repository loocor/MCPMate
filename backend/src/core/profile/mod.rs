//! Core Profile Module
//!
//! Business logic layer for profile, responsible for configuration merging,
//! tool checking and other core business functions.
//!
//! ## Module Responsibilities
//! - Profile merging algorithms
//! - Tool enablement status checking
//! - Server configuration aggregation
//! - Business rule validation
//! - Server lifecycle management for profile integration
//!
//! ## Architecture Principles
//! - Only depends on config/profile data interfaces
//! - No direct database connection operations
//! - Communicates with other modules through event mechanisms

pub mod authoring;
pub mod config;
pub mod materials;
pub mod merge;
pub mod projection;
pub mod service;
pub mod types;
pub mod visibility;
pub mod workflow;
pub mod workflow_guide;

// Re-export core types and services
pub use config::ConfigApplicationStateManager;
pub use merge::ProfileMerger;
pub use service::ProfileService;
pub use types::*;

use crate::config::database::Database;
use crate::core::models::Config;
use std::sync::Arc;

/// Get merged configuration from active profile
/// Returns both ProfileMergeResult and Config formats
pub async fn get_merged_configuration(db: &Database) -> anyhow::Result<(ProfileMergeResult, Config)> {
    let merger = ProfileMerger::new(Arc::new(db.clone()));
    let merge_result = merger
        .merge_all_configs()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to merge configurations: {}", e))?;

    // Convert to Config format using the unified loader
    let (_, config) = crate::core::foundation::loader::load_servers_from_active_profile(db).await?;

    Ok((merge_result, config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::database::Database;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::path::PathBuf;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_profile_service_creation() {
        // This test just verifies that we can create a ProfileService instance
        // without panicking. It doesn't require a real database connection.

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let db = Arc::new(Database {
            pool,
            path: PathBuf::from(":memory:"),
            capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
        });

        // Create ProfileService
        let _profile_service = ProfileService::new(db);

        // If we get here without panicking, the test passes
        // We can't access private fields, so we just verify creation succeeded
    }

    #[test]
    fn test_profile_types_creation() {
        use std::collections::HashMap;

        // Test MergedServerConfig creation
        let server_config = MergedServerConfig {
            server_id: "test-server".to_string(),
            name: "Test Server".to_string(),
            address: "localhost:8080".to_string(),
            enabled_tools: vec!["tool1".to_string(), "tool2".to_string()],
            source_profile: vec!["profile1".to_string()],
        };

        assert_eq!(server_config.server_id, "test-server");
        assert_eq!(server_config.enabled_tools.len(), 2);

        // Test MergedToolConfig creation
        let tool_config = MergedToolConfig {
            tool_name: "test-tool".to_string(),
            enabled: true,
            server_ids: vec!["server1".to_string()],
            config: HashMap::new(),
            source_profile: vec!["profile1".to_string()],
        };

        assert_eq!(tool_config.tool_name, "test-tool");
        assert!(tool_config.enabled);

        // Test ToolEnabledResult creation
        let enabled_result = ToolEnabledResult {
            tool_name: "test-tool".to_string(),
            enabled: true,
            enabled_servers: vec!["server1".to_string()],
            related_profile: vec!["profile1".to_string()],
        };

        assert_eq!(enabled_result.tool_name, "test-tool");
        assert!(enabled_result.enabled);
    }
}
