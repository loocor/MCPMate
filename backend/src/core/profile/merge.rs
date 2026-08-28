//! Profile merging algorithm implementation
//!
//! Merges server and tool configurations from multiple active profile

use crate::config::database::Database;
use crate::config::server::ServerEnabledService;
use crate::core::foundation::error::{CoreError, CoreResult};
use crate::core::profile::types::*;
use std::sync::Arc;

/// Profile merger
///
/// Responsible for merging server and tool configurations from multiple active profile
#[derive(Debug, Clone)]
pub struct ProfileMerger {
    /// Database reference
    db: Arc<Database>,
    /// Unified server enabled service
    server_enabled_service: ServerEnabledService,
}

impl ProfileMerger {
    /// Create a new ProfileMerger instance
    pub fn new(db: Arc<Database>) -> Self {
        let server_enabled_service = ServerEnabledService::new(db.pool.clone());
        Self {
            db,
            server_enabled_service,
        }
    }

    /// Execute complete configuration merging operation
    ///
    /// # Returns
    /// - `Ok(ProfileMergeResult)`: Merge result
    /// - `Err(CoreError)`: Error during merging process
    pub async fn merge_all_configs(&self) -> CoreResult<ProfileMergeResult> {
        // Get all active profile
        let active_profile = crate::config::profile::get_active_profile(&self.db.pool)
            .await
            .map_err(|e| CoreError::generic_error(&format!("Failed to get active  profile: {}", e), Some(e)))?;

        tracing::debug!("Found {} active profile", active_profile.len());

        // Merge servers from all active profile
        let merged_servers = self.merge_servers(&active_profile).await?;

        // Merge tools from all active profile
        let merged_tools = self.merge_all_tools(&active_profile).await?;

        // Build allowlists for tools/resources/prompts
        let (allowed_tool_unique, allowed_resource_unique, allowed_prompt_unique) = {
            // tools
            let tools_any: i64 = sqlx::query_scalar(
                r#"
                SELECT COUNT(1)
                FROM profile_capability_refs pcr
                JOIN capability_refs cr ON cr.ref_id = pcr.ref_id
                JOIN profile p ON pcr.profile_id = p.id
                WHERE p.is_active = 1 AND p.profile_mode != 'workflow' AND cr.kind = 'tools'
                "#,
            )
            .fetch_one(&self.db.pool)
            .await
            .unwrap_or(0);
            let tools_allowed = if tools_any == 0 {
                None
            } else {
                let sql = crate::config::profile::tool::build_enabled_tools_query(None);
                let rows: Vec<(String, String, String, String)> =
                    sqlx::query_as(&sql).fetch_all(&self.db.pool).await.unwrap_or_default();
                let mut v = Vec::new();
                for (unique_name, _server_name, _tool_name, _server_id) in rows {
                    v.push(unique_name);
                }
                Some(v)
            };

            // resources
            let res_any: i64 = sqlx::query_scalar(
                r#"
                SELECT COUNT(1)
                FROM profile_capability_refs pcr
                JOIN capability_refs cr ON cr.ref_id = pcr.ref_id
                JOIN profile p ON pcr.profile_id = p.id
                WHERE p.is_active = 1 AND p.profile_mode != 'workflow' AND cr.kind = 'resources'
                "#,
            )
            .fetch_one(&self.db.pool)
            .await
            .unwrap_or(0);
            let res_allowed = if res_any == 0 {
                None
            } else {
                let sql = crate::config::profile::resource::build_enabled_resources_query(None);
                // (server_id, server_name(original), resource_uri)
                let rows: Vec<(String, String, String)> =
                    sqlx::query_as(&sql).fetch_all(&self.db.pool).await.unwrap_or_default();
                let mut v = Vec::new();
                for (server_id, _server_name_original, upstream_uri) in rows {
                    let unique = crate::core::capability::naming::load_external_identifier(
                        &self.db.pool,
                        crate::core::capability::naming::NamingKind::Resource,
                        &server_id,
                        &upstream_uri,
                    )
                    .await?;
                    v.push(unique);
                }
                Some(v)
            };

            // prompts
            let pro_any: i64 = sqlx::query_scalar(
                r#"
                SELECT COUNT(1)
                FROM profile_capability_refs pcr
                JOIN capability_refs cr ON cr.ref_id = pcr.ref_id
                JOIN profile p ON pcr.profile_id = p.id
                WHERE p.is_active = 1 AND p.profile_mode != 'workflow' AND cr.kind = 'prompts'
                "#,
            )
            .fetch_one(&self.db.pool)
            .await
            .unwrap_or(0);
            let pro_allowed = if pro_any == 0 {
                None
            } else {
                let sql = crate::config::profile::prompt::build_enabled_prompts_query(None);
                // (server_id, server_name(original), prompt_name)
                let rows: Vec<(String, String, String)> =
                    sqlx::query_as(&sql).fetch_all(&self.db.pool).await.unwrap_or_default();
                let mut v = Vec::new();
                for (server_id, _server_name_original, prompt_name) in rows {
                    let unique = crate::core::capability::naming::load_external_identifier(
                        &self.db.pool,
                        crate::core::capability::naming::NamingKind::Prompt,
                        &server_id,
                        &prompt_name,
                    )
                    .await?;
                    v.push(unique);
                }
                Some(v)
            };

            (tools_allowed, res_allowed, pro_allowed)
        };

        // Build merge result
        let profile_ids: Vec<String> = active_profile.iter().filter_map(|s| s.id.clone()).collect();

        Ok(ProfileMergeResult {
            servers: merged_servers,
            tools: merged_tools,
            allowed_tool_unique,
            allowed_resource_unique,
            allowed_prompt_unique,
            merged_profile: profile_ids,
            merged_at: chrono::Utc::now(),
        })
    }

