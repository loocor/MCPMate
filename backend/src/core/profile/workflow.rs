use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
};

use uuid::Uuid;

use mcpmate_capability_store::CapabilityId;
use sqlx::{FromRow, Pool, Sqlite, Transaction};

use crate::config::models::ProfileMode;
use crate::core::capability::materializer::{
    MaterializationCoordinator, MaterializationTrigger, load_default_config_mode,
};
use crate::core::profile::publication::load_unify_consumer_ids_in_transaction;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowBindingPolicy {
    #[default]
    MetaOnDemand,
    Direct,
}

impl WorkflowBindingPolicy {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::MetaOnDemand => "meta_on_demand",
            Self::Direct => "direct",
        }
    }
}

impl FromStr for WorkflowBindingPolicy {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "meta_on_demand" => Ok(Self::MetaOnDemand),
            "direct" => Ok(Self::Direct),
            _ => Err("invalid workflow binding policy"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
pub struct WorkflowBindingCommand {
    pub ref_id: String,
    #[serde(default)]
    pub binding_policy: WorkflowBindingPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
pub struct WorkflowStepCommand {
    #[serde(default)]
    pub step_id: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub bindings: Vec<WorkflowBindingCommand>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowSpecificationSaveCommand {
    pub profile_id: String,
    pub expected_specification_revision: Option<i64>,
    pub validation_notes: Option<String>,
    pub avoid_rules: Option<String>,
    pub steps: Vec<WorkflowStepCommand>,
}

#[derive(Clone, Debug, Eq, PartialEq, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
pub struct WorkflowGuidanceSaveCommand {
    pub expected_specification_revision: Option<i64>,
    pub validation_notes: Option<String>,
    pub avoid_rules: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, schemars::JsonSchema, serde::Serialize)]
pub struct WorkflowSpecification {
    pub profile_id: String,
    pub specification_revision: Option<i64>,
    pub validation_notes: Option<String>,
    pub avoid_rules: Option<String>,
    pub tool_binding_count: i64,
    pub steps: Vec<WorkflowStep>,
}

#[derive(Clone, Debug, Eq, PartialEq, schemars::JsonSchema, serde::Serialize)]
pub struct WorkflowStep {
    pub step_id: String,
    pub title: String,
    pub description: Option<String>,
    pub bindings: Vec<WorkflowBinding>,
}

#[derive(Clone, Debug, Eq, PartialEq, schemars::JsonSchema, serde::Serialize)]
pub struct WorkflowBinding {
    pub ref_id: String,
    pub binding_policy: WorkflowBindingPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq, schemars::JsonSchema, serde::Serialize)]
pub struct WorkflowSpecificationPreview {
    pub profile_id: String,
    pub specification_revision: Option<i64>,
    pub validation_notes: Option<String>,
    pub avoid_rules: Option<String>,
    pub steps: Vec<WorkflowPreviewStep>,
    pub valid: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, schemars::JsonSchema, serde::Serialize)]
pub struct WorkflowPreviewStep {
    pub title: String,
    pub description: Option<String>,
    pub bindings: Vec<WorkflowBindingPreview>,
}

#[derive(Clone, Debug, Eq, PartialEq, schemars::JsonSchema, serde::Serialize)]
pub struct WorkflowBindingPreview {
    pub ref_id: String,
    pub binding_policy: WorkflowBindingPolicy,
    pub validation: WorkflowBindingValidation,
}

#[derive(Clone, Debug, Eq, PartialEq, schemars::JsonSchema, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowBindingValidation {
    Valid,
    Drifted,
    Unavailable,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkflowSpecificationError {
    #[error("workflow Profile '{profile_id}' was not found")]
    NotFound { profile_id: String },
    #[error("Profile '{profile_id}' is not a workflow Profile")]
    InvalidProfileMode { profile_id: String },
    #[error("invalid workflow specification request: {0}")]
    InvalidRequest(String),
    #[error("workflow specification was changed by another author")]
    SpecificationChanged { current_specification_revision: i64 },
    #[error("workflow capability binding is unavailable: {ref_id}")]
    InvalidBinding { ref_id: String },
    #[error("workflow specification database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("workflow capability data is invalid")]
    Capability(#[from] anyhow::Error),
    #[error("failed to publish workflow surface: {0}")]
    SurfacePublication(String),
}

#[derive(Clone)]
pub struct WorkflowSpecificationService {
    pool: Pool<Sqlite>,
}

impl WorkflowSpecificationService {
    pub fn new(pool: Pool<Sqlite>) -> Self {
        Self { pool }
    }

    pub async fn view(
        &self,
        profile_id: &str,
    ) -> Result<WorkflowSpecification, WorkflowSpecificationError> {
        let mut transaction = self.pool.begin().await?;
        verify_workflow_profile(&mut transaction, profile_id).await?;
        let specification = load_specification(&mut transaction, profile_id).await?;
        transaction.commit().await?;
        Ok(specification
            .map(Into::into)
            .unwrap_or_else(|| WorkflowSpecification::empty(profile_id)))
    }

    pub async fn save(
        &self,
        command: WorkflowSpecificationSaveCommand,
    ) -> Result<WorkflowSpecification, WorkflowSpecificationError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let specification = Self::save_in_transaction(&mut transaction, command, &self.pool).await?;
        transaction.commit().await?;
        Ok(specification)
    }

    pub(crate) async fn save_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        command: WorkflowSpecificationSaveCommand,
        pool: &Pool<Sqlite>,
    ) -> Result<WorkflowSpecification, WorkflowSpecificationError> {
        validate_save_command(&command)?;
        verify_workflow_profile(transaction, &command.profile_id).await?;
        let available = available_capability_refs(transaction).await?;
        validate_bindings(&command.steps, &available)?;

        let specification_revision = save_specification(transaction, &command).await?;
        replace_steps(transaction, &command.profile_id, &command.steps, &available).await?;
        rematerialize_unify_if_published(transaction, pool, &command.profile_id, "workflow_specification").await?;
        let specification = load_specification(transaction, &command.profile_id)
            .await?
            .expect("workflow specification exists after save");
        debug_assert_eq!(specification.specification_revision, specification_revision);
        Ok(specification.into())
    }

