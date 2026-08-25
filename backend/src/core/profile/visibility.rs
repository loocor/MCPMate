use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};

use crate::clients::models::{CapabilitySource, ClientCapabilityConfig, UnifyDirectExposureConfig};
use crate::config::database::Database;
use crate::config::profile::basic::get_active_profile;
use crate::core::capability::naming::{NamingKind, resolve_capability_route};
use crate::core::capability::resource_registry::{ResourceRouteSource, resolve_resource_route};
use crate::core::profile::ProfileService;
use crate::core::proxy::server::ClientContext;
use crate::mcper::{HOSTED_BUILTIN_TOOL_NAMES, UNIFY_BUILTIN_TOOL_NAMES};

fn builtin_tool_surface_ids(
    config_mode: Option<&str>,
    capability_source: CapabilitySource,
) -> Vec<&'static str> {
    match config_mode {
        Some("unify") => UNIFY_BUILTIN_TOOL_NAMES.to_vec(),
        Some("transparent") => Vec::new(),
        _ => {
            if capability_source == CapabilitySource::Profiles {
                HOSTED_BUILTIN_TOOL_NAMES.to_vec()
            } else {
                Vec::new()
            }
        }
    }
}

fn collect_sorted_surfaces<T, F>(
    surfaces: &[T],
    format_key: F,
) -> Vec<String>
where
    F: Fn(&T) -> String,
{
    let mut result: Vec<String> = surfaces.iter().map(format_key).collect();
    result.sort();
    result.dedup();
    result
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityKind {
    Tools,
    Resources,
    ResourceTemplates,
    Prompts,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibilityQuery {
    pub client_id: String,
    pub surface_fingerprint: String,
    pub capability_kind: CapabilityKind,
}

#[derive(Debug, Clone)]
pub struct VisibilitySnapshot {
    pub client_id: String,
    pub surface_fingerprint: String,
    pub profile_ids: Vec<String>,
    pub server_ids: Vec<String>,
    pub allowed_tools: HashSet<String>,
    pub allowed_resources: HashSet<String>,
    pub allowed_resource_templates: HashSet<String>,
    pub allowed_prompts: HashSet<String>,
    has_tool_policy: bool,
    has_resource_policy: bool,
    has_resource_template_policy: bool,
    has_prompt_policy: bool,
}

#[cfg(test)]
impl VisibilitySnapshot {
    /// Builds a fixture snapshot for tests outside this module. Production code always
    /// derives a `VisibilitySnapshot` through `ProfileVisibilityService::resolve_snapshot_for_client`.
    pub(crate) fn for_test(
        client_id: &str,
        surface_fingerprint: &str,
        server_ids: Vec<String>,
        has_tool_policy: bool,
    ) -> Self {
        Self {
            client_id: client_id.to_string(),
            surface_fingerprint: surface_fingerprint.to_string(),
            profile_ids: Vec::new(),
            server_ids,
            allowed_tools: HashSet::new(),
            allowed_resources: HashSet::new(),
            allowed_resource_templates: HashSet::new(),
            allowed_prompts: HashSet::new(),
            has_tool_policy,
            has_resource_policy: false,
            has_resource_template_policy: false,
            has_prompt_policy: false,
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct ClientCapabilityRow {
    capability_source: Option<String>,
    selected_profile_ids: Option<String>,
    custom_profile_id: Option<String>,
}

struct ResolvedPolicies {
    allowed_tools: HashSet<String>,
    allowed_resources: HashSet<String>,
    allowed_resource_templates: HashSet<String>,
    allowed_prompts: HashSet<String>,
    has_tool_policy: bool,
    has_resource_policy: bool,
    has_resource_template_policy: bool,
    has_prompt_policy: bool,
}

impl ResolvedPolicies {
    fn policy_flags(&self) -> [bool; 4] {
        [
            self.has_tool_policy,
            self.has_resource_policy,
            self.has_resource_template_policy,
            self.has_prompt_policy,
        ]
    }
}

pub struct ProfileVisibilityService {
    db: Option<Arc<Database>>,
    _profile_service: Option<Arc<ProfileService>>,
}

impl ProfileVisibilityService {
    pub fn new(
        db: Option<Arc<Database>>,
        profile_service: Option<Arc<ProfileService>>,
    ) -> Self {
        Self {
            db,
            _profile_service: profile_service,
        }
    }

    pub async fn resolve_snapshot(
        &self,
        client_id: &str,
        profile_id_override: Option<&str>,
    ) -> Result<VisibilitySnapshot> {
        let db = self
            .db
            .as_ref()
            .context("Profile visibility requires database access")?;

        let capability_config = self
            .load_client_capability_config(client_id, profile_id_override)
            .await?;

        let profile_ids = self.resolve_profile_ids(&db.pool, &capability_config).await?;
        let server_ids = self
            .resolve_server_ids(&db.pool, capability_config.capability_source, &profile_ids)
            .await?;

        let policies = self.resolve_policies(&db.pool, &server_ids, &profile_ids).await?;
        let surface_fingerprint = compute_surface_fingerprint(&capability_config, &policies, None, None, None);
        let snapshot = build_snapshot(client_id, surface_fingerprint, profile_ids, server_ids, policies);

        tracing::debug!(
            client_id = %client_id,
            fingerprint = %snapshot.surface_fingerprint,
            "Resolved visibility snapshot"
        );

        Ok(snapshot)
    }

    pub async fn resolve_snapshot_for_client(
        &self,
        client_context: &ClientContext,
    ) -> Result<VisibilitySnapshot> {
        if matches!(client_context.config_mode.as_deref(), Some("unify")) {
            return self
                .resolve_unify_snapshot(
                    &client_context.client_id,
                    client_context.config_mode.as_deref(),
                    client_context.unify_workspace.as_ref(),
                )
                .await;
        }

        let capability_config = self.resolve_capability_config_for_client(client_context).await?;
        self.resolve_snapshot_from_config(
            &client_context.client_id,
            &capability_config,
            client_context.config_mode.as_deref(),
            client_context.unify_workspace.as_ref(),
        )
        .await
    }

    pub async fn resolve_capability_config(
        &self,
        client_id: &str,
    ) -> Result<ClientCapabilityConfig> {
        self.load_client_capability_config(client_id, None).await
    }

    pub async fn resolve_capability_config_for_client(
        &self,
        client_context: &ClientContext,
    ) -> Result<ClientCapabilityConfig> {
        if matches!(client_context.config_mode.as_deref(), Some("unify")) {
            return Ok(Self::active_capability_config());
        }

        self.load_client_capability_config(&client_context.client_id, client_context.profile_id.as_deref())
            .await
    }

    async fn resolve_policies(
        &self,
        pool: &sqlx::Pool<sqlx::Sqlite>,
        server_ids: &[String],
        profile_ids: &[String],
    ) -> Result<ResolvedPolicies> {
        let (allowed_tools, has_tool_policy) = self.resolve_allowed_tools(pool, server_ids, profile_ids).await?;
        let (allowed_resources, has_resource_policy) =
            self.resolve_allowed_resources(pool, server_ids, profile_ids).await?;
        let (allowed_resource_templates, has_resource_template_policy) = self
            .resolve_allowed_resource_templates(pool, server_ids, profile_ids)
            .await?;
        let (allowed_prompts, has_prompt_policy) = self.resolve_allowed_prompts(pool, server_ids, profile_ids).await?;

        Ok(ResolvedPolicies {
            allowed_tools,
            allowed_resources,
            allowed_resource_templates,
            allowed_prompts,
            has_tool_policy,
            has_resource_policy,
            has_resource_template_policy,
            has_prompt_policy,
        })
    }

    async fn resolve_snapshot_from_config(
        &self,
        client_id: &str,
        capability_config: &ClientCapabilityConfig,
        config_mode: Option<&str>,
        unify_workspace: Option<&UnifyDirectExposureConfig>,
    ) -> Result<VisibilitySnapshot> {
        let db = self
            .db
            .as_ref()
            .context("Profile visibility requires database access")?;

        let profile_ids = self.resolve_profile_ids(&db.pool, capability_config).await?;
        let server_ids = self
            .resolve_server_ids(&db.pool, capability_config.capability_source, &profile_ids)
            .await?;

        let policies = self.resolve_policies(&db.pool, &server_ids, &profile_ids).await?;
        let direct_surface_fingerprint = self.compute_unify_direct_surface_fingerprint(unify_workspace).await?;

        let surface_fingerprint = compute_surface_fingerprint(
            capability_config,
            &policies,
            config_mode,
            direct_surface_fingerprint.as_deref(),
            Some(builtin_tool_surface_ids(
                config_mode,
                capability_config.capability_source,
            )),
        );

        let snapshot = build_snapshot(client_id, surface_fingerprint, profile_ids, server_ids, policies);

        Ok(snapshot)
    }

    async fn resolve_unify_snapshot(
        &self,
        client_id: &str,
        config_mode: Option<&str>,
        unify_workspace: Option<&UnifyDirectExposureConfig>,
    ) -> Result<VisibilitySnapshot> {
        let db = self
            .db
            .as_ref()
            .context("Profile visibility requires database access")?;

        let capability_config = Self::active_capability_config();

        let profile_ids = Vec::new();
        let server_ids = self.resolve_globally_enabled_server_ids(&db.pool).await?;

        let policies = self.resolve_policies(&db.pool, &server_ids, &profile_ids).await?;
        let direct_surface_fingerprint = self.compute_unify_direct_surface_fingerprint(unify_workspace).await?;

        let surface_fingerprint = compute_surface_fingerprint(
            &capability_config,
            &policies,
            config_mode,
            direct_surface_fingerprint.as_deref(),
            Some(builtin_tool_surface_ids(
                config_mode,
                capability_config.capability_source,
            )),
        );

        let snapshot = build_snapshot(client_id, surface_fingerprint, profile_ids, server_ids, policies);

        Ok(snapshot)
    }

    async fn compute_unify_direct_surface_fingerprint(
        &self,
        unify_workspace: Option<&UnifyDirectExposureConfig>,
    ) -> Result<Option<String>> {
        let Some(workspace) = unify_workspace else {
            return Ok(None);
        };

        let tool_surfaces = collect_sorted_surfaces(&workspace.selected_tool_surfaces, |surface| {
            format!("{}\u{1e}{}", surface.server_id, surface.tool_name)
        });
        let prompt_surfaces = collect_sorted_surfaces(&workspace.selected_prompt_surfaces, |surface| {
            format!("{}\u{1e}{}", surface.server_id, surface.prompt_name)
        });
        let resource_surfaces = collect_sorted_surfaces(&workspace.selected_resource_surfaces, |surface| {
            format!("{}\u{1e}{}", surface.server_id, surface.resource_uri)
        });
        let template_surfaces = collect_sorted_surfaces(&workspace.selected_template_surfaces, |surface| {
            format!("{}\u{1e}{}", surface.server_id, surface.uri_template)
        });

        let mut selected_server_ids = workspace.selected_server_ids.clone();
        selected_server_ids.sort();
        selected_server_ids.dedup();

        let mut hasher = Sha256::new();
        hasher.update(workspace.route_mode.as_str());
        hasher.update([0]);
        hasher.update(selected_server_ids.join("\u{1f}"));
        hasher.update([0]);
        hasher.update(tool_surfaces.join("\u{1f}"));
        hasher.update([0]);
        hasher.update(prompt_surfaces.join("\u{1f}"));
        hasher.update([0]);
        hasher.update(resource_surfaces.join("\u{1f}"));
        hasher.update([0]);
        hasher.update(template_surfaces.join("\u{1f}"));
        Ok(Some(format!("{:x}", hasher.finalize())))
    }

    pub async fn filter_tools_for_client(
        &self,
        client_context: &ClientContext,
        tools: Vec<rmcp::model::Tool>,
    ) -> Result<Vec<rmcp::model::Tool>> {
        let snapshot = self.resolve_snapshot_for_client(client_context).await?;
        Ok(self.filter_tools_with_snapshot(&snapshot, tools))
    }

    pub async fn filter_resources_for_client(
        &self,
        client_context: &ClientContext,
        resources: Vec<rmcp::model::Resource>,
        templates: Vec<rmcp::model::ResourceTemplate>,
    ) -> Result<(Vec<rmcp::model::Resource>, Vec<rmcp::model::ResourceTemplate>)> {
        let snapshot = self.resolve_snapshot_for_client(client_context).await?;
        Ok(self.filter_resources_with_snapshot(&snapshot, resources, templates))
    }

    pub async fn filter_prompts_for_client(
        &self,
        client_context: &ClientContext,
        prompts: Vec<rmcp::model::Prompt>,
    ) -> Result<Vec<rmcp::model::Prompt>> {
        let snapshot = self.resolve_snapshot_for_client(client_context).await?;
        Ok(self.filter_prompts_with_snapshot(&snapshot, prompts))
    }

    pub async fn assert_tool_allowed(
        &self,
        client_context: &ClientContext,
        unique_tool_name: &str,
    ) -> Result<()> {
        let snapshot = self.resolve_snapshot_for_client(client_context).await?;
        self.assert_tool_allowed_with_snapshot(&snapshot, unique_tool_name)
            .await
    }

    pub async fn assert_resource_allowed(
        &self,
        client_context: &ClientContext,
        unique_resource_uri: &str,
    ) -> Result<()> {
        let snapshot = self.resolve_snapshot_for_client(client_context).await?;
        self.assert_resource_allowed_with_snapshot(&snapshot, unique_resource_uri)
            .await
    }

    pub async fn assert_prompt_allowed(
        &self,
        client_context: &ClientContext,
        unique_prompt_name: &str,
    ) -> Result<()> {
        let snapshot = self.resolve_snapshot_for_client(client_context).await?;
        self.assert_prompt_allowed_with_snapshot(&snapshot, unique_prompt_name)
            .await
    }

    pub fn filter_tools_with_snapshot(
        &self,
        snapshot: &VisibilitySnapshot,
        mut tools: Vec<rmcp::model::Tool>,
    ) -> Vec<rmcp::model::Tool> {
        if snapshot.server_ids.is_empty() {
            return Vec::new();
        }

        if !snapshot.has_tool_policy {
            return tools;
        }

        tools.retain(|tool| snapshot.allowed_tools.contains(tool.name.as_ref()));
        tools
    }

    pub fn filter_resources_with_snapshot(
        &self,
        snapshot: &VisibilitySnapshot,
        mut resources: Vec<rmcp::model::Resource>,
        mut templates: Vec<rmcp::model::ResourceTemplate>,
    ) -> (Vec<rmcp::model::Resource>, Vec<rmcp::model::ResourceTemplate>) {
        if snapshot.server_ids.is_empty() {
            return (Vec::new(), Vec::new());
        }

        if snapshot.has_resource_policy || snapshot.has_resource_template_policy {
            resources.retain(|resource| resource_allowed_from_snapshot(snapshot, resource.uri.as_str()));
        }

        if snapshot.has_resource_template_policy {
            templates.retain(|template| {
                snapshot
                    .allowed_resource_templates
                    .contains(template.uri_template.as_str())
            });
        }

        (resources, templates)
    }

    pub fn filter_prompts_with_snapshot(
        &self,
        snapshot: &VisibilitySnapshot,
        mut prompts: Vec<rmcp::model::Prompt>,
    ) -> Vec<rmcp::model::Prompt> {
        if snapshot.server_ids.is_empty() {
            return Vec::new();
        }

        if !snapshot.has_prompt_policy {
            return prompts;
        }

        prompts.retain(|prompt| snapshot.allowed_prompts.contains(prompt.name.as_str()));
        prompts
    }

    pub async fn assert_tool_allowed_with_snapshot(
        &self,
        snapshot: &VisibilitySnapshot,
        unique_tool_name: &str,
    ) -> Result<()> {
        ensure_allowed(
            self.snapshot_allows_tool(snapshot, unique_tool_name).await?,
            format!("Tool '{unique_tool_name}' is not available for this client"),
        )
    }

    pub async fn assert_resource_allowed_with_snapshot(
        &self,
        snapshot: &VisibilitySnapshot,
        unique_resource_uri: &str,
    ) -> Result<()> {
        ensure_allowed(
            self.snapshot_allows_resource(snapshot, unique_resource_uri).await?,
            format!("Resource '{unique_resource_uri}' is not available for this client"),
        )
    }

    pub async fn assert_prompt_allowed_with_snapshot(
        &self,
        snapshot: &VisibilitySnapshot,
        unique_prompt_name: &str,
    ) -> Result<()> {
        ensure_allowed(
            self.snapshot_allows_prompt(snapshot, unique_prompt_name).await?,
            format!("Prompt '{unique_prompt_name}' is not available for this client"),
        )
    }

    async fn snapshot_allows_tool(
        &self,
        snapshot: &VisibilitySnapshot,
        unique_tool_name: &str,
    ) -> Result<bool> {
        if snapshot.server_ids.is_empty() {
            return Ok(false);
        }

        if snapshot.has_tool_policy {
            return Ok(snapshot.allowed_tools.contains(unique_tool_name));
        }

        self.snapshot_allows_server(NamingKind::Tool, snapshot, unique_tool_name)
            .await
    }

    async fn snapshot_allows_resource(
        &self,
        snapshot: &VisibilitySnapshot,
        unique_resource_uri: &str,
    ) -> Result<bool> {
        if snapshot.server_ids.is_empty() {
            return Ok(false);
        }

        let db = self
            .db
            .as_ref()
            .context("Profile visibility requires database access")?;
        let route = match resolve_resource_route(&db.pool, unique_resource_uri).await {
            Ok(route) => route,
            Err(_) => return Ok(false),
        };
        if !snapshot
            .server_ids
            .iter()
            .any(|candidate| candidate == &route.server_id)
        {
            return Ok(false);
        }
        if snapshot.has_resource_policy || snapshot.has_resource_template_policy {
            if snapshot.has_resource_policy && snapshot.allowed_resources.contains(unique_resource_uri) {
                return Ok(true);
            }
            if snapshot.has_resource_template_policy {
                let ResourceRouteSource::Template { upstream_template, .. } = &route.source else {
                    return Ok(false);
                };
                let external_template = crate::core::capability::naming::load_external_identifier(
                    &db.pool,
                    NamingKind::ResourceTemplate,
                    &route.server_id,
                    upstream_template,
                )
                .await?;
                return Ok(snapshot.allowed_resource_templates.contains(&external_template));
            }
            return Ok(false);
        }
        Ok(true)
    }

    async fn snapshot_allows_prompt(
        &self,
        snapshot: &VisibilitySnapshot,
        unique_prompt_name: &str,
    ) -> Result<bool> {
        if snapshot.server_ids.is_empty() {
            return Ok(false);
        }

        if snapshot.has_prompt_policy {
            return Ok(snapshot.allowed_prompts.contains(unique_prompt_name));
        }

        self.snapshot_allows_server(NamingKind::Prompt, snapshot, unique_prompt_name)
            .await
    }

    async fn snapshot_allows_server(
        &self,
        kind: NamingKind,
        snapshot: &VisibilitySnapshot,
        unique_value: &str,
    ) -> Result<bool> {
        let route = resolve_capability_route(kind, unique_value)
            .await
            .with_context(|| format!("Failed to resolve canonical capability name '{unique_value}'"))?;
        Ok(snapshot
            .server_ids
            .iter()
            .any(|candidate| candidate == &route.server_id))
    }

    async fn load_client_capability_config(
        &self,
        client_id: &str,
        profile_id_override: Option<&str>,
    ) -> Result<ClientCapabilityConfig> {
        if let Some(profile_id) = profile_id_override {
            tracing::info!(
                client_id = %client_id,
                profile_id = %profile_id,
                "Using profile_id override from URL parameter"
            );
            return Ok(Self::custom_capability_config(profile_id));
        }

        let db = self
            .db
            .as_ref()
            .context("Profile visibility requires database access")?;

        let row_opt = sqlx::query_as::<_, ClientCapabilityRow>(
            r#"
            SELECT capability_source, selected_profile_ids, custom_profile_id
            FROM client
            WHERE identifier = ?
            "#,
        )
        .bind(client_id)
        .fetch_optional(&db.pool)
        .await
        .with_context(|| format!("Failed to load client capability config for '{client_id}'"))?;

        if let Some(row) = row_opt {
            return ClientCapabilityConfig::from_parts(
                row.capability_source.as_deref(),
                row.selected_profile_ids.as_deref(),
                row.custom_profile_id,
            )
            .map_err(|error| anyhow!(error));
        }

        tracing::warn!(
            client_id = %client_id,
            "Client not configured in database, using active profile as fallback"
        );

        Ok(Self::active_capability_config())
    }

    fn custom_capability_config(profile_id: &str) -> ClientCapabilityConfig {
        ClientCapabilityConfig {
            capability_source: CapabilitySource::Custom,
            selected_profile_ids: vec![],
            custom_profile_id: Some(profile_id.to_string()),
        }
    }

    fn active_capability_config() -> ClientCapabilityConfig {
        ClientCapabilityConfig {
            capability_source: CapabilitySource::Activated,
            selected_profile_ids: vec![],
            custom_profile_id: None,
        }
    }

    async fn resolve_profile_ids(
        &self,
        pool: &sqlx::Pool<sqlx::Sqlite>,
        capability_config: &ClientCapabilityConfig,
    ) -> Result<Vec<String>> {
        let mut profile_ids = match capability_config.capability_source {
            CapabilitySource::Activated => get_active_profile(pool)
                .await
                .context("Failed to load active profiles")?
                .into_iter()
                .filter_map(|profile| profile.id)
                .collect(),
            CapabilitySource::Profiles => capability_config.selected_profile_ids.clone(),
            CapabilitySource::Custom => vec![
                capability_config
                    .custom_profile_id
                    .clone()
                    .ok_or_else(|| anyhow!("Custom capability source requires custom_profile_id"))?,
            ],
        };

        profile_ids.sort();
        profile_ids.dedup();
        Ok(profile_ids)
    }

    async fn resolve_server_ids(
        &self,
        pool: &sqlx::Pool<sqlx::Sqlite>,
        capability_source: CapabilitySource,
        profile_ids: &[String],
    ) -> Result<Vec<String>> {
        let mut server_ids = if profile_ids.is_empty() {
            if capability_source == CapabilitySource::Activated {
                self.resolve_globally_enabled_server_ids(pool).await?
            } else {
                Vec::new()
            }
        } else {
            let placeholders = repeat_placeholders(profile_ids.len());
            let sql = format!(
                r#"
                SELECT DISTINCT sc.id
                FROM server_config sc
                WHERE sc.enabled = 1
                  AND (
                    EXISTS (
                      SELECT 1
                      FROM profile_server_relationships psr
                      WHERE psr.server_id = sc.id
                        AND psr.enabled = 1
                        AND psr.profile_id IN ({placeholders})
                    )
                    OR EXISTS (
                      SELECT 1
                      FROM profile_capability_refs pcr
                      JOIN capability_refs cr ON cr.ref_id = pcr.ref_id
                      WHERE cr.server_id = sc.id
                        AND pcr.enabled = 1
                        AND pcr.profile_id IN ({placeholders})
                        AND NOT EXISTS (
                          SELECT 1
                          FROM profile_server_relationships gate
                          WHERE gate.profile_id = pcr.profile_id
                            AND gate.server_id = cr.server_id
                            AND gate.enabled = 0
                        )
                    )
                  )
                ORDER BY sc.name, sc.id
                "#,
            );

            let mut query = sqlx::query_scalar::<_, String>(&sql);
            for profile_id in profile_ids {
                query = query.bind(profile_id);
            }
            for profile_id in profile_ids {
                query = query.bind(profile_id);
            }
            query
                .fetch_all(pool)
                .await
                .context("Failed to resolve visible servers for client snapshot")?
        };

        server_ids.sort();
        server_ids.dedup();
        Ok(server_ids)
    }

    async fn resolve_globally_enabled_server_ids(
        &self,
        pool: &sqlx::Pool<sqlx::Sqlite>,
    ) -> Result<Vec<String>> {
        let mut server_ids = sqlx::query_scalar::<_, String>(
            r#"
            SELECT id
            FROM server_config
            WHERE enabled = 1
            ORDER BY name, id
            "#,
        )
        .fetch_all(pool)
        .await
        .context("Failed to load globally enabled servers for visibility snapshot")?;

        server_ids.sort();
        server_ids.dedup();
        Ok(server_ids)
    }

    async fn resolve_allowed_tools(
        &self,
        pool: &sqlx::Pool<sqlx::Sqlite>,
        server_ids: &[String],
        profile_ids: &[String],
    ) -> Result<(HashSet<String>, bool)> {
        self.resolve_allowed_kind(
            pool,
            server_ids,
            profile_ids,
            "tools",
            "server_tools",
            "unique_name",
            "tool_name",
        )
        .await
    }

    async fn resolve_allowed_resources(
        &self,
        pool: &sqlx::Pool<sqlx::Sqlite>,
        server_ids: &[String],
        profile_ids: &[String],
    ) -> Result<(HashSet<String>, bool)> {
        self.resolve_allowed_kind(
            pool,
            server_ids,
            profile_ids,
            "resources",
            "server_resources",
            "unique_uri",
            "resource_uri",
        )
        .await
    }

    async fn resolve_allowed_resource_templates(
        &self,
        pool: &sqlx::Pool<sqlx::Sqlite>,
        server_ids: &[String],
        profile_ids: &[String],
    ) -> Result<(HashSet<String>, bool)> {
        self.resolve_allowed_kind(
            pool,
            server_ids,
            profile_ids,
            "resource_templates",
            "server_resource_templates",
            "unique_name",
            "uri_template",
        )
        .await
    }

    async fn resolve_allowed_prompts(
        &self,
        pool: &sqlx::Pool<sqlx::Sqlite>,
        server_ids: &[String],
        profile_ids: &[String],
    ) -> Result<(HashSet<String>, bool)> {
        self.resolve_allowed_kind(
            pool,
            server_ids,
            profile_ids,
            "prompts",
            "server_prompts",
            "unique_name",
            "prompt_name",
        )
        .await
    }

    async fn resolve_allowed_kind(
        &self,
        pool: &sqlx::Pool<sqlx::Sqlite>,
        server_ids: &[String],
        profile_ids: &[String],
        kind: &str,
        projection_table: &str,
        external_column: &str,
        origin_column: &str,
    ) -> Result<(HashSet<String>, bool)> {
        if server_ids.is_empty() {
            return Ok((HashSet::new(), false));
        }
        if profile_ids.is_empty() {
            let server_placeholders = repeat_placeholders(server_ids.len());
            let sql = format!(
                r#"
                SELECT DISTINCT projection.{external_column}
                FROM capability_refs cr
                JOIN {projection_table} projection
                  ON projection.server_id = cr.server_id
                 AND projection.{origin_column} = cr.origin_key
                WHERE cr.kind = ?
                  AND cr.state = 'active'
                  AND cr.server_id IN ({server_placeholders})
                "#
            );
            let mut query = sqlx::query_scalar::<_, String>(&sql).bind(kind);
            for server_id in server_ids {
                query = query.bind(server_id);
            }
            let values = query
                .fetch_all(pool)
                .await
                .context("Failed to expand globally active CapabilityRefs")?;
            return Ok((values.into_iter().collect(), false));
        }
        let profile_placeholders = repeat_placeholders(profile_ids.len());
        let server_placeholders = repeat_placeholders(server_ids.len());
        let policy_sql = format!(
            r#"
            SELECT EXISTS (
              SELECT 1
              FROM profile_capability_refs pcr
              JOIN capability_refs cr ON cr.ref_id = pcr.ref_id
              WHERE pcr.profile_id IN ({profile_placeholders})
                AND cr.kind = ?
              UNION ALL
              SELECT 1
              FROM profile_server_relationships psr
              WHERE psr.profile_id IN ({profile_placeholders})
            )
            "#
        );
        let mut policy_query = sqlx::query_scalar::<_, bool>(&policy_sql);
        for profile_id in profile_ids {
            policy_query = policy_query.bind(profile_id);
        }
        policy_query = policy_query.bind(kind);
        for profile_id in profile_ids {
            policy_query = policy_query.bind(profile_id);
        }
        let has_policy = policy_query
            .fetch_one(pool)
            .await
            .context("Failed to resolve authoring policy presence")?;
        if !has_policy {
            return Ok((HashSet::new(), false));
        }

        let values_sql = format!(
            r#"
            SELECT DISTINCT projection.{external_column}
            FROM capability_refs cr
            JOIN server_config sc ON sc.id = cr.server_id
            JOIN {projection_table} projection
              ON projection.server_id = cr.server_id
             AND projection.{origin_column} = cr.origin_key
            WHERE cr.kind = ?
              AND cr.state = 'active'
              AND cr.server_id IN ({server_placeholders})
              AND sc.enabled = 1
              AND (
                EXISTS (
                  SELECT 1
                  FROM profile_capability_refs pcr
                  WHERE pcr.ref_id = cr.ref_id
                    AND pcr.enabled = 1
                    AND pcr.profile_id IN ({profile_placeholders})
                    AND NOT EXISTS (
                      SELECT 1
                      FROM profile_server_relationships gate
                      WHERE gate.profile_id = pcr.profile_id
                        AND gate.server_id = cr.server_id
                        AND gate.enabled = 0
                    )
                )
                OR EXISTS (
                  SELECT 1
                  FROM profile_server_relationships psr
                  WHERE psr.server_id = cr.server_id
                    AND psr.enabled = 1
                    AND psr.profile_id IN ({profile_placeholders})
                )
              )
            "#
        );
        let mut values_query = sqlx::query_scalar::<_, String>(&values_sql).bind(kind);
        for server_id in server_ids {
            values_query = values_query.bind(server_id);
        }
        for profile_id in profile_ids {
            values_query = values_query.bind(profile_id);
        }
        for profile_id in profile_ids {
            values_query = values_query.bind(profile_id);
        }
        let values = values_query
            .fetch_all(pool)
            .await
            .context("Failed to expand Profile authoring relationships")?;
        Ok((values.into_iter().collect(), true))
    }
}

fn ensure_allowed(
    allowed: bool,
    message: String,
) -> Result<()> {
    if allowed { Ok(()) } else { Err(anyhow!(message)) }
}

fn resource_allowed_from_snapshot(
    snapshot: &VisibilitySnapshot,
    unique_uri: &str,
) -> bool {
    if snapshot.server_ids.is_empty() {
        return false;
    }

    if !snapshot.has_resource_policy && !snapshot.has_resource_template_policy {
        return true;
    }

    if snapshot.has_resource_policy && snapshot.allowed_resources.contains(unique_uri) {
        return true;
    }

    false
}

fn repeat_placeholders(count: usize) -> String {
    vec!["?"; count].join(", ")
}

fn compute_surface_fingerprint(
    capability_config: &ClientCapabilityConfig,
    policies: &ResolvedPolicies,
    config_mode: Option<&str>,
    direct_surface_fingerprint: Option<&str>,
    builtin_tools: Option<Vec<&str>>,
) -> String {
    compute_surface_hash(SurfaceFingerprintInput {
        capability_config,
        allowed_tools: &policies.allowed_tools,
        allowed_resources: &policies.allowed_resources,
        allowed_resource_templates: &policies.allowed_resource_templates,
        allowed_prompts: &policies.allowed_prompts,
        policy_flags: policies.policy_flags(),
        config_mode,
        direct_surface_fingerprint,
        builtin_tools,
    })
}

fn build_snapshot(
    client_id: &str,
    surface_fingerprint: String,
    profile_ids: Vec<String>,
    server_ids: Vec<String>,
    policies: ResolvedPolicies,
) -> VisibilitySnapshot {
    VisibilitySnapshot {
        client_id: client_id.to_string(),
        surface_fingerprint,
        profile_ids,
        server_ids,
        allowed_tools: policies.allowed_tools,
        allowed_resources: policies.allowed_resources,
        allowed_resource_templates: policies.allowed_resource_templates,
        allowed_prompts: policies.allowed_prompts,
        has_tool_policy: policies.has_tool_policy,
        has_resource_policy: policies.has_resource_policy,
        has_resource_template_policy: policies.has_resource_template_policy,
        has_prompt_policy: policies.has_prompt_policy,
    }
}

struct SurfaceFingerprintInput<'a> {
    capability_config: &'a ClientCapabilityConfig,
    allowed_tools: &'a HashSet<String>,
    allowed_resources: &'a HashSet<String>,
    allowed_resource_templates: &'a HashSet<String>,
    allowed_prompts: &'a HashSet<String>,
    policy_flags: [bool; 4],
    config_mode: Option<&'a str>,
    direct_surface_fingerprint: Option<&'a str>,
    builtin_tools: Option<Vec<&'a str>>,
}

fn compute_surface_hash(input: SurfaceFingerprintInput<'_>) -> String {
    let mut hasher = Sha256::new();

    hasher.update(input.config_mode.unwrap_or("hosted"));
    hasher.update([0]);
    hasher.update(sorted_values(input.allowed_tools).join("\u{1f}"));
    hasher.update([0]);
    hasher.update(sorted_values(input.allowed_resources).join("\u{1f}"));
    hasher.update([0]);
    hasher.update(sorted_values(input.allowed_resource_templates).join("\u{1f}"));
    hasher.update([0]);
    hasher.update(sorted_values(input.allowed_prompts).join("\u{1f}"));
    hasher.update([0]);

    let mut builtin_tools = input
        .builtin_tools
        .unwrap_or_default()
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    builtin_tools.sort();
    builtin_tools.dedup();
    hasher.update(builtin_tools.join("\u{1f}"));
    hasher.update([0]);

    hasher.update(input.capability_config.capability_source.as_str());
    hasher.update([0]);

    if let Some(direct_surface_fingerprint) = input.direct_surface_fingerprint {
        hasher.update(direct_surface_fingerprint);
    }
    hasher.update([0]);

    for flag in input.policy_flags {
        hasher.update([u8::from(flag)]);
    }
    format!("{:x}", hasher.finalize())
}

fn sorted_values(values: &HashSet<String>) -> Vec<String> {
    let mut sorted = values.iter().cloned().collect::<Vec<_>>();
    sorted.sort();
    sorted
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::clients::models::CapabilitySource;
    use crate::common::profile::ProfileType;
    use crate::common::server::ServerType;
    use crate::config::{
        client::init::initialize_client_table,
        models::{Profile, Server},
        profile::{self, init::initialize_profile_tables},
        server::{self, init::initialize_server_tables},
    };
    use crate::core::capability::resource_uri::{encode_resource_template, encode_resource_uri};
    use mcpmate_capability_store::{
        CapabilityCatalog, CapabilityKind as CatalogCapabilityKind, CapabilityObservation, CapabilityPayload,
        CatalogRecord, DeclarationState, InventoryState, KindObservation, SqliteCapabilityCatalog,
    };
    use serial_test::serial;
    use sqlx::sqlite::SqlitePoolOptions;
    use tempfile::TempDir;

    async fn create_visibility_service() -> (TempDir, Arc<Database>, ProfileVisibilityService) {
        let temp_dir = TempDir::new().expect("temp dir");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");

        crate::helpers::prepare_config_database(&pool).await;
        initialize_server_tables(&pool).await.expect("init server tables");
        initialize_client_table(&pool).await.expect("init client table");
        crate::config::database::initialize_capability_catalog(&pool)
            .await
            .expect("init capability catalog");
        initialize_profile_tables(&pool).await.expect("init profile tables");
        crate::config::client::init::initialize_system_settings(&pool)
            .await
            .expect("init system settings table");

        crate::core::capability::naming::initialize(pool.clone());

        let db = Arc::new(Database {
            pool,
            path: temp_dir.path().join("test.db"),
            capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
        });

        let service = ProfileVisibilityService::new(Some(db.clone()), None);
        (temp_dir, db, service)
    }

    async fn insert_profile(
        db: &Arc<Database>,
        name: &str,
        profile_type: ProfileType,
        is_active: bool,
    ) -> String {
        let mut profile = Profile::new(name.to_string(), profile_type);
        profile.is_active = is_active;
        crate::helpers::insert_profile(&db.pool, &profile).await
    }

    async fn insert_server(
        db: &Arc<Database>,
        name: &str,
    ) -> String {
        let server = Server::new(name.to_string(), ServerType::Stdio);
        server::upsert_server(&db.pool, &server).await.expect("upsert server")
    }

    async fn seed_tool(
        db: &Arc<Database>,
        profile_id: &str,
        server_id: &str,
        server_name: &str,
        tool_name: &str,
    ) -> String {
        let result =
            crate::config::server::tools::upsert_server_tool(&db.pool, server_id, server_name, tool_name, None)
                .await
                .expect("upsert server tool");
        let tool: rmcp::model::Tool = serde_json::from_value(serde_json::json!({
            "name": tool_name,
            "inputSchema": {"type": "object"}
        }))
        .expect("build Tool");
        let ref_id = commit_catalog_record(
            db,
            server_id,
            server_name,
            CatalogRecord::materialize(server_id, tool_name, &result.unique_name, CapabilityPayload::Tool(tool))
                .expect("materialize Tool"),
        )
        .await;
        profile::add_tool_to_profile(&db.pool, profile_id, server_id, &ref_id, true)
            .await
            .expect("add tool to profile");
        result.unique_name
    }

    async fn seed_prompt(
        db: &Arc<Database>,
        profile_id: &str,
        server_id: &str,
        server_name: &str,
        prompt_name: &str,
    ) -> String {
        let unique_name = crate::config::server::capabilities::upsert_shadow_prompt(
            &db.pool,
            server_id,
            server_name,
            prompt_name,
            None,
        )
        .await
        .expect("upsert server prompt");
        let prompt: rmcp::model::Prompt = serde_json::from_value(serde_json::json!({
            "name": prompt_name,
            "arguments": []
        }))
        .expect("build Prompt");
        let ref_id = commit_catalog_record(
            db,
            server_id,
            server_name,
            CatalogRecord::materialize(server_id, prompt_name, &unique_name, CapabilityPayload::Prompt(prompt))
                .expect("materialize Prompt"),
        )
        .await;
        profile::add_prompt_to_profile(&db.pool, profile_id, server_id, &ref_id, true)
            .await
            .expect("add prompt to profile");
        unique_name
    }

    async fn seed_resource(
        db: &Arc<Database>,
        profile_id: &str,
        server_id: &str,
        server_name: &str,
        resource_uri: &str,
    ) -> String {
        let unique_uri = crate::config::server::capabilities::upsert_shadow_resource(
            &db.pool,
            server_id,
            server_name,
            resource_uri,
            None,
            None,
            None,
        )
        .await
        .expect("upsert server resource");
        let resource: rmcp::model::Resource =
            serde_json::from_value(serde_json::json!({"uri": resource_uri, "name": resource_uri}))
                .expect("build Resource");
        let ref_id = commit_catalog_record(
            db,
            server_id,
            server_name,
            CatalogRecord::materialize(
                server_id,
                resource_uri,
                &unique_uri,
                CapabilityPayload::Resource(resource),
            )
            .expect("materialize Resource"),
        )
        .await;
        profile::add_resource_to_profile(&db.pool, profile_id, server_id, &ref_id, true)
            .await
            .expect("add resource to profile");
        unique_uri
    }

    async fn seed_resource_template(
        db: &Arc<Database>,
        profile_id: &str,
        server_id: &str,
        server_name: &str,
        uri_template: &str,
    ) -> String {
        let unique_name = crate::config::server::capabilities::upsert_shadow_resource_template(
            &db.pool,
            server_id,
            server_name,
            uri_template,
            Some(uri_template),
            None,
        )
        .await
        .expect("upsert server resource template");
        let template: rmcp::model::ResourceTemplate =
            serde_json::from_value(serde_json::json!({"uriTemplate": uri_template, "name": uri_template}))
                .expect("build Resource Template");
        let ref_id = commit_catalog_record(
            db,
            server_id,
            server_name,
            CatalogRecord::materialize(
                server_id,
                uri_template,
                &unique_name,
                CapabilityPayload::ResourceTemplate(template),
            )
            .expect("materialize Resource Template"),
        )
        .await;
        profile::add_resource_template_to_profile(&db.pool, profile_id, server_id, &ref_id, true)
            .await
            .expect("add resource template to profile");
        unique_name
    }

    async fn commit_catalog_record(
        db: &Arc<Database>,
        server_id: &str,
        server_name: &str,
        record: CatalogRecord,
    ) -> String {
        let kind = record.kind();
        let ref_id = record.ref_id.to_string();
        let capabilities = match kind {
            CatalogCapabilityKind::Tools => serde_json::json!({"tools": {}}),
            CatalogCapabilityKind::Prompts => serde_json::json!({"prompts": {}}),
            CatalogCapabilityKind::Resources | CatalogCapabilityKind::ResourceTemplates => {
                serde_json::json!({"resources": {}})
            }
        };
        let initialize_result: rmcp::model::InitializeResult = serde_json::from_value(serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": capabilities,
            "serverInfo": {"name": server_name, "version": "1.0.0"}
        }))
        .expect("build InitializeResult");
        SqliteCapabilityCatalog::new(db.pool.clone())
            .commit_observation(CapabilityObservation::new(
                server_id,
                server_name,
                "test-config",
                initialize_result,
                vec![KindObservation::new(
                    kind,
                    DeclarationState::Supported,
                    InventoryState::Complete,
                )],
                vec![record],
            ))
            .await
            .expect("commit catalog observation");
        ref_id
    }

    async fn insert_client_config(
        db: &Arc<Database>,
        identifier: &str,
        capability_source: CapabilitySource,
        selected_profile_ids: Vec<String>,
        custom_profile_id: Option<String>,
    ) {
        let selected_profile_ids_json = if selected_profile_ids.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&selected_profile_ids).expect("selected profile ids json"))
        };

        sqlx::query(
            r#"
            INSERT INTO client (id, name, identifier, capability_source, selected_profile_ids, custom_profile_id)
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(crate::generate_id!("clnt"))
        .bind(identifier)
        .bind(identifier)
        .bind(capability_source.as_str())
        .bind(selected_profile_ids_json)
        .bind(custom_profile_id)
        .execute(&db.pool)
        .await
        .expect("insert client config");
    }

    #[tokio::test]
    #[serial]
    async fn resolve_snapshot_uses_active_profiles_for_activated_mode() {
        let (_temp_dir, db, service) = create_visibility_service().await;

        let active_profile_id = insert_profile(&db, "active", ProfileType::Shared, true).await;
        let inactive_profile_id = insert_profile(&db, "inactive", ProfileType::Shared, false).await;
        let active_server_id = insert_server(&db, "active_server").await;
        let inactive_server_id = insert_server(&db, "inactive_server").await;

        profile::add_server_to_profile(&db.pool, &active_profile_id, &active_server_id, true)
            .await
            .expect("add active server");
        profile::add_server_to_profile(&db.pool, &inactive_profile_id, &inactive_server_id, true)
            .await
            .expect("add inactive server");

        let active_tool = seed_tool(
            &db,
            &active_profile_id,
            &active_server_id,
            "active_server",
            "tool_alpha",
        )
        .await;
        let _inactive_tool = seed_tool(
            &db,
            &inactive_profile_id,
            &inactive_server_id,
            "inactive_server",
            "tool_beta",
        )
        .await;

        insert_client_config(&db, "client-a", CapabilitySource::Activated, Vec::new(), None).await;

        let snapshot = service
            .resolve_snapshot("client-a", None)
            .await
            .expect("resolve snapshot");

        assert_eq!(snapshot.profile_ids, vec![active_profile_id]);
        assert_eq!(snapshot.server_ids, vec![active_server_id]);
        assert!(snapshot.allowed_tools.contains(&active_tool));
        assert_eq!(snapshot.allowed_tools.len(), 1);
        assert!(!snapshot.surface_fingerprint.is_empty());
    }

    #[tokio::test]
    #[serial]
    async fn resolve_snapshot_uses_selected_profiles_for_profiles_mode() {
        let (_temp_dir, db, service) = create_visibility_service().await;

        let active_profile_id = insert_profile(&db, "active", ProfileType::Shared, true).await;
        let selected_profile_id = insert_profile(&db, "selected", ProfileType::Shared, false).await;
        let active_server_id = insert_server(&db, "active_server").await;
        let selected_server_id = insert_server(&db, "selected_server").await;

        profile::add_server_to_profile(&db.pool, &active_profile_id, &active_server_id, true)
            .await
            .expect("add active server");
        profile::add_server_to_profile(&db.pool, &selected_profile_id, &selected_server_id, true)
            .await
            .expect("add selected server");

        let selected_tool = seed_tool(
            &db,
            &selected_profile_id,
            &selected_server_id,
            "selected_server",
            "tool_selected",
        )
        .await;

        insert_client_config(
            &db,
            "client-b",
            CapabilitySource::Profiles,
            vec![selected_profile_id.clone()],
            None,
        )
        .await;

        let snapshot = service
            .resolve_snapshot("client-b", None)
            .await
            .expect("resolve snapshot");

        assert_eq!(snapshot.profile_ids, vec![selected_profile_id]);
        assert_eq!(snapshot.server_ids, vec![selected_server_id]);
        assert!(snapshot.allowed_tools.contains(&selected_tool));
        assert_eq!(snapshot.allowed_tools.len(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn resolve_snapshot_excludes_a_disabled_profile_server_without_erasing_capability_preferences() {
        let (_temp_dir, db, service) = create_visibility_service().await;

        let profile_id = insert_profile(&db, "selected", ProfileType::Shared, false).await;
        let server_id = insert_server(&db, "selected_server").await;
        profile::add_server_to_profile(&db.pool, &profile_id, &server_id, true)
            .await
            .expect("add selected server");
        let tool = seed_tool(&db, &profile_id, &server_id, "selected_server", "tool_selected").await;
        profile::add_server_to_profile(&db.pool, &profile_id, &server_id, false)
            .await
            .expect("disable selected server");
        insert_client_config(
            &db,
            "client-disabled-profile-server",
            CapabilitySource::Profiles,
            vec![profile_id.clone()],
            None,
        )
        .await;

        let snapshot = service
            .resolve_snapshot("client-disabled-profile-server", None)
            .await
            .expect("resolve snapshot");

        assert_eq!(snapshot.profile_ids, vec![profile_id]);
        assert!(snapshot.server_ids.is_empty());
        assert!(!snapshot.allowed_tools.contains(&tool));
        let saved_preference: bool =
            sqlx::query_scalar("SELECT enabled FROM profile_capability_refs WHERE profile_id = ?")
                .bind(&snapshot.profile_ids[0])
                .fetch_one(&db.pool)
                .await
                .expect("load saved capability preference");
        assert!(saved_preference);
    }

    #[tokio::test]
    #[serial]
    async fn resolve_snapshot_uses_custom_profile_for_custom_mode() {
        let (_temp_dir, db, service) = create_visibility_service().await;

        let custom_profile_id = insert_profile(&db, "custom", ProfileType::HostApp, false).await;
        let custom_server_id = insert_server(&db, "custom_server").await;

        profile::add_server_to_profile(&db.pool, &custom_profile_id, &custom_server_id, true)
            .await
            .expect("add custom server");

        let custom_prompt = seed_prompt(
            &db,
            &custom_profile_id,
            &custom_server_id,
            "custom_server",
            "prompt_custom",
        )
        .await;

        insert_client_config(
            &db,
            "client-c",
            CapabilitySource::Custom,
            Vec::new(),
            Some(custom_profile_id.clone()),
        )
        .await;

        let snapshot = service
            .resolve_snapshot("client-c", None)
            .await
            .expect("resolve snapshot");

        assert_eq!(snapshot.profile_ids, vec![custom_profile_id]);
        assert_eq!(snapshot.server_ids, vec![custom_server_id]);
        assert!(snapshot.allowed_prompts.contains(&custom_prompt));
        assert_eq!(snapshot.allowed_prompts.len(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn direct_authorization_uses_same_snapshot_rules_as_list_filtering() {
        let (_temp_dir, db, service) = create_visibility_service().await;

        let profile_id = insert_profile(&db, "selected", ProfileType::Shared, false).await;
        let allowed_server_id = insert_server(&db, "alpha_server").await;
        let denied_server_id = insert_server(&db, "beta_server").await;
        crate::core::capability::resolver::upsert(&allowed_server_id, "alpha_server").await;
        crate::core::capability::resolver::upsert(&denied_server_id, "beta_server").await;

        profile::add_server_to_profile(&db.pool, &profile_id, &allowed_server_id, true)
            .await
            .expect("add allowed server");

        let allowed_tool = seed_tool(&db, &profile_id, &allowed_server_id, "alpha_server", "tool_alpha").await;
        let allowed_prompt = seed_prompt(&db, &profile_id, &allowed_server_id, "alpha_server", "prompt_alpha").await;
        let _allowed_resource = seed_resource(
            &db,
            &profile_id,
            &allowed_server_id,
            "alpha_server",
            "file://workspace/explicit.txt",
        )
        .await;
        let allowed_template = seed_resource_template(
            &db,
            &profile_id,
            &allowed_server_id,
            "alpha_server",
            "file://workspace/{path}",
        )
        .await;

        let denied_tool = crate::config::server::tools::upsert_server_tool(
            &db.pool,
            &denied_server_id,
            "beta_server",
            "tool_beta",
            None,
        )
        .await
        .expect("upsert denied tool")
        .unique_name;
        let denied_prompt = crate::config::server::capabilities::upsert_shadow_prompt(
            &db.pool,
            &denied_server_id,
            "beta_server",
            "prompt_beta",
            None,
        )
        .await
        .expect("upsert denied prompt");
        let denied_resource = crate::config::server::capabilities::upsert_shadow_resource(
            &db.pool,
            &denied_server_id,
            "beta_server",
            "file://other/file.txt",
            None,
            None,
            None,
        )
        .await
        .expect("upsert denied resource");

        insert_client_config(&db, "client-d", CapabilitySource::Profiles, vec![profile_id], None).await;

        let snapshot = service
            .resolve_snapshot("client-d", None)
            .await
            .expect("resolve snapshot");

        assert!(
            service
                .assert_tool_allowed_with_snapshot(&snapshot, &allowed_tool)
                .await
                .is_ok()
        );
        assert!(
            service
                .assert_tool_allowed_with_snapshot(&snapshot, &denied_tool)
                .await
                .is_err()
        );
        assert!(
            service
                .assert_prompt_allowed_with_snapshot(&snapshot, &allowed_prompt)
                .await
                .is_ok()
        );
        assert!(
            service
                .assert_prompt_allowed_with_snapshot(&snapshot, &denied_prompt)
                .await
                .is_err()
        );

        let dynamic_allowed = encode_resource_template("alpha_server", "file://workspace/{path}")
            .expect("encode allowed template")
            .replace("{path}", "main.rs");
        assert!(
            service
                .assert_resource_allowed_with_snapshot(&snapshot, &dynamic_allowed)
                .await
                .is_ok()
        );
        assert!(snapshot.allowed_resource_templates.contains(&allowed_template));

        let unrelated_dynamic = encode_resource_template("alpha_server", "file://other/{path}")
            .expect("encode unrelated template")
            .replace("{path}", "main.rs");
        assert!(
            service
                .assert_resource_allowed_with_snapshot(&snapshot, &unrelated_dynamic)
                .await
                .is_err()
        );
        let unknown_namespace =
            encode_resource_uri("unknown_server", "file:///guide.md").expect("encode unknown namespace resource");
        assert!(
            service
                .assert_resource_allowed_with_snapshot(&snapshot, &unknown_namespace)
                .await
                .is_err()
        );
        assert!(
            service
                .assert_resource_allowed_with_snapshot(&snapshot, "file:///guide.md")
                .await
                .is_err()
        );

        let explicit_allowed =
            encode_resource_uri("alpha_server", "file://workspace/explicit.txt").expect("encode explicit resource");
        assert!(
            service
                .assert_resource_allowed_with_snapshot(&snapshot, &explicit_allowed)
                .await
                .is_ok()
        );
        let issued_resource = crate::core::capability::resource_registry::issue_resource_route(
            &db.pool,
            &allowed_server_id,
            "alpha_server",
            "file://other/generated.txt",
        )
        .await
        .expect("issue unmatched resource route");
        let issued_route =
            crate::core::capability::resource_registry::resolve_resource_route(&db.pool, &issued_resource)
                .await
                .expect("resolve issued resource route");
        assert_eq!(
            issued_route.source,
            crate::core::capability::resource_registry::ResourceRouteSource::Issued
        );
        assert!(
            service
                .assert_resource_allowed_with_snapshot(&snapshot, &issued_resource)
                .await
                .is_err()
        );
        assert!(
            service
                .assert_resource_allowed_with_snapshot(&snapshot, &denied_resource)
                .await
                .is_err()
        );
        crate::core::capability::resolver::remove_by_id(&allowed_server_id).await;
        crate::core::capability::resolver::remove_by_id(&denied_server_id).await;
    }

    #[tokio::test]
    #[serial]
    async fn unify_snapshot_uses_globally_enabled_servers_without_profile_semantics() {
        let (_temp_dir, db, service) = create_visibility_service().await;

        let active_profile_id = insert_profile(&db, "active", ProfileType::Shared, true).await;
        let selected_profile_id = insert_profile(&db, "selected", ProfileType::Shared, false).await;
        let disabled_server_id = insert_server(&db, "disabled_server").await;
        let enabled_server_id = insert_server(&db, "enabled_server").await;

        profile::add_server_to_profile(&db.pool, &active_profile_id, &disabled_server_id, true)
            .await
            .expect("add active server");
        profile::add_server_to_profile(&db.pool, &selected_profile_id, &enabled_server_id, true)
            .await
            .expect("add selected server");

        let disabled_tool = seed_tool(
            &db,
            &active_profile_id,
            &disabled_server_id,
            "disabled_server",
            "tool_disabled",
        )
        .await;
        let enabled_tool = seed_tool(
            &db,
            &selected_profile_id,
            &enabled_server_id,
            "enabled_server",
            "tool_enabled",
        )
        .await;

        sqlx::query(
            r#"
            UPDATE server_config
            SET enabled = 0
            WHERE id = ?
            "#,
        )
        .bind(&disabled_server_id)
        .execute(&db.pool)
        .await
        .expect("disable active-profile server globally");

        insert_client_config(
            &db,
            "client-unify",
            CapabilitySource::Profiles,
            vec![selected_profile_id.clone()],
            None,
        )
        .await;

        let client = ClientContext {
            client_id: "client-unify".to_string(),
            session_id: Some("unify-session".to_string()),
            profile_id: None,
            config_mode: Some("unify".to_string()),
            unify_workspace: None,
            surface_fingerprint: None,
            transport: crate::core::proxy::server::ClientTransport::Other,
            source: crate::core::proxy::server::ClientIdentitySource::SessionBinding,
            observed_client_info: None,
        };

        let snapshot = service
            .resolve_snapshot_for_client(&client)
            .await
            .expect("resolve unify snapshot");

        assert!(snapshot.profile_ids.is_empty());
        assert_eq!(snapshot.server_ids, vec![enabled_server_id]);
        assert!(snapshot.allowed_tools.contains(&enabled_tool));
        assert!(!snapshot.allowed_tools.contains(&disabled_tool));
    }
}
