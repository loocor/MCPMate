use super::core::{ClientConfigService, ClientStateRow, PersistedTemplateConfig};
use crate::clients::error::{ConfigError, ConfigResult};
use crate::clients::models::{
    AttachmentState, BackupPolicySetting, ClientConnectionMode, ClientGovernanceKind, ClientRegistrationOrigin,
    FirstContactBehavior,
};
use crate::core::capability::materializer::{
    MaterializationCoordinator, MaterializationTrigger, revoke_managed_surface_in_transaction,
};
use mcpmate_capability_store::CatalogError;
use std::collections::HashMap;

fn approval_status_for_first_contact_behavior(behavior: FirstContactBehavior) -> &'static str {
    match behavior {
        FirstContactBehavior::Deny => "suspended",
        FirstContactBehavior::Review => "pending",
        FirstContactBehavior::Allow => "approved",
    }
}

impl ClientConfigService {
    pub(super) async fn fetch_client_states(&self) -> ConfigResult<HashMap<String, ClientStateRow>> {
        let rows = sqlx::query_as::<_, ClientStateRow>(
            "SELECT id, identifier, name, display_name, config_path, config_mode, transport, client_version, backup_policy, backup_limit, capability_source, governance_kind, connection_mode, registration_origin, runtime_observed, template_identifier, selected_profile_ids, custom_profile_id, approval_status, attachment_state, template_id, template_version, approval_metadata, config_format, protocol_revision, container_type, container_keys, storage_kind, storage_adapter, storage_path_strategy, merge_strategy, (SELECT merge_strategy FROM client_writeback_policy WHERE client_identifier = client.identifier) AS merge_strategy_override, keep_original_config, managed_source, transports, config_file_parse FROM client",
        )
        .fetch_all(&*self.db_pool)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

        Ok(rows.into_iter().map(|row| (row.identifier.clone(), row)).collect())
    }

    pub(super) async fn ensure_state_row(
        &self,
        identifier: &str,
    ) -> ConfigResult<ClientStateRow> {
        let name = self.resolve_client_name(identifier).await?;
        self.ensure_state_row_with_name(identifier, &name).await
    }

    pub(super) async fn ensure_state_row_with_name(
        &self,
        identifier: &str,
        name: &str,
    ) -> ConfigResult<ClientStateRow> {
        if let Some(existing) = self.fetch_state(identifier).await? {
            return self.refresh_existing_state_name(identifier, name, existing).await;
        }

        let first_contact_behavior = self.get_first_contact_behavior().await?;
        let approval_status = approval_status_for_first_contact_behavior(first_contact_behavior);

        self.create_state_row(
            identifier,
            name,
            ClientGovernanceKind::Passive,
            approval_status,
            None,
            ClientRegistrationOrigin::Manual,
            false,
        )
        .await
    }