    /// Get merged tool configurations for a specific server
    ///
    /// # Arguments
    /// - `server_id`: Server ID
    ///
    /// # Returns
    /// - `Ok(Vec<MergedToolConfig>)`: List of tool configurations for this server
    /// - `Err(CoreError)`: Error during merging process
    pub async fn merge_tools_for_server(
        &self,
        server_id: &str,
    ) -> CoreResult<Vec<MergedToolConfig>> {
        // Get all active profile
        let active_profile = crate::config::profile::get_active_profile(&self.db.pool)
            .await
            .map_err(|e| CoreError::generic_error(&format!("Failed to get active  profile: {}", e), Some(e)))?;

        // Merge tools for the specific server
        self.merge_tools_for_specific_server(&active_profile, server_id).await
    }

    /// Check if a specific tool is enabled (with semi-blacklist mode)
    ///
    /// This function implements "semi-blacklist mode": tools are enabled by default
    /// unless explicitly disabled in profile.
    ///
    /// # Arguments
    /// - `server_id`: Server ID
    /// - `tool_name`: Tool name
    ///
    /// # Returns
    /// - `Ok(bool)`: Whether the tool is enabled
    /// - `Err(CoreError)`: Error during checking process
    pub async fn is_tool_enabled(
        &self,
        server_id: &str,
        tool_name: &str,
    ) -> CoreResult<bool> {
        tracing::debug!(
            "Checking tool enablement for '{}' on server '{}' using ProfileMerger",
            tool_name,
            server_id
        );

        // Get all active profile
        let active_profile = crate::config::profile::get_active_profile(&self.db.pool)
            .await
            .map_err(|e| CoreError::generic_error(&format!("Failed to get active  profile: {}", e), Some(e)))?;

        // If no active profile, tool is enabled by default (semi-blacklist mode)
        if active_profile.is_empty() {
            tracing::debug!(
                "No active profile found, tool '{}' on server '{}' is enabled by default (semi-blacklist mode)",
                tool_name,
                server_id
            );
            return Ok(true);
        }

        let mut found_explicit_config = false;

        // Check if the tool is configured in any of the active profile for this server
        for profile in &active_profile {
            if let Some(profile_id) = &profile.id {
                if let Ok(servers) = crate::config::profile::get_profile_servers(&self.db.pool, profile_id).await {
                    // Check if this profile contains the specified server and it's enabled
                    let has_enabled_server = servers.iter().any(|s| s.server_id == server_id && s.enabled);

                    if has_enabled_server {
                        if let Ok(tools) = crate::config::profile::get_profile_tools(&self.db.pool, profile_id).await {
                            for tool in tools {
                                if tool.tool_name == tool_name && tool.server_id == server_id {
                                    found_explicit_config = true;
                                    if tool.enabled {
                                        tracing::debug!(
                                            "Tool '{}' on server '{}' is explicitly enabled in profile '{}'",
                                            tool_name,
                                            server_id,
                                            profile_id
                                        );
                                        return Ok(true);
                                    } else {
                                        tracing::debug!(
                                            "Tool '{}' on server '{}' is explicitly disabled in profile '{}'",
                                            tool_name,
                                            server_id,
                                            profile_id
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // If no explicit configuration found, tool is enabled by default (semi-blacklist mode)
        if !found_explicit_config {
            tracing::debug!(
                "No explicit configuration for tool '{}' on server '{}', enabled by default (semi-blacklist mode)",
                tool_name,
                server_id
            );
            return Ok(true);
        }

        // If we found explicit configuration but none were enabled, tool is disabled
        tracing::debug!(
            "Tool '{}' on server '{}' has explicit configuration but is disabled",
            tool_name,
            server_id
        );
        Ok(false)
    }

    /// Merge servers from all active profile
    async fn merge_servers(
        &self,
        active_profile: &[crate::config::models::Profile],
    ) -> CoreResult<Vec<MergedServerConfig>> {
        use std::collections::HashMap;

        let mut server_map: HashMap<String, MergedServerConfig> = HashMap::new();

        for profile in active_profile {
            if let Some(profile_id) = &profile.id {
                if let Ok(profile_servers) =
                    crate::config::profile::get_profile_servers(&self.db.pool, profile_id).await
                {
                    for profile_server in profile_servers {
                        // Use unified service to check if server is enabled
                        let is_enabled = self
                            .server_enabled_service
                            .is_server_enabled(&profile_server.server_id)
                            .await
                            .map_err(|e| {
                                CoreError::generic_error(
                                    &format!("Failed to check server enabled status: {}", e),
                                    Some(e),
                                )
                            })?;

                        if !is_enabled {
                            tracing::debug!(
                                "Server {} is not enabled (either in profile or globally), skipping",
                                profile_server.server_id
                            );
                            continue;
                        }

                        // Get server details from server_config table
                        let server_details = match sqlx::query_as::<_, (String, Option<String>)>(
                            "SELECT name, url FROM server_config WHERE id = ?",
                        )
                        .bind(&profile_server.server_id)
                        .fetch_optional(&self.db.pool)
                        .await
                        {
                            Ok(Some((name, url))) => (name, url.unwrap_or_default()),
                            Ok(None) => {
                                tracing::warn!("Server {} not found in server_config", profile_server.server_id);
                                continue;
                            }
                            Err(e) => {
                                tracing::error!("Failed to get server details for {}: {}", profile_server.server_id, e);
                                continue;
                            }
                        };

                        let entry =
                            server_map
                                .entry(profile_server.server_id.clone())
                                .or_insert_with(|| MergedServerConfig {
                                    server_id: profile_server.server_id.clone(),
                                    name: server_details.0.clone(),
                                    address: server_details.1.clone(),
                                    enabled_tools: vec![],
                                    source_profile: vec![],
                                });

                        // Add this profile as a source
                        if !entry.source_profile.contains(profile_id) {
                            entry.source_profile.push(profile_id.clone());
                        }

                        // Get tools for this server in this profile
                        if let Ok(tools) = crate::config::profile::get_profile_tools(&self.db.pool, profile_id).await {
                            for tool in tools {
                                if tool.enabled
                                    && tool.server_id == profile_server.server_id
                                    && !entry.enabled_tools.contains(&tool.tool_name)
                                {
                                    entry.enabled_tools.push(tool.tool_name);
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(server_map.into_values().collect())
    }

    /// Merge all tools from active profile
    async fn merge_all_tools(
        &self,
        active_profile: &[crate::config::models::Profile],
    ) -> CoreResult<Vec<MergedToolConfig>> {
        use std::collections::HashMap;

        let mut tool_map: HashMap<String, MergedToolConfig> = HashMap::new();

        for profile in active_profile {
            if let Some(profile_id) = &profile.id {
                if let Ok(tools) = crate::config::profile::get_profile_tools(&self.db.pool, profile_id).await {
                    for tool in tools {
                        let entry = tool_map.entry(tool.tool_name.clone()).or_insert_with(|| {
                            MergedToolConfig {
                                tool_name: tool.tool_name.clone(),
                                enabled: false,
                                server_ids: vec![],
                                config: HashMap::new(), // Empty config for now
                                source_profile: vec![],
                            }
                        });

                        // Update enabled status (if any profile enables it, it's enabled)
                        if tool.enabled {
                            entry.enabled = true;
                        }

                        // Add server ID if not already present
                        if tool.enabled && !entry.server_ids.contains(&tool.server_id) {
                            entry.server_ids.push(tool.server_id.clone());
                        }

                        // Add this profile as a source
                        if !entry.source_profile.contains(profile_id) {
                            entry.source_profile.push(profile_id.clone());
                        }

                        // Note: ProfileTool doesn't have a config field in the current model
                        // If configuration is needed, it would need to be added to the database schema
                    }
                }
            }
        }

        Ok(tool_map.into_values().collect())
    }

    /// Merge tools for a specific server
    async fn merge_tools_for_specific_server(
        &self,
        active_profile: &[crate::config::models::Profile],
        server_id: &str,
    ) -> CoreResult<Vec<MergedToolConfig>> {
        use std::collections::HashMap;

        let mut tool_map: HashMap<String, MergedToolConfig> = HashMap::new();

        for profile in active_profile {
            if let Some(profile_id) = &profile.id {
                // Check if this profile contains the specified server and it's enabled
                if let Ok(servers) = crate::config::profile::get_profile_servers(&self.db.pool, profile_id).await {
                    let has_enabled_server = servers.iter().any(|s| s.server_id == server_id && s.enabled);

                    if has_enabled_server {
                        if let Ok(tools) = crate::config::profile::get_profile_tools(&self.db.pool, profile_id).await {
                            for tool in tools {
                                // Only include tools for the specified server
                                if tool.server_id != server_id {
                                    continue;
                                }

                                let entry =
                                    tool_map
                                        .entry(tool.tool_name.clone())
                                        .or_insert_with(|| MergedToolConfig {
                                            tool_name: tool.tool_name.clone(),
                                            enabled: false,
                                            server_ids: vec![server_id.to_string()],
                                            config: HashMap::new(),
                                            source_profile: vec![],
                                        });

                                // Update enabled status
                                if tool.enabled {
                                    entry.enabled = true;
                                }

                                // Add this profile as a source
                                if !entry.source_profile.contains(profile_id) {
                                    entry.source_profile.push(profile_id.clone());
                                }

                                // Note: ProfileTool doesn't have a config field in the current model
                                // Configuration merging would need database schema updates
                            }
                        }
                    }
                }
            }
        }

        Ok(tool_map.into_values().collect())
    }
}
