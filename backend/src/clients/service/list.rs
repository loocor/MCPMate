use super::core::{ClientConfigService, ClientDescriptor};
use crate::clients::detector::DetectedClient;
use crate::clients::error::{ConfigError, ConfigResult};
use crate::system::paths::get_path_service;
use std::collections::HashMap;
use std::path::PathBuf;

impl ClientConfigService {
    /// List known clients enriched with detection and filesystem information
    pub async fn list_clients(
        &self,
        force_detect: bool,
        persist_detected: bool,
    ) -> ConfigResult<Vec<ClientDescriptor>> {
        if let Err(err) = self.ensure_runtime_template_snapshot().await {
            tracing::warn!(
                error = %err,
                "Admin discovery runtime template refresh failed during client listing; continuing with existing SQLite snapshot"
            );
        }

        if force_detect {
            self.template_source.reload().await?;
        }

        let detected = self.detector.detect_installed_client().await?;
        let states = self.fetch_client_states().await?;
        let templates = self.template_source.list_client().await?;
        let mut template_map = templates
            .into_iter()
            .map(|template| (template.identifier.clone(), template))
            .collect::<HashMap<_, _>>();

        let mut detected_map: HashMap<String, DetectedClient> = HashMap::new();
        for entry in detected {
            detected_map.insert(entry.identifier.clone(), entry);
        }

        let mut results = Vec::with_capacity(states.len() + detected_map.len());
        for (identifier, state) in &states {
            let detection = detected_map.remove(identifier);
            let template = template_map.remove(identifier);

            let resolved_path = self.resolved_config_path(identifier).await?;
            let config_exists = if let Some(path_str) = &resolved_path {
                let path = PathBuf::from(path_str);
                get_path_service()
                    .validate_path_exists(&path)
                    .await
                    .map_err(|err| ConfigError::PathResolutionError(err.to_string()))?
            } else {
                false
            };

            let detected_at = detection.as_ref().map(|entry| entry.detected_at);
            results.push(ClientDescriptor {
                state: state.clone(),
                detection,
                template,
                config_path: resolved_path,
                config_exists,
                detected_at,
                persisted: true,
            });
        }

        for (identifier, detected) in detected_map {
            let display_name = detected.display_name.as_deref().unwrap_or(&identifier);
            let template = template_map.remove(&identifier);

            if !persist_detected {
                if let Some(template) = template {
                    let state = ClientConfigService::preview_state_from_detected_template(
                        &identifier,
                        display_name,
                        detected.config_path.as_deref(),
                        &template,
                    );
                    results.push(ClientDescriptor {
                        state,
                        detection: Some(detected.clone()),
                        template: Some(template),
                        config_path: detected.config_path.clone(),
                        config_exists: detected.config_path.is_some(),
                        detected_at: Some(detected.detected_at),
                        persisted: false,
                    });
                }
                continue;
            }

            match self
                .ensure_passive_observed_row(&identifier, display_name, detected.config_path.as_deref())
                .await
            {
                Ok(state_row) => {
                    tracing::info!(
                        identifier = %identifier,
                        approval_status = %state_row.approval_status(),
                        governance_kind = %state_row.governance_kind().as_str(),
                        "Created passive observed client row"
                    );

                    results.push(ClientDescriptor {
                        state: state_row.clone(),
                        detection: Some(detected.clone()),
                        template,
                        config_path: detected.config_path.clone(),
                        config_exists: false,
                        detected_at: Some(detected.detected_at),
                        persisted: true,
                    });
                }
                Err(err) => {
                    tracing::warn!(
                        identifier = %identifier,
                        error = %err,
                        "Failed to ensure pending row for detected client"
                    );
                }
            }
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::models::OnboardingPolicy;
    use crate::clients::source::{ClientConfigSource, DbTemplateSource, FileTemplateSource, TemplateRoot};
    use crate::config::{
        client::init::{initialize_client_table, initialize_system_settings},
        profile::init::initialize_profile_tables,
        server::init::initialize_server_tables,
    };
    use sqlx::sqlite::SqlitePoolOptions;
    use std::sync::Arc;
    use tempfile::TempDir;

    async fn create_test_service() -> (TempDir, ClientConfigService) {
        let temp_dir = TempDir::new().expect("temp dir");
        let pool = Arc::new(
            SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await
                .expect("sqlite pool"),
        );

        crate::helpers::prepare_config_database(pool.as_ref()).await;
        initialize_server_tables(pool.as_ref())
            .await
            .expect("init server tables");
        initialize_profile_tables(pool.as_ref())
            .await
            .expect("init profile tables");
        initialize_client_table(pool.as_ref()).await.expect("init client table");
        initialize_system_settings(pool.as_ref())
            .await
            .expect("init system settings table");
        mcpmate_capability_store::SqliteCapabilityCatalog::new(pool.as_ref().clone())
            .ensure_schema()
            .await
            .expect("init capability schema");

        let template_root = TemplateRoot::new(temp_dir.path().join("client-templates"));
        let source = Arc::new(
            FileTemplateSource::bootstrap(template_root)
                .await
                .expect("template source"),
        );
        ClientConfigService::seed_runtime_template_snapshots(pool.as_ref(), source.as_ref())
            .await
            .expect("seed runtime templates");
        ClientConfigService::seed_client_runtime_rows(pool.as_ref(), source.as_ref())
            .await
            .expect("seed runtime rows");
        let runtime_source: Arc<dyn ClientConfigSource> =
            Arc::new(DbTemplateSource::new(pool.clone()).expect("runtime source"));
        let service = ClientConfigService::with_source(pool, runtime_source)
            .await
            .expect("client config service");

        (temp_dir, service)
    }

    async fn set_onboarding_policy(
        service: &ClientConfigService,
        policy: OnboardingPolicy,
    ) -> ConfigResult<()> {
        service.set_onboarding_policy(policy).await
    }

    #[tokio::test]
    async fn passive_observed_row_creation_workflow() {
        let (_temp_dir, service) = create_test_service().await;

        set_onboarding_policy(&service, OnboardingPolicy::RequireApproval)
            .await
            .expect("set policy");

        let state = service
            .ensure_passive_observed_row("test.unknown", "Test Unknown", None)
            .await
            .expect("ensure passive observed row");

        assert_eq!(state.identifier, "test.unknown");
        assert_eq!(state.name, "Test Unknown");
        assert_eq!(state.approval_status.as_deref(), Some("pending"));
        assert!(state.template_id.is_none());
        assert!(state.is_pending_approval());
        assert_eq!(state.governance_kind().as_str(), "passive");

        let fetched_state = service
            .fetch_state("test.unknown")
            .await
            .expect("fetch state")
            .expect("state should exist");

        assert_eq!(fetched_state.identifier, "test.unknown");
        assert_eq!(fetched_state.approval_status.as_deref(), Some("pending"));
        assert!(!fetched_state.runtime_observed());
    }

    #[tokio::test]
    async fn ensure_passive_observed_row_is_idempotent() {
        let (_temp_dir, service) = create_test_service().await;

        set_onboarding_policy(&service, OnboardingPolicy::RequireApproval)
            .await
            .expect("set policy");

        let state1 = service
            .ensure_passive_observed_row("test.idempotent", "Test Idempotent", None)
            .await
            .expect("first ensure");

        let state2 = service
            .ensure_passive_observed_row("test.idempotent", "Different Name", None)
            .await
            .expect("second ensure");

        assert_eq!(state1.id, state2.id);
        assert_eq!(state1.identifier, state2.identifier);
        assert_eq!(state2.name, "Different Name");
    }

    #[tokio::test]
    async fn list_clients_includes_active_runtime_only_records() {
        let (_temp_dir, service) = create_test_service().await;

        service
            .set_client_settings("custom.runtime", None, None)
            .await
            .expect("create active runtime-only client");

        let descriptors = service.list_clients(false, true).await.expect("list clients");
        let descriptor = descriptors
            .into_iter()
            .find(|entry| entry.state.identifier() == "custom.runtime")
            .expect("runtime-only descriptor should exist");

        assert!(descriptor.template.is_none());
        assert_eq!(descriptor.state.display_name(), "custom.runtime");
        assert!(descriptor.state.is_approved());
    }
}