    pub(crate) async fn save_guidance_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        profile_id: &str,
        command: &WorkflowGuidanceSaveCommand,
    ) -> Result<(), WorkflowSpecificationError> {
        verify_workflow_profile(transaction, profile_id).await?;
        let existing = load_specification(transaction, profile_id).await?;
        match (command.expected_specification_revision, existing.as_ref()) {
            (None, None) => {}
            (Some(expected), Some(specification)) if specification.specification_revision == expected => {}
            _ => return Err(load_specification_conflict(transaction, profile_id).await?),
        }
        save_specification(
            transaction,
            &WorkflowSpecificationSaveCommand {
                profile_id: profile_id.to_string(),
                expected_specification_revision: command.expected_specification_revision,
                validation_notes: command.validation_notes.clone(),
                avoid_rules: command.avoid_rules.clone(),
                steps: Vec::new(),
            },
        )
        .await?;
        Ok(())
    }

    pub async fn delete(
        &self,
        profile_id: &str,
        expected_specification_revision: i64,
    ) -> Result<(), WorkflowSpecificationError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        verify_workflow_profile(&mut transaction, profile_id).await?;
        let removed_step_materials = has_step_materials(&mut transaction, profile_id, &[]).await?;
        let result = sqlx::query(
            "DELETE FROM workflow_profile_specifications WHERE profile_id = ? AND specification_revision = ?",
        )
        .bind(profile_id)
        .bind(expected_specification_revision)
        .execute(&mut *transaction)
        .await?;
        if result.rows_affected() != 1 {
            return Err(load_specification_conflict(&mut transaction, profile_id).await?);
        }
        if removed_step_materials {
            bump_materials_revision(&mut transaction, profile_id).await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn preview(
        &self,
        profile_id: &str,
    ) -> Result<WorkflowSpecificationPreview, WorkflowSpecificationError> {
        let mut transaction = self.pool.begin().await?;
        verify_workflow_profile(&mut transaction, profile_id).await?;
        let specification = load_specification(&mut transaction, profile_id).await?;
        let Some(specification) = specification else {
            transaction.commit().await?;
            return Ok(WorkflowSpecificationPreview::empty(profile_id));
        };
        let available = available_capability_refs(&mut transaction).await?;
        transaction.commit().await?;

        let mut valid = true;
        let steps = specification
            .steps
            .into_iter()
            .map(|step| {
                let bindings = step
                    .bindings
                    .into_iter()
                    .map(|binding| {
                        let validation = available
                            .get(&binding.ref_id)
                            .map(|current| {
                                if current.state_generation == binding.expected_state_generation
                                    && current.last_known_capability_id.to_string() == binding.expected_capability_id
                                {
                                    WorkflowBindingValidation::Valid
                                } else {
                                    WorkflowBindingValidation::Drifted
                                }
                            })
                            .unwrap_or(WorkflowBindingValidation::Unavailable);
                        valid &= validation == WorkflowBindingValidation::Valid;
                        WorkflowBindingPreview {
                            ref_id: binding.ref_id,
                            binding_policy: binding.binding_policy,
                            validation,
                        }
                    })
                    .collect();
                WorkflowPreviewStep {
                    title: step.title,
                    description: step.description,
                    bindings,
                }
            })
            .collect();
        Ok(WorkflowSpecificationPreview {
            profile_id: specification.profile_id,
            specification_revision: Some(specification.specification_revision),
            validation_notes: specification.validation_notes,
            avoid_rules: specification.avoid_rules,
            steps,
            valid,
        })
    }
}

