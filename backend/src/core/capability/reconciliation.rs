use std::{collections::HashMap, time::Duration};

use async_trait::async_trait;
use mcpmate_capability_store::{
    CapabilityId, CapabilityObservation, CapabilityRefId, CatalogError, CatalogReconciliation,
    EffectiveCapabilityRecordV1, Result, SqliteCapabilityCatalog, SqliteSurfaceStore, SurfaceManifest,
    SurfaceManifestEntryInput, SurfaceOutboxEvent, SurfacePublication, SurfaceReconciliationJob,
};
use serde_json::json;
use sqlx::{Pool, Row, Sqlite, Transaction};
use uuid::Uuid;

use super::{
    change_policy::{ChangeClass, classify_effective_definition_change},
    dependency::CatalogDependencyRevisions,
    materializer::{
        MaterializationCoordinator, MaterializationTrigger, SurfaceAuthoringLoader, load_default_config_mode,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReconciliationFault {
    None,
    AfterCatalog,
    AfterFirstSafePublication,
    AfterFirstJob,
}

#[async_trait]
pub trait SurfaceOutboxDelivery: Send + Sync {
    async fn deliver(
        &self,
        event: &SurfaceOutboxEvent,
    ) -> Result<()>;
}

#[derive(Clone)]
pub struct CatalogSurfaceReconciler {
    pool: Pool<Sqlite>,
    catalog: SqliteCapabilityCatalog,
    surfaces: SqliteSurfaceStore,
}

impl CatalogSurfaceReconciler {
    pub fn new(pool: Pool<Sqlite>) -> Self {
        Self {
            catalog: SqliteCapabilityCatalog::new(pool.clone()),
            surfaces: SqliteSurfaceStore::new(pool.clone()),
            pool,
        }
    }

    pub async fn reconcile(
        &self,
        observation: CapabilityObservation,
    ) -> Result<CatalogReconciliation> {
        self.reconcile_with_fault(observation, ReconciliationFault::None).await
    }

    pub async fn reconcile_with_fault(
        &self,
        observation: CapabilityObservation,
        fault: ReconciliationFault,
    ) -> Result<CatalogReconciliation> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let result = self
            .reconcile_in_transaction(&mut transaction, observation, fault)
            .await;
        match result {
            Ok(reconciliation) => {
                transaction.commit().await?;
                Ok(reconciliation)
            }
            Err(error) => {
                transaction.rollback().await?;
                Err(error)
            }
        }
    }

    pub async fn reconcile_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        observation: CapabilityObservation,
        fault: ReconciliationFault,
    ) -> Result<CatalogReconciliation> {
        self.reconcile_after_revision_in_transaction(transaction, observation, None, fault)
            .await
    }

    pub async fn reconcile_after_revision_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        observation: CapabilityObservation,
        previous_revision: Option<i64>,
        fault: ReconciliationFault,
    ) -> Result<CatalogReconciliation> {
        let server_id = observation.server_id.clone();
        let reconciliation = match previous_revision {
            Some(previous_revision) => {
                self.catalog
                    .reconcile_observation_after_revision_in(transaction, observation, previous_revision)
                    .await?
            }
            None => self.catalog.reconcile_observation_in(transaction, observation).await?,
        };
        if !reconciliation.commit.changed {
            return Ok(reconciliation);
        }
        if fault == ReconciliationFault::AfterCatalog {
            return Err(injected_fault("after catalog"));
        }
        let cause_id = format!("{server_id}:{}", reconciliation.commit.revision);
        let include_server_intent = !reconciliation.delta.added_refs.is_empty()
            || !reconciliation.delta.changed_versions.is_empty()
            || !reconciliation.delta.unresolved_refs.is_empty()
            || !reconciliation.delta.reappeared_refs.is_empty();
        self.enqueue_consumer_reconciliation_in_transaction(
            transaction,
            &server_id,
            &cause_id,
            include_server_intent,
            fault,
        )
        .await?;
        Ok(reconciliation)
    }

    pub async fn retire_server_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        server_id: &str,
    ) -> Result<Option<CatalogReconciliation>> {
        let Some(reconciliation) = self
            .catalog
            .retire_server_in_transaction(transaction, server_id)
            .await?
        else {
            return Ok(None);
        };
        let cause_id = format!("{server_id}:{}", reconciliation.commit.revision);
        self.enqueue_consumer_reconciliation_in_transaction(
            transaction,
            server_id,
            &cause_id,
            true,
            ReconciliationFault::None,
        )
        .await?;
        Ok(Some(reconciliation))
    }

    async fn enqueue_consumer_reconciliation_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        server_id: &str,
        cause_id: &str,
        include_server_intent: bool,
        fault: ReconciliationFault,
    ) -> Result<()> {
        let consumers = affected_consumers(transaction, server_id, include_server_intent).await?;
        let default_config_mode = load_default_config_mode(&self.pool).await?;
        for (index, (consumer_id, needs_contraction)) in consumers.iter().enumerate() {
            let input = SurfaceAuthoringLoader::load_consumer_input_in_transaction(
                transaction,
                consumer_id,
                &default_config_mode,
            )
            .await?;
            let target_revisions = CatalogDependencyRevisions::derive_in_transaction(
                transaction,
                consumer_id,
                &input.dependency_server_ids,
                Some(server_id),
            )
            .await?;
            let binding = self
                .surfaces
                .load_binding_in_transaction(transaction, consumer_id)
                .await?
                .ok_or_else(|| CatalogError::SurfaceNotFound {
                    entity: "consumer surface binding",
                    id: consumer_id.clone(),
                })?;
            let (safe_binding, publication) = if *needs_contraction {
                let active_manifest_id = self
                    .surfaces
                    .load_publication_manifest_id_in_transaction(transaction, &binding.active_publication_id)
                    .await?;
                let safe_entries = load_safe_entries(transaction, &active_manifest_id).await?;
                let safe_manifest = SurfaceManifest::compile(consumer_id, safe_entries)?;
                self.surfaces
                    .insert_manifest_in_transaction(transaction, &safe_manifest)
                    .await?;
                let publication = SurfacePublication::new(
                    format!("publication-{}", Uuid::new_v4()),
                    consumer_id,
                    safe_manifest.manifest_id,
                    None,
                    "safe_contraction",
                    "catalog_reconciler",
                    Some(binding.active_publication_id),
                );
                let safe_binding = self
                    .surfaces
                    .publish_and_bind_in_transaction(transaction, &publication, Some(binding.generation))
                    .await?;
                if index == 0 && fault == ReconciliationFault::AfterFirstSafePublication {
                    return Err(injected_fault("after first safe publication"));
                }
                (safe_binding, Some(publication))
            } else {
                (binding, None)
            };
            let job = SurfaceReconciliationJob::new(
                "catalog_delta",
                cause_id,
                consumer_id,
                serde_json::to_value(&target_revisions)?,
                safe_binding.generation,
            )?;
            self.surfaces
                .enqueue_reconciliation_job_in_transaction(transaction, &job)
                .await?;
            if index == 0 && fault == ReconciliationFault::AfterFirstJob {
                return Err(injected_fault("after first job"));
            }
            if let Some(publication) = publication {
                self.surfaces
                    .enqueue_outbox_event_in_transaction(
                        transaction,
                        &SurfaceOutboxEvent::new(
                            format!("outbox-{}", job.idempotency_key),
                            "surface_publication_changed",
                            consumer_id,
                            json!({
                                "publicationId": publication.publication_id,
                                "generation": safe_binding.generation,
                                "reason": "safe_contraction",
                            }),
                        ),
                    )
                    .await?;
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct SurfaceReconciliationWorker {
    pool: Pool<Sqlite>,
    store: SqliteSurfaceStore,
    worker_id: String,
    audit_service: Option<std::sync::Arc<crate::audit::AuditService>>,
    outbox_delivery: Option<std::sync::Arc<dyn SurfaceOutboxDelivery>>,
}

impl SurfaceReconciliationWorker {
    pub fn new(
        pool: Pool<Sqlite>,
        worker_id: impl Into<String>,
    ) -> Self {
        Self {
            store: SqliteSurfaceStore::new(pool.clone()),
            pool,
            worker_id: worker_id.into(),
            audit_service: None,
            outbox_delivery: None,
        }
    }

    pub fn with_audit_service(
        mut self,
        audit_service: Option<std::sync::Arc<crate::audit::AuditService>>,
    ) -> Self {
        self.audit_service = audit_service;
        self
    }

    pub fn with_outbox_delivery(
        mut self,
        outbox_delivery: Option<std::sync::Arc<dyn SurfaceOutboxDelivery>>,
    ) -> Self {
        self.outbox_delivery = outbox_delivery;
        self
    }

    pub async fn run_once(&self) -> Result<bool> {
        let Some(job) = self
            .store
            .lease_next_reconciliation_job(&self.worker_id, Duration::from_secs(30))
            .await?
        else {
            return Ok(false);
        };
        let result = self.process_job(&job).await;
        match result {
            Ok(()) => Ok(true),
            Err(error) => {
                self.store
                    .record_reconciliation_failure(
                        &job.idempotency_key,
                        &self.worker_id,
                        &error.to_string(),
                        chrono::Utc::now(),
                    )
                    .await?;
                self.emit_failure_audit(&job, &error).await;
                Err(error)
            }
        }
    }

    pub async fn dispatch_outbox_once(&self) -> Result<usize> {
        let events = self.store.load_pending_outbox_events(50).await?;
        let Some(delivery) = &self.outbox_delivery else {
            return Ok(0);
        };
        for event in &events {
            if event.event_kind != "surface_publication_changed" {
                return Err(CatalogError::InvalidSurfaceValue {
                    field: "surface outbox event kind",
                    value: event.event_kind.clone(),
                });
            }
            delivery.deliver(event).await?;
            self.store.mark_outbox_event_delivered(&event.event_id).await?;
        }
        Ok(events.len())
    }

    async fn process_job(
        &self,
        job: &SurfaceReconciliationJob,
    ) -> Result<()> {
        let expected: CatalogDependencyRevisions = serde_json::from_value(job.target_revision_set.clone())?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        self.store
            .validate_reconciliation_lease_owner_in_transaction(&mut transaction, job, &self.worker_id)
            .await?;
        let actual =
            CatalogDependencyRevisions::load_current_for_expected_servers_in_transaction(&mut transaction, &expected)
                .await?;
        let actual_binding_generation: Option<i64> =
            sqlx::query_scalar("SELECT generation FROM consumer_surface_bindings WHERE consumer_id = ?")
                .bind(&job.consumer_id)
                .fetch_optional(&mut *transaction)
                .await?;
        let catalog_dependencies_changed = actual != expected;
        let binding_generation_changed = actual_binding_generation != Some(job.expected_binding_generation);
        if catalog_dependencies_changed || binding_generation_changed {
            let actual_binding_generation = actual_binding_generation.ok_or_else(|| CatalogError::SurfaceNotFound {
                entity: "consumer surface binding",
                id: job.consumer_id.clone(),
            })?;
            let default_config_mode = load_default_config_mode(&self.pool).await?;
            let input = SurfaceAuthoringLoader::load_consumer_input_in_transaction(
                &mut transaction,
                &job.consumer_id,
                &default_config_mode,
            )
            .await?;
            let cause_server_id = catalog_job_cause_server_id(job)?;
            let successor_revisions = CatalogDependencyRevisions::derive_in_transaction(
                &mut transaction,
                &job.consumer_id,
                &input.dependency_server_ids,
                Some(cause_server_id),
            )
            .await?;
            let supersede_reason = if catalog_dependencies_changed {
                "catalog_dependency_changed"
            } else {
                "binding_generation_changed"
            };
            let successor = SurfaceReconciliationJob::new(
                &job.cause_kind,
                &job.cause_id,
                &job.consumer_id,
                serde_json::to_value(&successor_revisions)?,
                actual_binding_generation,
            )?;
            self.store
                .enqueue_reconciliation_job_in_transaction(&mut transaction, &successor)
                .await?;
            let receipt = json!({
                "outcome": "superseded",
                "supersedeReason": supersede_reason,
                "dependencyServerCount": successor_revisions.server_ids().count(),
                "successorIdempotencyKey": successor.idempotency_key,
                "actualRevisionSet": actual,
                "actualBindingGeneration": actual_binding_generation,
            });
            self.store
                .record_reconciliation_success_in_transaction(
                    &mut transaction,
                    &job.idempotency_key,
                    &self.worker_id,
                    receipt.clone(),
                )
                .await?;
            transaction.commit().await?;
            self.emit_audit(job, &receipt).await;
            return Ok(());
        }
        let (changes, baseline_manifest_id) = load_job_changes(&mut transaction, job).await?;
        let trigger =
            MaterializationTrigger::from_dependencies(&job.cause_kind, &job.cause_id, expected, &self.worker_id)
                .with_review_baseline_manifest_id(baseline_manifest_id);
        let commit = MaterializationCoordinator::new(self.pool.clone())
            .compile_consumer_with_changes_in_transaction(&mut transaction, &job.consumer_id, changes, &trigger)
            .await?;
        let receipt = json!({
            "proposalId": commit.proposal_id,
            "bindingGeneration": commit.binding.as_ref().map(|binding| binding.generation),
            "reviewItemIds": commit.review_item_ids,
            "effectiveSurfaceChanged": commit.effective_surface_changed,
        });
        self.store
            .record_reconciliation_success_in_transaction(
                &mut transaction,
                &job.idempotency_key,
                &self.worker_id,
                receipt.clone(),
            )
            .await?;
        transaction.commit().await?;
        self.emit_audit(job, &receipt).await;
        Ok(())
    }

    async fn emit_audit(
        &self,
        job: &SurfaceReconciliationJob,
        receipt: &serde_json::Value,
    ) {
        let mut data = serde_json::Map::new();
        data.insert(
            "consumer_id".to_string(),
            serde_json::Value::String(job.consumer_id.clone()),
        );
        data.insert(
            "idempotency_key".to_string(),
            serde_json::Value::String(job.idempotency_key.clone()),
        );
        data.insert("target_revision_set".to_string(), job.target_revision_set.clone());
        data.insert("receipt".to_string(), receipt.clone());
        crate::audit::interceptor::emit_event(
            self.audit_service.as_ref(),
            crate::audit::interceptor::build_rest_event(
                crate::audit::AuditAction::SurfaceReconciliation,
                crate::audit::AuditStatus::Success,
                "WORKER",
                "/internal/surface/reconciliation",
                None,
                None,
                None,
                Some(data),
                None,
            ),
        )
        .await;
    }

    async fn emit_failure_audit(
        &self,
        job: &SurfaceReconciliationJob,
        error: &CatalogError,
    ) {
        let mut data = serde_json::Map::new();
        data.insert(
            "consumer_id".to_string(),
            serde_json::Value::String(job.consumer_id.clone()),
        );
        data.insert(
            "idempotency_key".to_string(),
            serde_json::Value::String(job.idempotency_key.clone()),
        );
        data.insert("target_revision_set".to_string(), job.target_revision_set.clone());
        crate::audit::interceptor::emit_event(
            self.audit_service.as_ref(),
            crate::audit::interceptor::build_rest_event(
                crate::audit::AuditAction::SurfaceReconciliation,
                crate::audit::AuditStatus::Failed,
                "WORKER",
                "/internal/surface/reconciliation",
                None,
                None,
                None,
                Some(data),
                Some(error.to_string()),
            ),
        )
        .await;
    }
}

pub fn spawn_surface_background_workers(
    pool: Pool<Sqlite>,
    cancellation_token: tokio_util::sync::CancellationToken,
    audit_service: Option<std::sync::Arc<crate::audit::AuditService>>,
    outbox_delivery: Option<std::sync::Arc<dyn SurfaceOutboxDelivery>>,
) {
    let worker = SurfaceReconciliationWorker::new(pool, format!("surface-worker-{}", Uuid::new_v4()))
        .with_audit_service(audit_service)
        .with_outbox_delivery(outbox_delivery);
    tokio::spawn(async move {
        loop {
            if cancellation_token.is_cancelled() {
                return;
            }
            let reconciliation_result = worker.run_once().await;
            let outbox_result = worker.dispatch_outbox_once().await;
            let had_work = reconciliation_result.as_ref().copied().unwrap_or(false)
                || outbox_result.as_ref().copied().unwrap_or(0) > 0;
            if let Err(error) = reconciliation_result {
                tracing::error!(error = %error, "Surface reconciliation worker iteration failed");
            }
            if let Err(error) = outbox_result {
                tracing::error!(error = %error, "Surface publication outbox delivery failed");
            }
            if !had_work {
                tokio::select! {
                    () = cancellation_token.cancelled() => return,
                    () = tokio::time::sleep(Duration::from_millis(500)) => {}
                }
            }
        }
    });
}

async fn load_job_changes(
    transaction: &mut Transaction<'_, Sqlite>,
    job: &SurfaceReconciliationJob,
) -> Result<(
    HashMap<CapabilityRefId, ChangeClass>,
    mcpmate_capability_store::SurfaceManifestId,
)> {
    let mut publication = sqlx::query(
        r#"
        SELECT publication.publication_id, publication.manifest_id, publication.reason,
               publication.supersedes_publication_id
        FROM consumer_surface_bindings binding
        JOIN surface_publications publication
          ON publication.publication_id = binding.active_publication_id
        WHERE binding.consumer_id = ?
        "#,
    )
    .bind(&job.consumer_id)
    .fetch_one(&mut **transaction)
    .await?;
    let mut traversing_safe_contraction = false;
    let baseline_manifest_id = loop {
        let manifest_id: String = publication.try_get("manifest_id")?;
        let reason: String = publication.try_get("reason")?;
        if reason != "safe_contraction" && traversing_safe_contraction {
            break manifest_id;
        }
        let Some(previous_publication_id) = publication.try_get::<Option<String>, _>("supersedes_publication_id")?
        else {
            break manifest_id;
        };
        let previous = sqlx::query(
            r#"
            SELECT publication_id, manifest_id, reason, supersedes_publication_id
            FROM surface_publications
            WHERE publication_id = ?
            "#,
        )
        .bind(previous_publication_id)
        .fetch_one(&mut **transaction)
        .await?;
        if reason == "safe_contraction" {
            traversing_safe_contraction = true;
            publication = previous;
            continue;
        }
        let previous_manifest_id: String = previous.try_get("manifest_id")?;
        let previous_reason: String = previous.try_get("reason")?;
        if previous_reason == "safe_contraction" && previous_manifest_id == manifest_id {
            publication = previous;
            continue;
        }
        break manifest_id;
    };
    let rows = sqlx::query(
        r#"
        SELECT capability_ref.ref_id, capability_ref.state, capability_ref.state_generation,
               current.capability_id AS current_capability_id,
               baseline.capability_id AS baseline_capability_id,
               current_version.canonical_record AS current_canonical_record,
               baseline_version.canonical_record AS baseline_canonical_record
        FROM capability_refs capability_ref
        LEFT JOIN capability_ref_current current ON current.ref_id = capability_ref.ref_id
        LEFT JOIN capability_versions current_version
          ON current_version.capability_id = current.capability_id
        LEFT JOIN surface_manifest_entries baseline
          ON baseline.manifest_id = ? AND baseline.ref_id = capability_ref.ref_id
        LEFT JOIN capability_versions baseline_version
          ON baseline_version.capability_id = baseline.capability_id
        ORDER BY capability_ref.ref_id
        "#,
    )
    .bind(&baseline_manifest_id)
    .fetch_all(&mut **transaction)
    .await?;
    let changes = rows
        .into_iter()
        .map(|row| {
            let ref_id = row.try_get::<String, _>("ref_id")?.parse::<CapabilityRefId>()?;
            let state: String = row.try_get("state")?;
            let state_generation: i64 = row.try_get("state_generation")?;
            let current: Option<String> = row.try_get("current_capability_id")?;
            let baseline: Option<String> = row.try_get("baseline_capability_id")?;
            let change = if matches!(state.as_str(), "unresolved" | "retired") {
                ChangeClass::Missing
            } else if current != baseline && baseline.is_some() {
                classify_version_change(
                    row.try_get::<Option<Vec<u8>>, _>("baseline_canonical_record")?,
                    row.try_get::<Option<Vec<u8>>, _>("current_canonical_record")?,
                )?
            } else if baseline.is_none() && state_generation > 1 {
                ChangeClass::Reappeared
            } else if baseline.is_none() {
                ChangeClass::NewRef
            } else {
                ChangeClass::Unchanged
            };
            Ok((ref_id, change))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    Ok((changes, baseline_manifest_id.parse()?))
}

fn classify_version_change(
    baseline: Option<Vec<u8>>,
    current: Option<Vec<u8>>,
) -> Result<ChangeClass> {
    let baseline = baseline.ok_or_else(|| CatalogError::IntegrityMismatch {
        identity: "missing baseline capability version".to_string(),
    })?;
    let current = current.ok_or_else(|| CatalogError::IntegrityMismatch {
        identity: "missing current capability version".to_string(),
    })?;
    let baseline: EffectiveCapabilityRecordV1 = serde_json::from_slice(&baseline)?;
    let current: EffectiveCapabilityRecordV1 = serde_json::from_slice(&current)?;
    let baseline = serde_json::to_value(baseline.definition)?;
    let current = serde_json::to_value(current.definition)?;
    Ok(classify_effective_definition_change(&baseline, &current))
}

fn catalog_job_cause_server_id(job: &SurfaceReconciliationJob) -> Result<&str> {
    if job.cause_kind != "catalog_delta" {
        return Err(CatalogError::InvalidSurfaceValue {
            field: "surface reconciliation cause kind",
            value: job.cause_kind.clone(),
        });
    }
    job.cause_id
        .rsplit_once(':')
        .map(|(server_id, _)| server_id)
        .filter(|server_id| !server_id.is_empty())
        .ok_or_else(|| CatalogError::InvalidSurfaceValue {
            field: "surface reconciliation cause id",
            value: job.cause_id.clone(),
        })
}

async fn affected_consumers(
    transaction: &mut Transaction<'_, Sqlite>,
    server_id: &str,
    include_server_intent: bool,
) -> Result<Vec<(String, bool)>> {
    let rows = sqlx::query(
        r#"
        WITH unsafe_consumers AS (
            SELECT DISTINCT b.consumer_id
            FROM consumer_surface_bindings b
            JOIN surface_publications p ON p.publication_id = b.active_publication_id
            JOIN surface_manifest_entries e ON e.manifest_id = p.manifest_id
            JOIN capability_refs r ON r.ref_id = e.ref_id
            LEFT JOIN capability_ref_current c ON c.ref_id = e.ref_id
            WHERE r.server_id = ?
              AND (r.state <> 'active' OR c.capability_id IS NULL OR c.capability_id <> e.capability_id)
        ),
        server_intent_consumers AS (
            SELECT d.consumer_id
            FROM direct_exposure_servers d
            WHERE d.server_id = ? AND ? = 1
            UNION
            SELECT direct_ref.consumer_id
            FROM direct_exposure_refs direct_ref
            JOIN capability_refs capability_ref ON capability_ref.ref_id = direct_ref.ref_id
            WHERE capability_ref.server_id = ? AND direct_ref.enabled = 1 AND ? = 1
            UNION
            SELECT c.identifier
            FROM profile_server_relationships p
            JOIN profile profile_record ON profile_record.id = p.profile_id
            JOIN client c ON (
                c.custom_profile_id = p.profile_id
                OR EXISTS (
                    SELECT 1 FROM json_each(c.selected_profile_ids)
                    WHERE json_each.value = p.profile_id
                )
                OR (c.capability_source = 'activated' AND profile_record.is_active = 1 AND profile_record.profile_mode != 'workflow')
            )
            WHERE p.server_id = ? AND ? = 1
            UNION
            SELECT c.identifier
            FROM profile_capability_refs profile_ref
            JOIN capability_refs capability_ref ON capability_ref.ref_id = profile_ref.ref_id
            JOIN profile profile_record ON profile_record.id = profile_ref.profile_id
            JOIN client c ON (
                c.custom_profile_id = profile_ref.profile_id
                OR EXISTS (
                    SELECT 1 FROM json_each(c.selected_profile_ids)
                    WHERE json_each.value = profile_ref.profile_id
                )
                OR (c.capability_source = 'activated' AND profile_record.is_active = 1 AND profile_record.profile_mode != 'workflow')
            )
            WHERE capability_ref.server_id = ? AND profile_ref.enabled = 1 AND ? = 1
        ),
        candidates AS (
            SELECT consumer_id, 1 AS needs_contraction FROM unsafe_consumers
            UNION ALL
            SELECT consumer_id, 0 AS needs_contraction FROM server_intent_consumers
        )
        SELECT candidates.consumer_id, MAX(candidates.needs_contraction) AS needs_contraction
        FROM candidates
        JOIN consumer_surface_bindings b ON b.consumer_id = candidates.consumer_id
        GROUP BY candidates.consumer_id
        ORDER BY candidates.consumer_id
        "#,
    )
    .bind(server_id)
    .bind(server_id)
    .bind(include_server_intent)
    .bind(server_id)
    .bind(include_server_intent)
    .bind(server_id)
    .bind(include_server_intent)
    .bind(server_id)
    .bind(include_server_intent)
    .fetch_all(&mut **transaction)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get("consumer_id")?,
                row.try_get::<i64, _>("needs_contraction")? == 1,
            ))
        })
        .collect()
}

async fn load_safe_entries(
    transaction: &mut Transaction<'_, Sqlite>,
    manifest_id: &mcpmate_capability_store::SurfaceManifestId,
) -> Result<Vec<SurfaceManifestEntryInput>> {
    let rows = sqlx::query(
        r#"
        SELECT e.ref_id, e.capability_id, v.canonical_record
        FROM surface_manifest_entries e
        JOIN capability_refs r ON r.ref_id = e.ref_id
        JOIN capability_ref_current c ON c.ref_id = e.ref_id
        JOIN capability_versions v ON v.capability_id = e.capability_id
        WHERE e.manifest_id = ?
          AND r.state = 'active'
          AND c.capability_id = e.capability_id
        ORDER BY e.position
        "#,
    )
    .bind(manifest_id.as_str())
    .fetch_all(&mut **transaction)
    .await?;
    rows.into_iter()
        .map(|row| {
            let ref_id = row
                .try_get::<String, _>("ref_id")?
                .parse::<mcpmate_capability_store::CapabilityRefId>()?;
            let capability_id = row.try_get::<String, _>("capability_id")?.parse::<CapabilityId>()?;
            let canonical_record: Vec<u8> = row.try_get("canonical_record")?;
            capability_id.verify_canonical_content(&canonical_record, &canonical_record)?;
            let record: EffectiveCapabilityRecordV1 = serde_json::from_slice(&canonical_record)?;
            record.validate()?;
            if record.ref_id != ref_id {
                return Err(CatalogError::IntegrityMismatch {
                    identity: capability_id.to_string(),
                });
            }
            Ok(SurfaceManifestEntryInput::new(
                ref_id,
                capability_id,
                record.definition.kind(),
                record.definition.external_key(),
            ))
        })
        .collect()
}

fn injected_fault(stage: &str) -> CatalogError {
    CatalogError::InvalidSurfaceValue {
        field: "reconciliation fault",
        value: stage.to_string(),
    }
}