    pub async fn fetch_state(
        &self,
        identifier: &str,
    ) -> ConfigResult<Option<ClientStateRow>> {
        sqlx::query_as::<_, ClientStateRow>(
            "SELECT id, identifier, name, display_name, config_path, config_mode, transport, client_version, backup_policy, backup_limit, capability_source, governance_kind, connection_mode, registration_origin, runtime_observed, template_identifier, selected_profile_ids, custom_profile_id, approval_status, attachment_state, template_id, template_version, approval_metadata, config_format, protocol_revision, container_type, container_keys, storage_kind, storage_adapter, storage_path_strategy, merge_strategy, (SELECT merge_strategy FROM client_writeback_policy WHERE client_identifier = client.identifier) AS merge_strategy_override, keep_original_config, managed_source, transports, config_file_parse FROM client WHERE identifier = ?",
        )
        .bind(identifier)
        .fetch_optional(&*self.db_pool)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))
    }

    pub async fn delete_client_record(
        &self,
        identifier: &str,
    ) -> ConfigResult<bool> {
        if self.fetch_state(identifier).await?.is_none() {
            return Ok(false);
        }

        if let Err(err) = self.delete_all_client_backups(identifier).await {
            tracing::warn!(
                client = %identifier,
                error = %err,
                "Failed to delete all client backups before record removal; continuing"
            );
        }

        let mut transaction = self
            .db_pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;
        let custom_profile = sqlx::query_as::<_, (String, i64)>(
            r#"
            SELECT profile.id, profile.authoring_generation
            FROM client
            JOIN profile ON profile.id = client.custom_profile_id
            WHERE client.identifier = ?
            "#,
        )
        .bind(identifier)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;
        revoke_managed_surface_in_transaction(self.db_pool.as_ref(), &mut transaction, identifier, "client-delete")
            .await
            .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

        sqlx::query("DELETE FROM client_template_runtime WHERE identifier = ?")
            .bind(identifier)
            .execute(&mut *transaction)
            .await
            .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

        sqlx::query("DELETE FROM client_writeback_policy WHERE client_identifier = ?")
            .bind(identifier)
            .execute(&mut *transaction)
            .await
            .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

        let result = sqlx::query("DELETE FROM client WHERE identifier = ?")
            .bind(identifier)
            .execute(&mut *transaction)
            .await
            .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

        if result.rows_affected() == 0 {
            return Ok(false);
        }

        if let Some((custom_profile_id, authoring_generation)) = custom_profile {
            let deleted = sqlx::query("DELETE FROM profile WHERE id = ? AND authoring_generation = ?")
                .bind(&custom_profile_id)
                .bind(authoring_generation)
                .execute(&mut *transaction)
                .await
                .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;
            if deleted.rows_affected() != 1 {
                return Err(ConfigError::DataAccessError(format!(
                    "Profile '{custom_profile_id}' changed before client cleanup"
                )));
            }
        }

        transaction
            .commit()
            .await
            .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;
        Ok(true)
    }

    pub(crate) async fn resolve_client_name(
        &self,
        identifier: &str,
    ) -> ConfigResult<String> {
        if let Some(state) = self.fetch_state(identifier).await? {
            if !state.display_name().trim().is_empty() {
                return Ok(state.display_name().to_string());
            }
            if !state.name.trim().is_empty() {
                return Ok(state.name);
            }
        }
        Ok(identifier.to_string())
    }

    pub async fn is_client_approved(
        &self,
        identifier: &str,
    ) -> ConfigResult<bool> {
        let status = sqlx::query_scalar::<_, String>("SELECT approval_status FROM client WHERE identifier = ?")
            .bind(identifier)
            .fetch_optional(&*self.db_pool)
            .await
            .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

        match status {
            Some(value) => Ok(value == "approved"),
            None => {
                let state = self.ensure_state_row(identifier).await?;
                Ok(state.is_approved())
            }
        }
    }

    pub async fn get_backup_policy(
        &self,
        identifier: &str,
    ) -> ConfigResult<BackupPolicySetting> {
        let state = self.ensure_state_row(identifier).await?;
        Ok(state.to_setting())
    }

    pub async fn set_backup_policy(
        &self,
        identifier: &str,
        policy: BackupPolicySetting,
    ) -> ConfigResult<BackupPolicySetting> {
        let (policy_label, limit) = policy.as_db_pair();
        let name = self.resolve_client_name(identifier).await?;
        self.ensure_active_state_row_with_name(identifier, &name, Some("approved"))
            .await?;

        sqlx::query(
            r#"
            UPDATE client
            SET name = ?,
                backup_policy = ?,
                backup_limit = ?,
                governance_kind = 'active',
                updated_at = CURRENT_TIMESTAMP
            WHERE identifier = ?
            "#,
        )
        .bind(name)
        .bind(policy_label)
        .bind(limit.map(|v| v as i64))
        .bind(identifier)
        .execute(&*self.db_pool)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

        Ok(policy)
    }

    pub async fn get_onboarding_policy(&self) -> ConfigResult<crate::clients::models::OnboardingPolicy> {
        let behavior = crate::system::settings::get_first_contact_behavior(&self.db_pool).await?;
        Ok(crate::system::settings::onboarding_policy_from_behavior(behavior))
    }

    pub async fn get_first_contact_behavior(&self) -> ConfigResult<FirstContactBehavior> {
        crate::system::settings::get_first_contact_behavior(&self.db_pool).await
    }

    pub async fn set_first_contact_behavior(
        &self,
        behavior: FirstContactBehavior,
    ) -> ConfigResult<()> {
        crate::system::settings::set_first_contact_behavior(&self.db_pool, behavior).await
    }

    pub async fn get_inspector_timeout_ms(&self) -> ConfigResult<u64> {
        crate::system::settings::get_inspector_timeout_ms(&self.db_pool).await
    }

    pub async fn set_inspector_timeout_ms(
        &self,
        timeout_ms: u64,
    ) -> ConfigResult<()> {
        crate::system::settings::set_inspector_timeout_ms(&self.db_pool, timeout_ms).await
    }

    pub async fn set_onboarding_policy(
        &self,
        policy: crate::clients::models::OnboardingPolicy,
    ) -> ConfigResult<()> {
        let behavior = crate::system::settings::behavior_from_onboarding_policy(policy);
        self.set_first_contact_behavior(behavior).await
    }

    pub async fn approve_client(
        &self,
        identifier: &str,
    ) -> ConfigResult<(
        String,
        Option<crate::core::capability::materializer::MaterializationCommit>,
    )> {
        let name = self.resolve_client_name(identifier).await?;
        if let Some(existing) = self.fetch_state(identifier).await? {
            self.promote_existing_state(identifier, &name, None, existing).await?;
        } else {
            self.create_state_row(
                identifier,
                &name,
                ClientGovernanceKind::Active,
                "pending",
                None,
                ClientRegistrationOrigin::Manual,
                false,
            )
            .await?;
        }

        self.ensure_local_config_target_metadata(identifier).await?;
        let default_config_mode = crate::config::client::init::resolve_default_client_config_mode(&self.db_pool)
            .await
            .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
        let mut transaction = self
            .db_pool
            .begin()
            .await
            .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
        sqlx::query(
            r#"
            UPDATE client
            SET approval_status = 'approved', governance_kind = 'active', updated_at = CURRENT_TIMESTAMP
            WHERE identifier = ?
            "#,
        )
        .bind(identifier)
        .execute(&mut *transaction)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;
        let materialization = self
            .materialize_managed_surface_in_transaction(
                &mut transaction,
                identifier,
                &default_config_mode,
                &MaterializationTrigger::for_consumer(
                    "consumer_approval",
                    format!("approve:{identifier}"),
                    "client_management",
                ),
            )
            .await
            .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
        transaction
            .commit()
            .await
            .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;

        Ok(("approved".to_string(), materialization))
    }

    /// Load client state and repair local-config metadata drift when needed.
    pub async fn fetch_state_repairing_local_target(
        &self,
        identifier: &str,
    ) -> ConfigResult<Option<ClientStateRow>> {
        let Some(state) = self.fetch_state(identifier).await? else {
            return Ok(None);
        };
        if self
            .ensure_local_config_target_metadata_for_state(identifier, &state)
            .await?
        {
            self.fetch_state(identifier).await
        } else {
            Ok(Some(state))
        }
    }

    /// Align `connection_mode` with a persisted config path so attach/detach lifecycle metadata matches file clients.
    pub async fn ensure_local_config_target_metadata(
        &self,
        identifier: &str,
    ) -> ConfigResult<bool> {
        let Some(state) = self.fetch_state(identifier).await? else {
            return Ok(false);
        };
        self.ensure_local_config_target_metadata_for_state(identifier, &state)
            .await
    }

    async fn ensure_local_config_target_metadata_for_state(
        &self,
        identifier: &str,
        state: &ClientStateRow,
    ) -> ConfigResult<bool> {
        let has_local_config_target = state.has_local_config_target();
        let attachment_needs_repair =
            has_local_config_target && matches!(state.attachment_state(), AttachmentState::NotApplicable);

        if has_local_config_target && !attachment_needs_repair {
            return Ok(false);
        }
        let Some(config_path) = state.config_path() else {
            return Ok(false);
        };
        match state.effective_config_file_parse() {
            Ok(Some(_)) => {}
            Ok(None) => return Ok(false),
            Err(err) => {
                tracing::warn!(
                    client = %identifier,
                    error = %err,
                    "Skipping local target repair due to invalid parse metadata"
                );
                return Ok(false);
            }
        }

        let mut repaired = false;
        if !has_local_config_target {
            self.update_runtime_target(
                identifier,
                Some(config_path),
                Some(ClientConnectionMode::LocalConfigDetected.as_str()),
                false,
            )
            .await?;
            repaired = true;
        }

        let result = sqlx::query(
            r#"
            UPDATE client
            SET attachment_state = 'detached'
            WHERE identifier = ?
              AND (attachment_state IS NULL OR attachment_state = 'not_applicable')
            "#,
        )
        .bind(identifier)
        .execute(&*self.db_pool)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;
        repaired = repaired || result.rows_affected() > 0;

        if repaired {
            tracing::info!(client = %identifier, "Repaired local config target metadata");
        }
        Ok(repaired)
    }

    pub async fn suspend_client(
        &self,
        identifier: &str,
    ) -> ConfigResult<String> {
        let name = self.resolve_client_name(identifier).await?;
        let row = self
            .ensure_active_state_row_with_name(identifier, &name, Some("suspended"))
            .await?;

        if row.approval_status() == "suspended" {
            return Ok(row.approval_status().to_string());
        }

        sqlx::query(
            r#"
            UPDATE client
            SET approval_status = 'suspended', governance_kind = 'active', updated_at = CURRENT_TIMESTAMP
            WHERE identifier = ?
            "#,
        )
        .bind(identifier)
        .execute(&*self.db_pool)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

        Ok("suspended".to_string())
    }

    pub async fn mark_client_attached(
        &self,
        identifier: &str,
    ) -> ConfigResult<()> {
        sqlx::query(
            r#"
            UPDATE client
            SET attachment_state = CASE
                    WHEN connection_mode = 'local_config_detected'
                        AND config_path IS NOT NULL
                        AND TRIM(config_path) <> ''
                    THEN 'attached'
                    ELSE 'not_applicable'
                END,
                updated_at = CURRENT_TIMESTAMP
            WHERE identifier = ?
            "#,
        )
        .bind(identifier)
        .execute(&*self.db_pool)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

        Ok(())
    }

    /// Request bootstrap of the managed Consumer Surface through the shared materialization pipeline.
    ///
    /// This is the client-management notification entrypoint: it delegates to the same
    /// bootstrap path used at startup and does not materialize inline from ad-hoc logic.
    pub async fn notify_managed_consumer_surface_bootstrap(
        &self,
        identifier: &str,
        trigger_kind: &str,
        trigger_id: impl Into<String>,
    ) -> ConfigResult<Option<crate::core::capability::materializer::MaterializationCommit>> {
        let Some(state) = self.fetch_state(identifier).await? else {
            return Ok(None);
        };
        let default_config_mode = crate::config::client::init::resolve_default_client_config_mode(&self.db_pool)
            .await
            .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
        let effective_config_mode = crate::config::client::init::effective_client_config_mode(
            state.config_mode.as_deref(),
            &default_config_mode,
        );
        if !state.is_approved() || !crate::config::client::init::is_managed_client_config_mode(effective_config_mode) {
            return Ok(None);
        }

        let materialization = crate::core::capability::materializer::bootstrap_managed_consumer_surface_if_missing(
            self.db_pool.as_ref(),
            identifier,
            trigger_kind,
            trigger_id,
            "client_management",
        )
        .await
        .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
        if let Some(materialization) = &materialization {
            tracing::info!(
                client = %identifier,
                trigger = %trigger_kind,
                generation = ?materialization.binding.as_ref().map(|binding| binding.generation),
                "Published managed Consumer Surface through bootstrap notification"
            );
        }
        Ok(materialization)
    }

    /// Publish the initial managed Surface for a client that was created after startup.
    ///
    /// The managed Surface is materialized when MCPMate configuration is saved, never lazily
    /// while serving a downstream MCP request. Existing bindings are deliberately left intact.
    pub async fn ensure_initial_managed_surface(
        &self,
        identifier: &str,
    ) -> ConfigResult<Option<crate::core::capability::materializer::MaterializationCommit>> {
        self.notify_managed_consumer_surface_bootstrap(
            identifier,
            "consumer_attachment",
            format!("attach:{identifier}"),
        )
        .await
    }

    pub(super) async fn materialize_managed_surface_in_transaction(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        identifier: &str,
        default_config_mode: &str,
        trigger: &MaterializationTrigger,
    ) -> Result<Option<crate::core::capability::materializer::MaterializationCommit>, CatalogError> {
        let state: Option<(Option<String>, String)> =
            sqlx::query_as("SELECT config_mode, approval_status FROM client WHERE identifier = ?")
                .bind(identifier)
                .fetch_optional(&mut **transaction)
                .await?;
        let Some((config_mode, approval_status)) = state else {
            return Ok(None);
        };
        let effective_config_mode =
            crate::config::client::init::effective_client_config_mode(config_mode.as_deref(), default_config_mode);
        if approval_status != "approved"
            || !crate::config::client::init::is_managed_client_config_mode(effective_config_mode)
        {
            return Ok(None);
        }

        MaterializationCoordinator::new(self.db_pool.as_ref().clone())
            .compile_consumer_in_transaction_with_default(transaction, identifier, default_config_mode, trigger)
            .await
            .map(Some)
    }

    pub async fn mark_client_detached(
        &self,
        identifier: &str,
    ) -> ConfigResult<()> {
        sqlx::query(
            r#"
            UPDATE client
            SET attachment_state = CASE
                    WHEN connection_mode = 'local_config_detected'
                        AND config_path IS NOT NULL
                        AND TRIM(config_path) <> ''
                    THEN 'detached'
                    ELSE 'not_applicable'
                END,
                updated_at = CURRENT_TIMESTAMP
            WHERE identifier = ?
            "#,
        )
        .bind(identifier)
        .execute(&*self.db_pool)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

        Ok(())
    }

    /// Used by detection/list when registering a client candidate discovered from local configuration metadata.
    pub async fn ensure_passive_observed_row(
        &self,
        identifier: &str,
        name: &str,
        config_path: Option<&str>,
    ) -> ConfigResult<ClientStateRow> {
        if let Some(existing) = self.fetch_state_repairing_local_target(identifier).await? {
            return self.refresh_existing_state_name(identifier, name, existing).await;
        }
        let first_contact_behavior = self.get_first_contact_behavior().await?;
        let approval_status = approval_status_for_first_contact_behavior(first_contact_behavior);

        self.create_state_row(
            identifier,
            name,
            ClientGovernanceKind::Passive,
            approval_status,
            config_path,
            if config_path.is_some() {
                ClientRegistrationOrigin::ConfigDetection
            } else {
                ClientRegistrationOrigin::Manual
            },
            false,
        )
        .await
    }

    /// Used by the MCP proxy when an unknown client reaches MCPMate through the runtime boundary.
    pub async fn ensure_passive_runtime_observed_row(
        &self,
        identifier: &str,
        name: &str,
    ) -> ConfigResult<ClientStateRow> {
        if let Some(existing) = self.fetch_state(identifier).await? {
            self.refresh_existing_state_name(identifier, name, existing).await?;
            self.mark_runtime_observed(identifier).await?;
            return self.fetch_state(identifier).await?.ok_or_else(|| {
                ConfigError::DataAccessError(format!("Failed to reload runtime-observed client {}", identifier))
            });
        }
        let first_contact_behavior = self.get_first_contact_behavior().await?;
        let approval_status = approval_status_for_first_contact_behavior(first_contact_behavior);

        self.create_state_row(
            identifier,
            name,
            ClientGovernanceKind::Passive,
            approval_status,
            None,
            ClientRegistrationOrigin::RuntimeInitialize,
            true,
        )
        .await
    }

    pub async fn apply_first_contact_behavior_to_passive_state(
        &self,
        identifier: &str,
        name: &str,
    ) -> ConfigResult<ClientStateRow> {
        let Some(existing) = self.fetch_state(identifier).await? else {
            return self.ensure_passive_runtime_observed_row(identifier, name).await;
        };

        if existing.governance_kind() != ClientGovernanceKind::Passive {
            return self.refresh_existing_state_name(identifier, name, existing).await;
        }

        let first_contact_behavior = self.get_first_contact_behavior().await?;
        let approval_status = approval_status_for_first_contact_behavior(first_contact_behavior);
        self.refresh_existing_state_name(identifier, name, existing).await?;
        self.mark_runtime_observed(identifier).await?;

        sqlx::query(
            r#"
            UPDATE client
            SET approval_status = ?, updated_at = CURRENT_TIMESTAMP
            WHERE identifier = ? AND governance_kind = 'passive'
            "#,
        )
        .bind(approval_status)
        .bind(identifier)
        .execute(&*self.db_pool)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

        let state = self.fetch_state(identifier).await?.ok_or_else(|| {
            ConfigError::DataAccessError(format!(
                "Failed to reload first-contact state for client {}",
                identifier
            ))
        })?;
        if state.is_approved() {
            self.notify_managed_consumer_surface_bootstrap(
                identifier,
                "consumer_first_contact",
                format!("first-contact:{identifier}"),
            )
            .await?;
        }
        Ok(state)
    }

    pub(crate) async fn ensure_active_state_row_with_name(
        &self,
        identifier: &str,
        name: &str,
        approval_status: Option<&str>,
    ) -> ConfigResult<ClientStateRow> {
        self.ensure_active_state_row_with_creation(identifier, name, approval_status)
            .await
            .map(|(state, _created)| state)
    }

    pub(crate) async fn ensure_active_state_row_with_creation(
        &self,
        identifier: &str,
        name: &str,
        approval_status: Option<&str>,
    ) -> ConfigResult<(ClientStateRow, bool)> {
        if let Some(existing) = self.fetch_state(identifier).await? {
            self.promote_existing_state(identifier, name, approval_status, existing)
                .await
                .map(|state| (state, false))
        } else {
            self.create_state_row_with_creation(
                identifier,
                name,
                ClientGovernanceKind::Active,
                approval_status.unwrap_or("approved"),
                None,
                ClientRegistrationOrigin::Manual,
                false,
            )
            .await
        }
    }

    pub(crate) async fn mark_runtime_observed(
        &self,
        identifier: &str,
    ) -> ConfigResult<()> {
        sqlx::query(
            r#"
            UPDATE client
            SET runtime_observed = 1,
                registration_origin = CASE
                    WHEN registration_origin IS NULL
                        OR registration_origin = ''
                        OR (registration_origin = 'manual' AND governance_kind = 'passive')
                    THEN 'runtime_initialize'
                    ELSE registration_origin
                END,
                updated_at = CURRENT_TIMESTAMP
            WHERE identifier = ?
            "#,
        )
        .bind(identifier)
        .execute(&*self.db_pool)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

        Ok(())
    }

    async fn refresh_existing_state_name(
        &self,
        identifier: &str,
        name: &str,
        existing: ClientStateRow,
    ) -> ConfigResult<ClientStateRow> {
        if existing.name == name {
            return Ok(existing);
        }

        sqlx::query(
            r#"
            UPDATE client
            SET name = ?, updated_at = CURRENT_TIMESTAMP
            WHERE identifier = ?
            "#,
        )
        .bind(name)
        .bind(identifier)
        .execute(&*self.db_pool)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

        self.fetch_state(identifier).await?.ok_or_else(|| {
            ConfigError::DataAccessError(format!("Failed to update management state for client {}", identifier))
        })
    }

    async fn promote_existing_state(
        &self,
        identifier: &str,
        name: &str,
        approval_status: Option<&str>,
        existing: ClientStateRow,
    ) -> ConfigResult<ClientStateRow> {
        let approval_status = approval_status.unwrap_or(existing.approval_status());

        sqlx::query(
            r#"
            UPDATE client
            SET name = ?,
                governance_kind = 'active',
                approval_status = ?,
                updated_at = CURRENT_TIMESTAMP
            WHERE identifier = ?
            "#,
        )
        .bind(name)
        .bind(approval_status)
        .bind(identifier)
        .execute(&*self.db_pool)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

        self.fetch_state(identifier).await?.ok_or_else(|| {
            ConfigError::DataAccessError(format!("Failed to promote management state for client {}", identifier))
        })
    }

    async fn create_state_row(
        &self,
        identifier: &str,
        name: &str,
        governance_kind: ClientGovernanceKind,
        approval_status: &str,
        observed_config_path: Option<&str>,
        registration_origin: ClientRegistrationOrigin,
        runtime_observed: bool,
    ) -> ConfigResult<ClientStateRow> {
        self.create_state_row_with_creation(
            identifier,
            name,
            governance_kind,
            approval_status,
            observed_config_path,
            registration_origin,
            runtime_observed,
        )
        .await
        .map(|(state, _created)| state)
    }

    async fn create_state_row_with_creation(
        &self,
        identifier: &str,
        name: &str,
        governance_kind: ClientGovernanceKind,
        approval_status: &str,
        observed_config_path: Option<&str>,
        registration_origin: ClientRegistrationOrigin,
        runtime_observed: bool,
    ) -> ConfigResult<(ClientStateRow, bool)> {
        let platform = crate::system::paths::PathService::get_current_platform();
        let template = self.template_source.get_template(identifier, platform).await?;
        let display_name = template
            .as_ref()
            .and_then(|entry| entry.display_name.clone())
            .unwrap_or_else(|| name.to_string());
        let config_path = observed_config_path.map(str::to_string).or_else(|| {
            template
                .as_ref()
                .and_then(Self::extract_runtime_config_path_from_template)
        });
        let connection_mode = if config_path.is_some() {
            ClientConnectionMode::LocalConfigDetected.as_str()
        } else {
            ClientConnectionMode::Manual.as_str()
        };
        let attachment_state = if config_path.is_some() {
            AttachmentState::Detached.as_str()
        } else {
            AttachmentState::NotApplicable.as_str()
        };
        let template_identifier = template.as_ref().map(|entry| entry.identifier.clone());
        let generated_id = crate::generate_id!("clnt");
        let persisted_config = template
            .as_ref()
            .map(PersistedTemplateConfig::from_template)
            .unwrap_or_default();

        let insert_result = sqlx::query(
            r#"
            INSERT INTO client (
                id, name, display_name, identifier, config_path, backup_policy, backup_limit,
                approval_status, governance_kind, connection_mode, registration_origin, runtime_observed, template_identifier,
                config_format, protocol_revision, container_type, container_keys,
                storage_kind, storage_adapter, storage_path_strategy,
                merge_strategy, keep_original_config, managed_source, transports, config_file_parse, attachment_state
            )
            VALUES (?, ?, ?, ?, ?, 'keep_n', 5, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&generated_id)
        .bind(name)
        .bind(display_name)
        .bind(identifier)
        .bind(config_path)
        .bind(approval_status)
        .bind(governance_kind.as_str())
        .bind(connection_mode)
        .bind(registration_origin.as_str())
        .bind(if runtime_observed { 1_i64 } else { 0_i64 })
        .bind(template_identifier)
        .bind(persisted_config.config_format)
        .bind(persisted_config.protocol_revision)
        .bind(persisted_config.container_type)
        .bind(persisted_config.container_keys)
        .bind(persisted_config.storage_kind)
        .bind(persisted_config.storage_adapter)
        .bind(persisted_config.storage_path_strategy)
        .bind(persisted_config.merge_strategy)
        .bind(persisted_config.keep_original_config)
        .bind(persisted_config.managed_source)
        .bind(persisted_config.transports)
        .bind(persisted_config.config_file_parse)
        .bind(attachment_state)
        .execute(&*self.db_pool)
        .await;

        let created = match insert_result {
            Ok(result) => result.rows_affected() == 1,
            Err(err) => {
                if let sqlx::Error::Database(db_err) = &err {
                    if db_err.code().map(|code| code == "2067").unwrap_or(false) {
                        tracing::warn!(client = %identifier, "Concurrent client state insert detected; reusing existing row");
                        false
                    } else {
                        return Err(ConfigError::DataAccessError(err.to_string()));
                    }
                } else {
                    return Err(ConfigError::DataAccessError(err.to_string()));
                }
            }
        };

        let state = self.fetch_state(identifier).await?.ok_or_else(|| {
            ConfigError::DataAccessError(format!("Failed to create management state for client {}", identifier))
        })?;
        Ok((state, created))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::models::OnboardingPolicy;
    use crate::clients::service::settings::ActiveClientSettingsUpdate;
    use crate::clients::source::{ClientConfigSource, DbTemplateSource, FileTemplateSource, TemplateRoot};
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
        crate::config::initialization::run_initialization(pool.as_ref())
            .await
            .expect("initialize database");

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
    async fn auto_manage_policy_creates_approved_client() {
        let (_temp_dir, service) = create_test_service().await;

        set_onboarding_policy(&service, OnboardingPolicy::AutoManage)
            .await
            .expect("set policy");

        let state = service
            .ensure_state_row_with_name("test.client", "Test Client")
            .await
            .expect("ensure state");

        assert_eq!(state.identifier, "test.client");
        assert_eq!(state.name, "Test Client");
        assert_eq!(state.approval_status.as_deref(), Some("approved"));
        assert_eq!(state.governance_kind().as_str(), "passive");
    }

    #[tokio::test]
    async fn require_approval_policy_creates_pending_client() {
        let (_temp_dir, service) = create_test_service().await;

        set_onboarding_policy(&service, OnboardingPolicy::RequireApproval)
            .await
            .expect("set policy");

        let state = service
            .ensure_state_row_with_name("test.client", "Test Client")
            .await
            .expect("ensure state");

        assert_eq!(state.identifier, "test.client");
        assert_eq!(state.name, "Test Client");
        assert_eq!(state.approval_status.as_deref(), Some("pending"));
        assert_eq!(state.governance_kind().as_str(), "passive");
    }

    #[tokio::test]
    async fn manual_policy_creates_suspended_client() {
        let (_temp_dir, service) = create_test_service().await;

        set_onboarding_policy(&service, OnboardingPolicy::Manual)
            .await
            .expect("set policy");

        let state = service
            .ensure_state_row_with_name("test.client", "Test Client")
            .await
            .expect("ensure state");

        assert_eq!(state.identifier, "test.client");
        assert_eq!(state.name, "Test Client");
        assert_eq!(state.approval_status.as_deref(), Some("suspended"));
        assert_eq!(state.governance_kind().as_str(), "passive");
    }

    #[tokio::test]
    async fn default_policy_is_require_approval() {
        let (_temp_dir, service) = create_test_service().await;

        let policy = service.get_onboarding_policy().await.expect("get policy");

        assert_eq!(policy, OnboardingPolicy::RequireApproval);
    }

    #[tokio::test]
    async fn existing_client_is_returned_regardless_of_policy() {
        let (_temp_dir, service) = create_test_service().await;

        set_onboarding_policy(&service, OnboardingPolicy::AutoManage)
            .await
            .expect("set policy");

        let first = service
            .ensure_state_row_with_name("test.client", "Test Client")
            .await
            .expect("first ensure");

        set_onboarding_policy(&service, OnboardingPolicy::Manual)
            .await
            .expect("change policy to manual");

        let second = service
            .ensure_state_row_with_name("test.client", "Test Client")
            .await
            .expect("second ensure should succeed");

        assert_eq!(first.id, second.id);
        assert_eq!(second.approval_status.as_deref(), Some("approved"));
    }

    #[tokio::test]
    async fn passive_first_contact_state_reapplies_current_policy() {
        let (_temp_dir, service) = create_test_service().await;

        set_onboarding_policy(&service, OnboardingPolicy::RequireApproval)
            .await
            .expect("set review policy");
        let pending = service
            .ensure_passive_observed_row("test.client", "Test Client", None)
            .await
            .expect("create pending passive client");
        assert_eq!(pending.approval_status(), "pending");

        set_onboarding_policy(&service, OnboardingPolicy::AutoManage)
            .await
            .expect("set allow policy");
        let approved = service
            .apply_first_contact_behavior_to_passive_state("test.client", "Test Client")
            .await
            .expect("reapply allow policy");
        assert_eq!(approved.approval_status(), "approved");
        assert_eq!(approved.governance_kind(), ClientGovernanceKind::Passive);
        assert!(approved.runtime_observed());
        assert_eq!(
            approved.registration_origin(),
            ClientRegistrationOrigin::RuntimeInitialize
        );

        set_onboarding_policy(&service, OnboardingPolicy::Manual)
            .await
            .expect("set deny policy");
        let suspended = service
            .apply_first_contact_behavior_to_passive_state("test.client", "Test Client")
            .await
            .expect("reapply deny policy");
        assert_eq!(suspended.approval_status(), "suspended");
        assert_eq!(suspended.governance_kind(), ClientGovernanceKind::Passive);
        assert!(suspended.runtime_observed());
    }

    #[tokio::test]
    async fn active_client_creation_materializes_initial_managed_surface() {
        let (_temp_dir, service) = create_test_service().await;

        service
            .set_active_client_settings(
                "cherry-studio",
                ActiveClientSettingsUpdate {
                    display_name: Some("Cherry Studio".to_string()),
                    transport: None,
                    client_version: None,
                    config_file_state: None,
                    config_path: None,
                    description: None,
                    homepage_url: None,
                    docs_url: None,
                    support_url: None,
                    logo_url: None,
                    config_file_parse: None,
                    clear_config_file_parse: false,
                    merge_strategy_override: None,
                    clear_merge_strategy_override: false,
                    transports: None,
                    clear_transports: false,
                },
            )
            .await
            .expect("register client through settings save");

        assert!(
            mcpmate_capability_store::SqliteSurfaceStore::new(service.db_pool.as_ref().clone())
                .load_binding("cherry-studio")
                .await
                .expect("load managed Consumer Surface")
                .is_some(),
            "creating an approved managed Consumer must publish its initial Surface"
        );
    }

    #[tokio::test]
    async fn concurrent_state_inserts_report_exactly_one_creation() {
        let (_temp_dir, service) = create_test_service().await;

        let (first, second) = tokio::join!(
            service.create_state_row_with_creation(
                "concurrent.client",
                "Concurrent Client",
                ClientGovernanceKind::Active,
                "approved",
                None,
                ClientRegistrationOrigin::Manual,
                false,
            ),
            service.create_state_row_with_creation(
                "concurrent.client",
                "Concurrent Client",
                ClientGovernanceKind::Active,
                "approved",
                None,
                ClientRegistrationOrigin::Manual,
                false,
            ),
        );

        let created_count = [first, second]
            .into_iter()
            .map(|result| result.expect("concurrent state insert succeeds"))
            .filter(|(_state, created)| *created)
            .count();

        assert_eq!(created_count, 1);
    }

    #[tokio::test]
    async fn active_client_approval_materializes_inherited_managed_mode() {
        let (_temp_dir, service) = create_test_service().await;

        set_onboarding_policy(&service, OnboardingPolicy::RequireApproval)
            .await
            .expect("set review policy");
        service
            .ensure_passive_observed_row("test.client", "Test Client", None)
            .await
            .expect("create pending passive client");
        let explicit_mode: Option<String> =
            sqlx::query_scalar("SELECT config_mode FROM client WHERE identifier = 'test.client'")
                .fetch_one(service.db_pool.as_ref())
                .await
                .expect("load explicit client mode");
        assert_eq!(explicit_mode, None);
        service.approve_client("test.client").await.expect("approve client");
        let consumer_id: String = sqlx::query_scalar("SELECT identifier FROM client WHERE identifier = 'test.client'")
            .fetch_one(service.db_pool.as_ref())
            .await
            .expect("load Consumer id");
        assert!(
            mcpmate_capability_store::SqliteSurfaceStore::new(service.db_pool.as_ref().clone())
                .load_binding(&consumer_id)
                .await
                .expect("load approval publication")
                .is_some(),
            "approving a managed Consumer must create its initial publication"
        );

        set_onboarding_policy(&service, OnboardingPolicy::Manual)
            .await
            .expect("set deny policy");
        let active = service
            .apply_first_contact_behavior_to_passive_state("test.client", "Test Client")
            .await
            .expect("reapply policy to active client");

        assert_eq!(active.approval_status(), "approved");
        assert_eq!(active.governance_kind(), ClientGovernanceKind::Active);
    }

    #[tokio::test]
    async fn client_name_update_works_with_existing_client() {
        let (_temp_dir, service) = create_test_service().await;

        let first = service
            .ensure_state_row_with_name("test.client", "Original Name")
            .await
            .expect("first ensure");

        let updated = service
            .ensure_state_row_with_name("test.client", "Updated Name")
            .await
            .expect("second ensure with new name");

        assert_eq!(first.id, updated.id);
        assert_eq!(updated.name, "Updated Name");
    }

    #[tokio::test]
    async fn active_updates_promote_existing_passive_client() {
        let (_temp_dir, service) = create_test_service().await;

        let passive = service
            .ensure_state_row_with_name("test.client", "Test Client")
            .await
            .expect("create passive client");
        assert_eq!(passive.governance_kind().as_str(), "passive");

        service
            .set_client_settings("test.client", None, None)
            .await
            .expect("promote via settings update");

        let promoted = service
            .fetch_state("test.client")
            .await
            .expect("fetch state")
            .expect("state exists");
        assert_eq!(promoted.governance_kind().as_str(), "active");
        assert_eq!(promoted.config_mode, None);
    }

    #[tokio::test]
    async fn handshake_observation_fills_existing_passive_client_once() {
        let (_temp_dir, service) = create_test_service().await;

        set_onboarding_policy(&service, OnboardingPolicy::RequireApproval)
            .await
            .expect("set policy");
        service
            .ensure_passive_observed_row("test.observed", "Observed", None)
            .await
            .expect("create passive observed row");

        service
            .persist_handshake_observation(
                "test.observed",
                Some("Observed App"),
                Some("1.2.3"),
                Some("streamable_http"),
                Some("Observed description"),
                Some("https://example.com"),
                Some("https://example.com/logo.png"),
            )
            .await
            .expect("persist first handshake observation");

        service
            .persist_handshake_observation(
                "test.observed",
                Some("Changed App"),
                Some("9.9.9"),
                Some("sse"),
                Some("Changed description"),
                Some("https://changed.example.com"),
                Some("https://changed.example.com/logo.png"),
            )
            .await
            .expect("ignore later handshake observation");

        let state = service
            .fetch_state("test.observed")
            .await
            .expect("fetch state")
            .expect("state exists");
        assert_eq!(state.governance_kind().as_str(), "passive");
        assert_eq!(state.approval_status.as_deref(), Some("pending"));
        assert_eq!(state.display_name.as_deref(), Some("Observed App"));
        assert_eq!(state.client_version.as_deref(), Some("1.2.3"));
        assert_eq!(state.transport.as_deref(), Some("streamable_http"));
        assert_eq!(state.connection_mode.as_deref(), Some("manual"));
        assert!(state.runtime_observed());
        assert_eq!(state.registration_origin(), ClientRegistrationOrigin::RuntimeInitialize);

        let metadata = state.runtime_client_metadata();
        assert_eq!(metadata.description.as_deref(), Some("Observed description"));
        assert_eq!(metadata.homepage_url.as_deref(), Some("https://example.com"));
        assert_eq!(metadata.logo_url.as_deref(), Some("https://example.com/logo.png"));

        let transports = state.parsed_transports().expect("transports json");
        assert!(transports.contains_key("streamable_http"));
        assert!(!transports.contains_key("sse"));
    }

    #[tokio::test]
    async fn handshake_observation_does_not_override_template_client() {
        let (_temp_dir, service) = create_test_service().await;
        service
            .ensure_passive_observed_row("template.client", "Template Client", None)
            .await
            .expect("create template-like client");
        sqlx::query("UPDATE client SET template_identifier = ? WHERE identifier = ?")
            .bind("template.client")
            .bind("template.client")
            .execute(service.db_pool.as_ref())
            .await
            .expect("mark template identifier");
        let template_client = service
            .fetch_state("template.client")
            .await
            .expect("fetch template-like state")
            .expect("template-like state exists");
        let identifier = template_client.identifier.clone();
        let original_display_name = template_client.display_name.clone();
        let original_client_version = template_client.client_version.clone();
        let original_metadata = template_client.runtime_client_metadata();

        service
            .persist_handshake_observation(
                &identifier,
                Some("Changed Template"),
                Some("9.9.9"),
                Some("streamable_http"),
                Some("Changed description"),
                Some("https://changed.example.com"),
                Some("https://changed.example.com/logo.png"),
            )
            .await
            .expect("template handshake observation should be ignored");

        let state = service
            .fetch_state(&identifier)
            .await
            .expect("fetch template state")
            .expect("template state exists");
        assert_eq!(state.display_name, original_display_name);
        assert_eq!(state.client_version, original_client_version);
        assert_eq!(state.runtime_client_metadata(), original_metadata);
    }

    #[tokio::test]
    async fn persist_handshake_observation_rejects_alias_transport_keys() {
        let (_temp_dir, service) = create_test_service().await;

        service
            .ensure_passive_observed_row("test.observed", "Observed", None)
            .await
            .expect("create passive observed row");
        let before = service
            .fetch_state("test.observed")
            .await
            .expect("fetch initial state")
            .expect("state exists");

        let error = service
            .persist_handshake_observation(
                "test.observed",
                Some("Observed App"),
                Some("1.2.3"),
                Some("http"),
                Some("Observed description"),
                Some("https://example.com"),
                Some("https://example.com/logo.png"),
            )
            .await
            .expect_err("alias observed transport should be rejected");

        let message = error.to_string();
        assert!(
            message.contains("Invalid") || message.contains("transport"),
            "unexpected error: {message}"
        );

        let state = service
            .fetch_state("test.observed")
            .await
            .expect("fetch state")
            .expect("state exists");
        assert_eq!(state.display_name, before.display_name);
        assert_eq!(state.client_version, before.client_version);
        assert_eq!(state.transport, before.transport);
        assert_eq!(state.runtime_client_metadata(), before.runtime_client_metadata());
        assert_eq!(state.runtime_observed(), before.runtime_observed());
    }

    #[tokio::test]
    async fn handshake_observation_without_metadata_still_marks_runtime_observed() {
        let (_temp_dir, service) = create_test_service().await;

        service
            .ensure_passive_observed_row("test.empty-observed", "Empty Observed", None)
            .await
            .expect("create passive observed row");

        service
            .persist_handshake_observation("test.empty-observed", None, None, None, None, None, None)
            .await
            .expect("persist empty handshake observation");

        let state = service
            .fetch_state("test.empty-observed")
            .await
            .expect("fetch state")
            .expect("state exists");
        assert!(state.runtime_observed());
        assert_eq!(state.registration_origin(), ClientRegistrationOrigin::RuntimeInitialize);
        assert_eq!(state.display_name(), "Empty Observed");
    }

    #[tokio::test]
    async fn delete_client_record_cleans_runtime_and_allows_recreation() {
        let (_temp_dir, service) = create_test_service().await;

        service
            .set_client_settings("test.client", None, None)
            .await
            .expect("create active client state");
        let surface_store = mcpmate_capability_store::SqliteSurfaceStore::new(service.db_pool.as_ref().clone());
        assert!(
            surface_store
                .load_binding("test.client")
                .await
                .expect("load managed surface before delete")
                .is_some()
        );

        sqlx::query(
            "INSERT OR REPLACE INTO client_template_runtime (identifier, payload_json, updated_at) VALUES (?, ?, CURRENT_TIMESTAMP)",
        )
        .bind("test.client")
        .bind("{}")
        .execute(service.db_pool.as_ref())
        .await
        .expect("insert runtime snapshot");

        let deleted = service
            .delete_client_record("test.client")
            .await
            .expect("delete client record");

        assert!(deleted);
        assert!(
            service
                .fetch_state("test.client")
                .await
                .expect("fetch state after delete")
                .is_none()
        );
        let runtime_payload: Option<String> =
            sqlx::query_scalar("SELECT payload_json FROM client_template_runtime WHERE identifier = ?")
                .bind("test.client")
                .fetch_optional(service.db_pool.as_ref())
                .await
                .expect("query runtime snapshot after delete");
        assert!(runtime_payload.is_none());
        assert!(
            surface_store
                .load_binding("test.client")
                .await
                .expect("load managed surface after delete")
                .is_none()
        );

        let recreated = service
            .ensure_passive_observed_row("test.client", "Test Client", None)
            .await
            .expect("deleted client can be recreated passively");
        assert_eq!(recreated.identifier, "test.client");
    }

    #[tokio::test]
    async fn delete_client_record_generation_aware_deletes_custom_profile() {
        let (_temp_dir, service) = create_test_service().await;

        service
            .set_client_settings("test.custom", None, None)
            .await
            .expect("create custom client state");
        let mut profile = crate::config::models::Profile::new(
            "Test Custom Profile".to_string(),
            crate::common::profile::ProfileType::HostApp,
        );
        profile.id = Some("profile-custom-test".to_string());
        profile.authoring_generation = 7;
        let profile_id = crate::helpers::insert_profile(service.db_pool.as_ref(), &profile).await;
        sqlx::query("UPDATE client SET capability_source = 'custom', custom_profile_id = ? WHERE identifier = ?")
            .bind(&profile_id)
            .bind("test.custom")
            .execute(service.db_pool.as_ref())
            .await
            .expect("assign custom Profile to client");

        assert!(
            service
                .delete_client_record("test.custom")
                .await
                .expect("delete client with custom Profile")
        );

        let profile_exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM profile WHERE id = ?)")
            .bind(&profile_id)
            .fetch_one(service.db_pool.as_ref())
            .await
            .expect("query custom Profile after client cleanup");
        assert!(!profile_exists);
        assert!(
            service
                .fetch_state("test.custom")
                .await
                .expect("fetch custom client after delete")
                .is_none()
        );
    }

    #[tokio::test]
    async fn ensure_local_config_target_metadata_repairs_connection_mode_drift() {
        let (_temp_dir, service) = create_test_service().await;

        sqlx::query(
            r#"
            INSERT INTO client (
                id, name, identifier, config_path, connection_mode, attachment_state,
                config_format, container_type, container_keys, config_file_parse
            )
            VALUES (
                'clnt_drift', 'Cursor Drift', 'cursor.drift', '/tmp/cursor-drift.json',
                'manual', 'not_applicable', 'json', 'object', '["mcpServers"]',
                '{"format":"json","container_type":"object_map","container_keys":["mcpServers"]}'
            )
            "#,
        )
        .execute(service.db_pool.as_ref())
        .await
        .expect("insert drifted client row");

        let repaired = service
            .ensure_local_config_target_metadata("cursor.drift")
            .await
            .expect("repair metadata");
        assert!(repaired);

        let state = service
            .fetch_state("cursor.drift")
            .await
            .expect("fetch state")
            .expect("state exists");
        assert!(state.has_local_config_target());
        assert_eq!(state.attachment_state().as_str(), "detached");
    }
}