async fn rematerialize_unify_if_published(
    transaction: &mut Transaction<'_, Sqlite>,
    pool: &Pool<Sqlite>,
    profile_id: &str,
    actor: &str,
) -> Result<(), WorkflowSpecificationError> {
    let is_active: bool = sqlx::query_scalar("SELECT is_active FROM profile WHERE id = ?")
        .bind(profile_id)
        .fetch_one(&mut **transaction)
        .await?;
    if !is_active {
        return Ok(());
    }
    let coordinator = MaterializationCoordinator::new(pool.clone());
    let default_config_mode = load_default_config_mode(pool)
        .await
        .map_err(|error| WorkflowSpecificationError::SurfacePublication(error.to_string()))?;
    let consumer_ids = load_unify_consumer_ids_in_transaction(transaction, &default_config_mode)
        .await
        .map_err(|error| WorkflowSpecificationError::SurfacePublication(error.to_string()))?;
    let trigger = MaterializationTrigger::for_consumer(
        "workflow_specification_save",
        format!("{profile_id}:{}", Uuid::new_v4()),
        actor,
    );
    for consumer_id in consumer_ids {
        coordinator
            .compile_consumer_in_transaction_with_default(transaction, &consumer_id, &default_config_mode, &trigger)
            .await
            .map_err(|error| WorkflowSpecificationError::SurfacePublication(error.to_string()))?;
    }
    Ok(())
}

#[derive(FromRow)]
struct BindingRow {
    step_index: i64,
    step_id: String,
    title: String,
    description: Option<String>,
    ref_id: Option<String>,
    binding_policy: Option<String>,
    expected_state_generation: Option<i64>,
    expected_capability_id: Option<String>,
}

#[derive(Clone, Debug)]
struct StoredWorkflowBinding {
    ref_id: String,
    binding_policy: WorkflowBindingPolicy,
    expected_state_generation: i64,
    expected_capability_id: String,
}

#[derive(Clone, Debug)]
struct StoredWorkflowStep {
    step_id: String,
    title: String,
    description: Option<String>,
    bindings: Vec<StoredWorkflowBinding>,
}

#[derive(Clone, Debug)]
struct StoredWorkflowSpecification {
    profile_id: String,
    specification_revision: i64,
    validation_notes: Option<String>,
    avoid_rules: Option<String>,
    tool_binding_count: i64,
    steps: Vec<StoredWorkflowStep>,
}

impl WorkflowSpecification {
    fn empty(profile_id: &str) -> Self {
        Self {
            profile_id: profile_id.to_string(),
            specification_revision: None,
            validation_notes: None,
            avoid_rules: None,
            tool_binding_count: 0,
            steps: Vec::new(),
        }
    }
}

impl WorkflowSpecificationPreview {
    fn empty(profile_id: &str) -> Self {
        Self {
            profile_id: profile_id.to_string(),
            specification_revision: None,
            validation_notes: None,
            avoid_rules: None,
            steps: Vec::new(),
            valid: true,
        }
    }
}

pub(crate) async fn verify_workflow_profile(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
) -> Result<(), WorkflowSpecificationError> {
    let profile_mode: Option<String> = sqlx::query_scalar("SELECT profile_mode FROM profile WHERE id = ?")
        .bind(profile_id)
        .fetch_optional(&mut **transaction)
        .await?;
    let profile_mode = profile_mode.ok_or_else(|| WorkflowSpecificationError::NotFound {
        profile_id: profile_id.to_string(),
    })?;
    if ProfileMode::from_str(&profile_mode).ok() != Some(ProfileMode::Workflow) {
        return Err(WorkflowSpecificationError::InvalidProfileMode {
            profile_id: profile_id.to_string(),
        });
    }
    Ok(())
}

fn validate_save_command(command: &WorkflowSpecificationSaveCommand) -> Result<(), WorkflowSpecificationError> {
    if command.profile_id.trim().is_empty() {
        return Err(WorkflowSpecificationError::InvalidRequest(
            "workflow Profile ID must not be empty".to_string(),
        ));
    }
    let mut step_ids = BTreeSet::new();
    for step in &command.steps {
        if step.title.trim().is_empty() {
            return Err(WorkflowSpecificationError::InvalidRequest(
                "workflow step title must not be empty".to_string(),
            ));
        }
        if let Some(step_id) = &step.step_id {
            if Uuid::parse_str(step_id).is_err() {
                return Err(WorkflowSpecificationError::InvalidRequest(
                    "workflow step ID must be a UUID".to_string(),
                ));
            }
            if !step_ids.insert(step_id) {
                return Err(WorkflowSpecificationError::InvalidRequest(
                    "workflow step IDs must be unique".to_string(),
                ));
            }
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct AvailableWorkflowCapability {
    state_generation: i64,
    last_known_capability_id: CapabilityId,
}

async fn available_capability_refs(
    transaction: &mut Transaction<'_, Sqlite>
) -> Result<BTreeMap<String, AvailableWorkflowCapability>, WorkflowSpecificationError> {
    let rows: Vec<(String, i64, String)> = sqlx::query_as(
        r#"
        SELECT cr.ref_id, cr.state_generation, versions.capability_id
        FROM capability_refs cr
        JOIN server_config server ON server.id = cr.server_id
        LEFT JOIN capability_ref_current current ON current.ref_id = cr.ref_id
        JOIN capability_versions versions ON versions.capability_id = COALESCE(
            current.capability_id,
            (
                SELECT historical.capability_id
                FROM capability_versions historical
                WHERE historical.ref_id = cr.ref_id
                ORDER BY historical.first_observed_revision DESC, historical.capability_id DESC
                LIMIT 1
            )
        )
        WHERE server.enabled = 1 AND cr.state = 'active'
        "#,
    )
    .fetch_all(&mut **transaction)
    .await?;
    let mut available = BTreeMap::new();
    for (ref_id, state_generation, capability_id) in rows {
        let Ok(capability_id) = CapabilityId::from_str(&capability_id) else {
            continue;
        };
        available.insert(
            ref_id,
            AvailableWorkflowCapability {
                state_generation,
                last_known_capability_id: capability_id,
            },
        );
    }
    Ok(available)
}

fn validate_bindings(
    steps: &[WorkflowStepCommand],
    available: &BTreeMap<String, AvailableWorkflowCapability>,
) -> Result<(), WorkflowSpecificationError> {
    for binding in steps.iter().flat_map(|step| &step.bindings) {
        if !available.contains_key(&binding.ref_id) {
            return Err(WorkflowSpecificationError::InvalidBinding {
                ref_id: binding.ref_id.clone(),
            });
        }
    }
    Ok(())
}

async fn save_specification(
    transaction: &mut Transaction<'_, Sqlite>,
    command: &WorkflowSpecificationSaveCommand,
) -> Result<i64, WorkflowSpecificationError> {
    match command.expected_specification_revision {
        None => {
            let inserted = sqlx::query(
                "INSERT INTO workflow_profile_specifications (
                    profile_id, specification_revision, validation_notes, avoid_rules
                 ) VALUES (?, 0, ?, ?)",
            )
            .bind(&command.profile_id)
            .bind(&command.validation_notes)
            .bind(&command.avoid_rules)
            .execute(&mut **transaction)
            .await;
            match inserted {
                Ok(_) => Ok(0),
                Err(error) if is_unique_constraint(&error) => {
                    Err(load_specification_conflict(transaction, &command.profile_id).await?)
                }
                Err(error) => Err(error.into()),
            }
        }
        Some(expected_revision) => {
            let revision: Option<i64> = sqlx::query_scalar(
                "UPDATE workflow_profile_specifications
                 SET specification_revision = specification_revision + 1,
                     validation_notes = ?,
                     avoid_rules = ?,
                     updated_at = CURRENT_TIMESTAMP
                 WHERE profile_id = ? AND specification_revision = ?
                 RETURNING specification_revision",
            )
            .bind(&command.validation_notes)
            .bind(&command.avoid_rules)
            .bind(&command.profile_id)
            .bind(expected_revision)
            .fetch_optional(&mut **transaction)
            .await?;
            revision.ok_or(load_specification_conflict(transaction, &command.profile_id).await?)
        }
    }
}

async fn replace_steps(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
    steps: &[WorkflowStepCommand],
    available: &BTreeMap<String, AvailableWorkflowCapability>,
) -> Result<(), WorkflowSpecificationError> {
    let step_ids: Vec<String> = steps
        .iter()
        .map(|step| step.step_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string()))
        .collect();
    sqlx::query("DELETE FROM workflow_profile_step_bindings WHERE profile_id = ?")
        .bind(profile_id)
        .execute(&mut **transaction)
        .await?;
    if step_ids.is_empty() {
        let removed_step_materials = has_step_materials(transaction, profile_id, &step_ids).await?;
        sqlx::query("DELETE FROM workflow_profile_steps WHERE profile_id = ?")
            .bind(profile_id)
            .execute(&mut **transaction)
            .await?;
        if removed_step_materials {
            bump_materials_revision(transaction, profile_id).await?;
        }
    } else {
        let removed_step_materials = has_step_materials(transaction, profile_id, &step_ids).await?;
        let mut deleted_steps =
            sqlx::QueryBuilder::<Sqlite>::new("DELETE FROM workflow_profile_steps WHERE profile_id = ");
        deleted_steps.push_bind(profile_id).push(" AND step_id NOT IN (");
        let mut separated = deleted_steps.separated(", ");
        for step_id in &step_ids {
            separated.push_bind(step_id);
        }
        separated.push_unseparated(")");
        deleted_steps.build().execute(&mut **transaction).await?;
        if removed_step_materials {
            bump_materials_revision(transaction, profile_id).await?;
        }
        let maximum_existing_step_index: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(step_index), -1) + 1 FROM workflow_profile_steps WHERE profile_id = ?",
        )
        .bind(profile_id)
        .fetch_one(&mut **transaction)
        .await?;
        let temporary_index_offset = maximum_existing_step_index.max(steps.len() as i64);
        sqlx::query("UPDATE workflow_profile_steps SET step_index = step_index + ? WHERE profile_id = ?")
            .bind(temporary_index_offset)
            .bind(profile_id)
            .execute(&mut **transaction)
            .await?;
    }
    for ((step_index, step), step_id) in steps.iter().enumerate().zip(&step_ids) {
        sqlx::query(
            "INSERT INTO workflow_profile_steps (profile_id, step_index, step_id, title, description)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(profile_id, step_id) DO UPDATE SET
                step_index = excluded.step_index,
                title = excluded.title,
                description = excluded.description",
        )
        .bind(profile_id)
        .bind(step_index as i64)
        .bind(step_id)
        .bind(&step.title)
        .bind(&step.description)
        .execute(&mut **transaction)
        .await?;
        for (binding_index, binding) in step.bindings.iter().enumerate() {
            let capability = &available[&binding.ref_id];
            sqlx::query(
                "INSERT INTO workflow_profile_step_bindings (
                    profile_id, step_index, binding_index, ref_id, binding_policy,
                    expected_state_generation, expected_capability_id
                 ) VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(profile_id)
            .bind(step_index as i64)
            .bind(binding_index as i64)
            .bind(&binding.ref_id)
            .bind(binding.binding_policy.as_str())
            .bind(capability.state_generation)
            .bind(capability.last_known_capability_id.to_string())
            .execute(&mut **transaction)
            .await?;
        }
    }
    Ok(())
}

async fn has_step_materials(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
    retained_step_ids: &[String],
) -> Result<bool, WorkflowSpecificationError> {
    let mut query = sqlx::QueryBuilder::<Sqlite>::new(
        "SELECT EXISTS(SELECT 1 FROM workflow_profile_step_materials WHERE profile_id = ",
    );
    query.push_bind(profile_id);
    if retained_step_ids.is_empty() {
        query.push(")");
    } else {
        query.push(" AND step_id NOT IN (");
        let mut separated = query.separated(", ");
        for step_id in retained_step_ids {
            separated.push_bind(step_id);
        }
        separated.push_unseparated("))");
    }
    Ok(query.build_query_scalar().fetch_one(&mut **transaction).await?)
}

async fn bump_materials_revision(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
) -> Result<(), WorkflowSpecificationError> {
    sqlx::query(
        "UPDATE workflow_profile_material_libraries \
         SET materials_revision = materials_revision + 1, updated_at = CURRENT_TIMESTAMP \
         WHERE profile_id = ?",
    )
    .bind(profile_id)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn load_specification(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
) -> Result<Option<StoredWorkflowSpecification>, WorkflowSpecificationError> {
    let specification: Option<(i64, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT specification_revision, validation_notes, avoid_rules
         FROM workflow_profile_specifications WHERE profile_id = ?",
    )
    .bind(profile_id)
    .fetch_optional(&mut **transaction)
    .await?;
    let Some((specification_revision, validation_notes, avoid_rules)) = specification else {
        return Ok(None);
    };
    let rows: Vec<BindingRow> = sqlx::query_as(
        "SELECT step.step_index, step.step_id, step.title, step.description, binding.ref_id, binding.binding_policy,
                binding.expected_state_generation, binding.expected_capability_id
         FROM workflow_profile_steps step
         LEFT JOIN workflow_profile_step_bindings binding
           ON binding.profile_id = step.profile_id AND binding.step_index = step.step_index
         WHERE step.profile_id = ?
         ORDER BY step.step_index, binding.binding_index",
    )
    .bind(profile_id)
    .fetch_all(&mut **transaction)
    .await?;
    let tool_binding_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM workflow_profile_step_bindings binding
         JOIN capability_refs capability ON capability.ref_id = binding.ref_id
         WHERE binding.profile_id = ? AND capability.kind = 'tools'",
    )
    .bind(profile_id)
    .fetch_one(&mut **transaction)
    .await?;
    let mut steps: BTreeMap<i64, StoredWorkflowStep> = BTreeMap::new();
    for row in rows {
        let step = steps.entry(row.step_index).or_insert_with(|| StoredWorkflowStep {
            step_id: row.step_id,
            title: row.title,
            description: row.description,
            bindings: Vec::new(),
        });
        if let Some(ref_id) = row.ref_id {
            let binding_policy = row.binding_policy.expect("binding policy exists with binding ref");
            let expected_state_generation = row
                .expected_state_generation
                .expect("state generation exists with binding ref");
            let expected_capability_id = row
                .expected_capability_id
                .expect("capability ID exists with binding ref");
            step.bindings.push(StoredWorkflowBinding {
                ref_id,
                binding_policy: WorkflowBindingPolicy::from_str(&binding_policy).map_err(|_| {
                    WorkflowSpecificationError::InvalidRequest("invalid stored workflow binding policy".to_string())
                })?,
                expected_state_generation,
                expected_capability_id,
            });
        }
    }
    Ok(Some(StoredWorkflowSpecification {
        profile_id: profile_id.to_string(),
        specification_revision,
        validation_notes,
        avoid_rules,
        tool_binding_count,
        steps: steps.into_values().collect(),
    }))
}

impl From<StoredWorkflowSpecification> for WorkflowSpecification {
    fn from(specification: StoredWorkflowSpecification) -> Self {
        Self {
            profile_id: specification.profile_id,
            specification_revision: Some(specification.specification_revision),
            validation_notes: specification.validation_notes,
            avoid_rules: specification.avoid_rules,
            tool_binding_count: specification.tool_binding_count,
            steps: specification
                .steps
                .into_iter()
                .map(|step| WorkflowStep {
                    step_id: step.step_id,
                    title: step.title,
                    description: step.description,
                    bindings: step
                        .bindings
                        .into_iter()
                        .map(|binding| WorkflowBinding {
                            ref_id: binding.ref_id,
                            binding_policy: binding.binding_policy,
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

async fn load_specification_conflict(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
) -> Result<WorkflowSpecificationError, WorkflowSpecificationError> {
    let current: Option<i64> =
        sqlx::query_scalar("SELECT specification_revision FROM workflow_profile_specifications WHERE profile_id = ?")
            .bind(profile_id)
            .fetch_optional(&mut **transaction)
            .await?;
    match current {
        Some(current_specification_revision) => Ok(WorkflowSpecificationError::SpecificationChanged {
            current_specification_revision,
        }),
        None => Ok(WorkflowSpecificationError::NotFound {
            profile_id: profile_id.to_string(),
        }),
    }
}

fn is_unique_constraint(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(database_error) if database_error.is_unique_violation())
}
