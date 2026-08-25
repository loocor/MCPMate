use anyhow::{Context, Result, anyhow};
use sqlx::{Pool, Sqlite};

use super::namespace::{suggest_server_namespace, validate_server_namespace};
use crate::core::capability::naming::{
    ExternalIdentifierCollision, NamingKind, begin_naming_transaction, rebuild_server_external_identifiers,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NamespaceRepairOutcome {
    Noop { namespace: String },
    Repaired(NamespaceRepairSummary),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamespaceRepairSummary {
    pub server_id: String,
    pub old_namespace: String,
    pub new_namespace: String,
    pub identifier_changes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamespaceIssue {
    pub kind: NamespaceIssueKind,
    pub current_namespace: String,
    pub suggested_namespace: Option<String>,
    pub conflicts: Vec<NamespaceConflict>,
    pub external_identifier: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NamespaceIssueKind {
    InvalidNamespace,
    CapabilityCollision,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamespaceConflict {
    pub server_id: String,
    pub namespace: String,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct NamespaceExposureBlocked {
    message: String,
}

fn exposure_blocked(message: impl Into<String>) -> anyhow::Error {
    NamespaceExposureBlocked {
        message: message.into(),
    }
    .into()
}

pub fn is_namespace_exposure_blocked(error: &anyhow::Error) -> bool {
    error.downcast_ref::<NamespaceExposureBlocked>().is_some()
}

async fn load_namespaces(pool: &Pool<Sqlite>) -> Result<Vec<(String, String)>> {
    sqlx::query_as::<_, (String, String)>("SELECT id, name FROM server_config ORDER BY created_at, id")
        .fetch_all(pool)
        .await
        .context("Failed to load server namespaces for canonicalization preflight")
}

pub(crate) async fn record_capability_collision(
    pool: &Pool<Sqlite>,
    collision: &ExternalIdentifierCollision,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO server_namespace_issue (
            server_id, issue_kind, capability_kind, external_identifier, upstream_value,
            conflicting_server_id, conflicting_upstream_value
        ) VALUES (?, 'capability_collision', ?, ?, ?, ?, ?)
        ON CONFLICT(server_id) DO UPDATE SET
            issue_kind = excluded.issue_kind,
            capability_kind = excluded.capability_kind,
            external_identifier = excluded.external_identifier,
            upstream_value = excluded.upstream_value,
            conflicting_server_id = excluded.conflicting_server_id,
            conflicting_upstream_value = excluded.conflicting_upstream_value,
            updated_at = CURRENT_TIMESTAMP
        "#,
    )
    .bind(&collision.server_id)
    .bind(collision.kind.as_str())
    .bind(&collision.external_identifier)
    .bind(&collision.upstream_value)
    .bind(&collision.conflicting_server_id)
    .bind(&collision.conflicting_upstream_value)
    .execute(pool)
    .await
    .context("Failed to persist capability collision for namespace remediation")?;

    crate::core::events::EventBus::global().publish(crate::core::events::Event::CapabilityCollisionDetected {
        server_id: collision.server_id.clone(),
        conflicting_server_id: collision.conflicting_server_id.clone(),
        external_identifier: collision.external_identifier.clone(),
    });
    Ok(())
}

pub(crate) async fn record_capability_collision_from_error(
    pool: &Pool<Sqlite>,
    error: &anyhow::Error,
) -> Result<Option<ExternalIdentifierCollision>> {
    let Some(collision) = error.downcast_ref::<ExternalIdentifierCollision>().cloned() else {
        return Ok(None);
    };

    record_capability_collision(pool, &collision).await?;
    Ok(Some(collision))
}

async fn load_capability_collision_issue(
    pool: &Pool<Sqlite>,
    server_id: &str,
    current_namespace: &str,
) -> Result<Option<NamespaceIssue>> {
    let row = sqlx::query_as::<_, (String, String, String)>(
        r#"
        SELECT issue.external_identifier, issue.conflicting_server_id, owner.name
        FROM server_namespace_issue issue
        JOIN server_config owner ON owner.id = issue.conflicting_server_id
        WHERE issue.server_id = ? AND issue.issue_kind = 'capability_collision'
        "#,
    )
    .bind(server_id)
    .fetch_optional(pool)
    .await
    .context("Failed to load persisted capability collision")?;

    Ok(row.map(
        |(external_identifier, conflicting_server_id, conflicting_namespace)| NamespaceIssue {
            kind: NamespaceIssueKind::CapabilityCollision,
            current_namespace: current_namespace.to_string(),
            suggested_namespace: None,
            conflicts: vec![NamespaceConflict {
                server_id: conflicting_server_id,
                namespace: conflicting_namespace,
            }],
            external_identifier: Some(external_identifier),
        },
    ))
}

fn find_conflicts(
    rows: &[(String, String)],
    server_id: &str,
    candidate: &str,
) -> Vec<NamespaceConflict> {
    rows.iter()
        .filter(|(id, _)| id != server_id)
        .filter(|(_, namespace)| validate_server_namespace(namespace).is_ok())
        .filter(|(_, namespace)| namespace == candidate)
        .map(|(server_id, namespace)| NamespaceConflict {
            server_id: server_id.clone(),
            namespace: namespace.clone(),
        })
        .collect()
}

pub async fn inspect_namespace_issue(
    pool: &Pool<Sqlite>,
    server_id: &str,
) -> Result<Option<NamespaceIssue>> {
    let rows = load_namespaces(pool).await?;
    let current_namespace = rows
        .iter()
        .find_map(|(id, namespace)| (id == server_id).then(|| namespace.clone()))
        .ok_or_else(|| anyhow!("Server '{}' not found for namespace inspection", server_id))?;
    if let Some(issue) = load_capability_collision_issue(pool, server_id, &current_namespace).await? {
        return Ok(Some(issue));
    }
    if validate_server_namespace(&current_namespace).is_ok() {
        return Ok(None);
    }

    let suggested_namespace = suggest_server_namespace(&current_namespace);
    let conflicts = suggested_namespace
        .as_deref()
        .map(|candidate| find_conflicts(&rows, server_id, candidate))
        .unwrap_or_default();
    Ok(Some(NamespaceIssue {
        kind: NamespaceIssueKind::InvalidNamespace,
        current_namespace,
        suggested_namespace,
        conflicts,
        external_identifier: None,
    }))
}

pub async fn ensure_canonical_namespace_before_exposure(
    pool: &Pool<Sqlite>,
    server_id: &str,
) -> Result<NamespaceRepairOutcome> {
    let rows = load_namespaces(pool).await?;
    let old_namespace = rows
        .iter()
        .find_map(|(id, name)| (id == server_id).then(|| name.clone()))
        .ok_or_else(|| anyhow!("Server '{}' not found for namespace canonicalization", server_id))?;

    if let Some(issue) = load_capability_collision_issue(pool, server_id, &old_namespace).await? {
        return Err(exposure_blocked(format!(
            "Server '{}' is blocked because external capability identifier '{}' conflicts with server '{}'",
            server_id,
            issue.external_identifier.as_deref().unwrap_or("unknown"),
            issue
                .conflicts
                .first()
                .map(|conflict| conflict.server_id.as_str())
                .unwrap_or("unknown")
        )));
    }

    if validate_server_namespace(&old_namespace).is_ok() {
        return Ok(NamespaceRepairOutcome::Noop {
            namespace: old_namespace,
        });
    }

    let candidate = suggest_server_namespace(&old_namespace).ok_or_else(|| {
        exposure_blocked(format!(
            "Server '{}' namespace '{}' cannot be canonicalized safely",
            server_id, old_namespace
        ))
    })?;

    let conflicts = find_conflicts(&rows, server_id, &candidate);
    if !conflicts.is_empty() {
        let owners = conflicts
            .iter()
            .map(|conflict| format!("{} ('{}')", conflict.server_id, conflict.namespace))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(exposure_blocked(format!(
            "Server '{}' namespace '{}' cannot be canonicalized to '{}' because it conflicts with {}",
            server_id, old_namespace, candidate, owners
        )));
    }

    match repair_namespace(pool, server_id, old_namespace, candidate, None).await {
        Err(error) if error.downcast_ref::<ExternalIdentifierCollision>().is_some() => {
            record_capability_collision_from_error(pool, &error).await?;
            Err(exposure_blocked(error.to_string()))
        }
        outcome => outcome,
    }
}

pub async fn remediate_namespace(
    pool: &Pool<Sqlite>,
    server_id: &str,
    requested_namespace: &str,
) -> Result<NamespaceRepairOutcome> {
    remediate_namespace_inner(pool, server_id, requested_namespace, None).await
}

pub async fn remediate_namespace_with_snapshot(
    pool: &Pool<Sqlite>,
    server_id: &str,
    requested_namespace: &str,
    snapshot: &mut crate::config::server::capabilities::CapabilitySnapshot,
) -> Result<NamespaceRepairOutcome> {
    remediate_namespace_inner(pool, server_id, requested_namespace, Some(snapshot)).await
}

async fn remediate_namespace_inner(
    pool: &Pool<Sqlite>,
    server_id: &str,
    requested_namespace: &str,
    snapshot: Option<&mut crate::config::server::capabilities::CapabilitySnapshot>,
) -> Result<NamespaceRepairOutcome> {
    validate_server_namespace(requested_namespace)?;
    let rows = load_namespaces(pool).await?;
    let old_namespace = rows
        .iter()
        .find_map(|(id, namespace)| (id == server_id).then(|| namespace.clone()))
        .ok_or_else(|| anyhow!("Server '{}' not found for namespace remediation", server_id))?;
    let collision_issue = load_capability_collision_issue(pool, server_id, &old_namespace)
        .await?
        .is_some();
    if validate_server_namespace(&old_namespace).is_ok() && !collision_issue {
        return Err(anyhow!(
            "Server '{}' already has canonical namespace '{}'; ordinary namespace rename is not allowed",
            server_id,
            old_namespace
        ));
    }

    let conflicts = find_conflicts(&rows, server_id, requested_namespace);
    if !conflicts.is_empty() {
        let owners = conflicts
            .iter()
            .map(|conflict| format!("{} ('{}')", conflict.server_id, conflict.namespace))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(anyhow!(
            "Server '{}' namespace '{}' cannot be remediated to '{}' because it conflicts with {}",
            server_id,
            old_namespace,
            requested_namespace,
            owners
        ));
    }

    repair_namespace(
        pool,
        server_id,
        old_namespace,
        requested_namespace.to_string(),
        snapshot,
    )
    .await
}

async fn repair_namespace(
    pool: &Pool<Sqlite>,
    server_id: &str,
    old_namespace: String,
    candidate: String,
    snapshot: Option<&mut crate::config::server::capabilities::CapabilitySnapshot>,
) -> Result<NamespaceRepairOutcome> {
    let mut tx = begin_naming_transaction(pool)
        .await
        .context("Failed to begin canonical namespace repair transaction")?;

    sqlx::query("UPDATE server_config SET name = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?")
        .bind(&candidate)
        .bind(server_id)
        .execute(&mut *tx)
        .await
        .context("Failed to update canonical server namespace")?;

    for table in ["server_args", "server_env"] {
        let update = format!("UPDATE {table} SET server_name = ? WHERE server_id = ?");
        sqlx::query(&update)
            .bind(&candidate)
            .bind(server_id)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("Failed to update denormalized namespace in {table}"))?;
    }
    for table in [
        "server_meta",
        "server_tools",
        "server_prompts",
        "server_resources",
        "server_resource_templates",
    ] {
        let update = format!("UPDATE {table} SET server_name = ?, updated_at = CURRENT_TIMESTAMP WHERE server_id = ?");
        sqlx::query(&update)
            .bind(&candidate)
            .bind(server_id)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("Failed to update denormalized namespace in {table}"))?;
    }

    crate::core::capability::resource_registry::remap_issued_resource_routes(&mut tx, server_id, &candidate).await?;

    let identifier_changes = if let Some(snapshot) = snapshot {
        crate::config::server::capabilities::apply_snapshot_catalog_in_transaction(
            &mut tx, server_id, &candidate, snapshot,
        )
        .await?;
        Vec::new()
    } else {
        rebuild_server_external_identifiers(&mut tx, server_id, &candidate).await?
    };

    sqlx::query("DELETE FROM server_namespace_issue WHERE server_id = ?")
        .bind(server_id)
        .execute(&mut *tx)
        .await
        .context("Failed to clear resolved namespace issue")?;

    tx.commit()
        .await
        .context("Failed to commit canonical namespace repair")?;

    crate::core::capability::resolver::remove_by_id(server_id).await;
    crate::core::capability::resolver::upsert(server_id, &candidate).await;

    let tool_changes = identifier_changes
        .iter()
        .filter(|change| change.kind == NamingKind::Tool)
        .count();
    let prompt_changes = identifier_changes
        .iter()
        .filter(|change| change.kind == NamingKind::Prompt)
        .count();
    let resource_changes = identifier_changes
        .iter()
        .filter(|change| change.kind == NamingKind::Resource)
        .count();
    let template_changes = identifier_changes
        .iter()
        .filter(|change| change.kind == NamingKind::ResourceTemplate)
        .count();

    crate::core::events::EventBus::global().publish(crate::core::events::Event::ServerNamespaceRepaired {
        server_id: server_id.to_string(),
        old_namespace: old_namespace.clone(),
        new_namespace: candidate.clone(),
        outcome: "repaired".to_string(),
        tool_changes,
        prompt_changes,
        resource_changes,
        template_changes,
    });

    tracing::info!(
        server_id,
        old_namespace,
        new_namespace = %candidate,
        identifier_changes = identifier_changes.len(),
        "Canonicalized legacy server namespace before downstream exposure"
    );

    Ok(NamespaceRepairOutcome::Repaired(NamespaceRepairSummary {
        server_id: server_id.to_string(),
        old_namespace,
        new_namespace: candidate,
        identifier_changes: identifier_changes.len(),
    }))
}

#[cfg(test)]
mod tests {
    use sqlx::{Pool, Sqlite, sqlite::SqlitePoolOptions};

    use super::{
        NamespaceRepairOutcome, ensure_canonical_namespace_before_exposure, inspect_namespace_issue,
        record_capability_collision, record_capability_collision_from_error, remediate_namespace,
        remediate_namespace_with_snapshot,
    };
    use crate::core::capability::naming::{ExternalIdentifierCollision, NamingKind};

    async fn test_pool() -> Pool<Sqlite> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory database");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::initialization::run_initialization(&pool)
            .await
            .expect("initialize database");
        pool
    }

    async fn insert_server(
        pool: &Pool<Sqlite>,
        id: &str,
        namespace: &str,
    ) {
        sqlx::query(
            r#"
            INSERT INTO server_config (id, name, server_type, command, enabled)
            VALUES (?, ?, 'stdio', 'demo-command', 1)
            "#,
        )
        .bind(id)
        .bind(namespace)
        .execute(pool)
        .await
        .expect("insert server");
    }

    async fn insert_legacy_fixture(pool: &Pool<Sqlite>) {
        insert_server(pool, "server-legacy", "Sequential Thinking").await;
        sqlx::query(
            "INSERT INTO server_args (id, server_id, server_name, arg_index, arg_value) VALUES ('arg-1', 'server-legacy', 'Sequential Thinking', 0, '--demo')",
        )
        .execute(pool)
        .await
        .expect("insert server arg");
        sqlx::query(
            "INSERT INTO server_env (id, server_id, server_name, env_key, env_value) VALUES ('env-1', 'server-legacy', 'Sequential Thinking', 'DEMO', '1')",
        )
        .execute(pool)
        .await
        .expect("insert server env");
        sqlx::query(
            "INSERT INTO server_meta (id, server_id, server_name) VALUES ('meta-1', 'server-legacy', 'Sequential Thinking')",
        )
        .execute(pool)
        .await
        .expect("insert server meta");

        sqlx::query("INSERT INTO profile (id, name, type, is_active) VALUES ('profile-1', 'Test', 'user', 1)")
            .execute(pool)
            .await
            .expect("insert profile");
        sqlx::query(
            "INSERT INTO profile_server_relationships (profile_id, server_id, new_ref_policy) VALUES ('profile-1', 'server-legacy', 'review')",
        )
        .execute(pool)
        .await
        .expect("insert profile server");

        sqlx::query(
            "INSERT INTO server_tools (id, server_id, server_name, tool_name, unique_name) VALUES ('tool-1', 'server-legacy', 'Sequential Thinking', 'get_sequential_thinking_status', 'old_tool')",
        )
        .execute(pool)
        .await
        .expect("insert tool");
        sqlx::query(
            "INSERT INTO server_prompts (id, server_id, server_name, prompt_name, unique_name) VALUES ('prompt-1', 'server-legacy', 'Sequential Thinking', 'sequential_thinking_help', 'old_prompt')",
        )
        .execute(pool)
        .await
        .expect("insert prompt");
        sqlx::query(
            "INSERT INTO server_resources (id, server_id, server_name, resource_uri, unique_uri) VALUES ('resource-1', 'server-legacy', 'Sequential Thinking', 'file:///guide.md', 'old_resource')",
        )
        .execute(pool)
        .await
        .expect("insert resource");
        sqlx::query(
            "INSERT INTO server_resource_templates (id, server_id, server_name, uri_template, unique_name, name) VALUES ('template-1', 'server-legacy', 'Sequential Thinking', 'demo://resource/lookup/{id}', 'old_template', 'Lookup')",
        )
        .execute(pool)
        .await
        .expect("insert resource template");
        sqlx::query(
            "INSERT INTO server_issued_resources (id, server_id, server_name, resource_uri, unique_uri) VALUES ('issued-1', 'server-legacy', 'Sequential Thinking', 'file:///guide.md', 'old_issued_resource')",
        )
        .execute(pool)
        .await
        .expect("insert issued resource");

        sqlx::query("INSERT INTO client (id, name, identifier) VALUES ('client-1', 'Test Client', 'test-client')")
            .execute(pool)
            .await
            .expect("insert client");
        sqlx::query(
            "INSERT INTO direct_exposure_servers (consumer_id, server_id, new_ref_policy) VALUES ('test-client', 'server-legacy', 'follow')",
        )
        .execute(pool)
        .await
        .expect("insert direct exposure server");
    }

    #[tokio::test]
    async fn canonical_namespace_is_an_idempotent_noop() {
        let pool = test_pool().await;
        insert_server(&pool, "server-canonical", "sequential_thinking").await;

        let outcome = ensure_canonical_namespace_before_exposure(&pool, "server-canonical")
            .await
            .expect("canonical namespace should pass");

        assert_eq!(
            outcome,
            NamespaceRepairOutcome::Noop {
                namespace: "sequential_thinking".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn legacy_namespace_conflict_fails_without_writes() {
        let pool = test_pool().await;
        insert_server(&pool, "server-canonical", "sequential_thinking").await;
        insert_server(&pool, "server-legacy", "Sequential Thinking").await;

        let error = ensure_canonical_namespace_before_exposure(&pool, "server-legacy")
            .await
            .expect_err("canonical owner must block legacy repair");

        assert!(error.to_string().contains("sequential_thinking"));
        let namespace: String = sqlx::query_scalar("SELECT name FROM server_config WHERE id = ?")
            .bind("server-legacy")
            .fetch_one(&pool)
            .await
            .expect("load unchanged namespace");
        assert_eq!(namespace, "Sequential Thinking");

        let issue = inspect_namespace_issue(&pool, "server-legacy")
            .await
            .expect("inspect namespace issue")
            .expect("legacy namespace issue");
        assert_eq!(issue.suggested_namespace.as_deref(), Some("sequential_thinking"));
        assert_eq!(issue.conflicts.len(), 1);
        assert_eq!(issue.conflicts[0].server_id, "server-canonical");
    }

    #[tokio::test]
    async fn manual_remediation_is_limited_to_invalid_legacy_namespaces() {
        let pool = test_pool().await;
        insert_server(&pool, "server-canonical", "stable_namespace").await;

        let error = remediate_namespace(&pool, "server-canonical", "renamed_namespace")
            .await
            .expect_err("ordinary canonical namespace rename must fail");

        assert!(error.to_string().contains("ordinary namespace rename is not allowed"));
        let namespace: String = sqlx::query_scalar("SELECT name FROM server_config WHERE id = 'server-canonical'")
            .fetch_one(&pool)
            .await
            .expect("load unchanged namespace");
        assert_eq!(namespace, "stable_namespace");
    }

    #[tokio::test]
    async fn capability_collision_enters_namespace_remediation_without_renaming_capabilities() {
        let pool = test_pool().await;
        insert_server(&pool, "server-owner", "a").await;
        insert_server(&pool, "server-challenger", "a_b").await;
        sqlx::query(
            "INSERT INTO server_tools (id, server_id, server_name, tool_name, unique_name) VALUES ('owner-tool', 'server-owner', 'a', 'b_c', 'a_b_c')",
        )
        .execute(&pool)
        .await
        .expect("insert owner tool");
        sqlx::query(
            "INSERT INTO server_tools (id, server_id, server_name, tool_name, unique_name) VALUES ('challenger-tool', 'server-challenger', 'a_b', 'c', 'a_b_old_c')",
        )
        .execute(&pool)
        .await
        .expect("insert challenger tool");

        record_capability_collision(
            &pool,
            &ExternalIdentifierCollision {
                kind: NamingKind::Tool,
                external_identifier: "a_b_c".to_string(),
                server_id: "server-challenger".to_string(),
                upstream_value: "c".to_string(),
                conflicting_server_id: "server-owner".to_string(),
                conflicting_upstream_value: "b_c".to_string(),
            },
        )
        .await
        .expect("record capability collision");

        let issue = inspect_namespace_issue(&pool, "server-challenger")
            .await
            .expect("inspect namespace issue")
            .expect("capability collision issue");
        assert_eq!(issue.current_namespace, "a_b");
        assert_eq!(issue.conflicts[0].server_id, "server-owner");

        let exposure_error = ensure_canonical_namespace_before_exposure(&pool, "server-challenger")
            .await
            .expect_err("collision must block only the challenger");
        assert!(exposure_error.to_string().contains("a_b_c"));

        let outcome = remediate_namespace(&pool, "server-challenger", "challenger")
            .await
            .expect("namespace remediation should resolve the collision");
        assert!(matches!(outcome, NamespaceRepairOutcome::Repaired(_)));
        let (upstream_name, external_name): (String, String) =
            sqlx::query_as("SELECT tool_name, unique_name FROM server_tools WHERE id = 'challenger-tool'")
                .fetch_one(&pool)
                .await
                .expect("load remediated capability");
        assert_eq!(upstream_name, "c");
        assert_eq!(external_name, "challenger_c");
        assert!(
            inspect_namespace_issue(&pool, "server-challenger")
                .await
                .expect("inspect repaired namespace")
                .is_none()
        );
    }

    #[tokio::test]
    async fn typed_sync_error_records_capability_collision_for_board_remediation() {
        let pool = test_pool().await;
        insert_server(&pool, "server-owner", "a").await;
        insert_server(&pool, "server-challenger", "a_b").await;
        let mut events = crate::core::events::EventBus::global().subscribe_async();
        let error = anyhow::Error::new(ExternalIdentifierCollision {
            kind: NamingKind::Tool,
            external_identifier: "a_b_c".to_string(),
            server_id: "server-challenger".to_string(),
            upstream_value: "c".to_string(),
            conflicting_server_id: "server-owner".to_string(),
            conflicting_upstream_value: "b_c".to_string(),
        })
        .context("background capability sync failed");

        let collision = record_capability_collision_from_error(&pool, &error)
            .await
            .expect("record typed collision")
            .expect("typed collision should be returned");

        assert_eq!(collision.server_id, "server-challenger");
        let issue = inspect_namespace_issue(&pool, "server-challenger")
            .await
            .expect("inspect namespace issue")
            .expect("persisted collision issue");
        assert_eq!(issue.kind, super::NamespaceIssueKind::CapabilityCollision);
        assert_eq!(issue.external_identifier.as_deref(), Some("a_b_c"));

        let event = tokio::time::timeout(std::time::Duration::from_millis(100), events.recv())
            .await
            .expect("recording a collision must publish a pool-block event")
            .expect("collision event channel must remain open");
        assert!(matches!(
            event,
            crate::core::events::Event::CapabilityCollisionDetected {
                server_id,
                conflicting_server_id,
                external_identifier,
            } if server_id == "server-challenger"
                && conflicting_server_id == "server-owner"
                && external_identifier == "a_b_c"
        ));
    }

    #[tokio::test]
    async fn manual_remediation_reuses_the_atomic_namespace_repair_flow() {
        let pool = test_pool().await;
        insert_legacy_fixture(&pool).await;
        insert_server(&pool, "server-owner", "sequential_thinking").await;

        let conflict = remediate_namespace(&pool, "server-legacy", "sequential_thinking")
            .await
            .expect_err("conflicting remediation must fail");
        assert!(conflict.to_string().contains("server-owner"));

        let outcome = remediate_namespace(&pool, "server-legacy", "sequential_reasoning")
            .await
            .expect("remediate legacy namespace");
        assert!(matches!(outcome, NamespaceRepairOutcome::Repaired(_)));
        let namespace: String = sqlx::query_scalar("SELECT name FROM server_config WHERE id = 'server-legacy'")
            .fetch_one(&pool)
            .await
            .expect("load remediated namespace");
        assert_eq!(namespace, "sequential_reasoning");
    }

    #[tokio::test]
    async fn authoritative_inventory_collision_rolls_back_namespace_remediation() {
        let pool = test_pool().await;
        insert_server(&pool, "server-owner", "a").await;
        insert_server(&pool, "server-challenger", "A B").await;
        sqlx::query(
            "INSERT INTO server_tools (id, server_id, server_name, tool_name, unique_name) VALUES ('owner-tool', 'server-owner', 'a', 'b_c', 'a_b_c')",
        )
        .execute(&pool)
        .await
        .expect("insert owner tool");
        let mut snapshot = crate::config::server::capabilities::CapabilitySnapshot {
            tools: vec![crate::core::capability::index::CachedToolInfo {
                name: "c".to_string(),
                description: None,
                input_schema_json: r#"{"type":"object"}"#.to_string(),
                output_schema_json: None,
                unique_name: None,
                icons: None,
                enabled: true,
                cached_at: chrono::Utc::now(),
            }],
            ..Default::default()
        };

        let error = remediate_namespace_with_snapshot(&pool, "server-challenger", "a_b", &mut snapshot)
            .await
            .expect_err("live capability collision must fail before namespace commit");

        assert!(error.downcast_ref::<ExternalIdentifierCollision>().is_some());
        let namespace: String = sqlx::query_scalar("SELECT name FROM server_config WHERE id = 'server-challenger'")
            .fetch_one(&pool)
            .await
            .expect("load unchanged challenger namespace");
        assert_eq!(namespace, "A B");
        let challenger_tools: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM server_tools WHERE server_id = 'server-challenger'")
                .fetch_one(&pool)
                .await
                .expect("count challenger tools");
        assert_eq!(challenger_tools, 0);
    }

    #[tokio::test]
    async fn first_legacy_repair_becomes_owner_and_blocks_only_the_challenger() {
        let pool = test_pool().await;
        insert_server(&pool, "server-a", "Sequential Thinking").await;
        insert_server(&pool, "server-b", "Sequential-Thinking").await;

        let owner = ensure_canonical_namespace_before_exposure(&pool, "server-a")
            .await
            .expect("first repair should claim the canonical namespace");
        assert!(matches!(owner, NamespaceRepairOutcome::Repaired(_)));
        let error = ensure_canonical_namespace_before_exposure(&pool, "server-b")
            .await
            .expect_err("canonical owner must block only the later challenger");
        assert!(error.to_string().contains("sequential_thinking"));

        let namespaces = sqlx::query_scalar::<_, String>("SELECT name FROM server_config ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("load owner and challenger namespaces");
        assert_eq!(namespaces, ["sequential_thinking", "Sequential-Thinking"]);
    }

    #[tokio::test]
    async fn repairs_legacy_namespace_and_preserves_relationships() {
        let pool = test_pool().await;
        insert_legacy_fixture(&pool).await;

        let outcome = ensure_canonical_namespace_before_exposure(&pool, "server-legacy")
            .await
            .expect("repair legacy namespace");

        assert_eq!(
            outcome,
            NamespaceRepairOutcome::Repaired(super::NamespaceRepairSummary {
                server_id: "server-legacy".to_string(),
                old_namespace: "Sequential Thinking".to_string(),
                new_namespace: "sequential_thinking".to_string(),
                identifier_changes: 4,
            })
        );

        let namespace: String = sqlx::query_scalar("SELECT name FROM server_config WHERE id = 'server-legacy'")
            .fetch_one(&pool)
            .await
            .expect("load repaired namespace");
        assert_eq!(namespace, "sequential_thinking");

        for table in [
            "server_args",
            "server_env",
            "server_meta",
            "server_tools",
            "server_prompts",
            "server_resources",
            "server_resource_templates",
            "server_issued_resources",
        ] {
            let query = format!("SELECT server_name FROM {table} WHERE server_id = 'server-legacy'");
            let values = sqlx::query_scalar::<_, String>(&query)
                .fetch_all(&pool)
                .await
                .expect("load repaired denormalized namespace");
            assert!(values.iter().all(|value| value == "sequential_thinking"), "{table}");
        }

        let (issued_upstream, issued_external): (String, String) =
            sqlx::query_as("SELECT resource_uri, unique_uri FROM server_issued_resources WHERE id = 'issued-1'")
                .fetch_one(&pool)
                .await
                .expect("load repaired issued resource");
        assert_eq!(issued_upstream, "file:///guide.md");
        assert_eq!(
            issued_external,
            crate::core::capability::resource_uri::encode_resource_uri("sequential_thinking", "file:///guide.md",)
                .expect("encode repaired issued resource")
        );
        let listed_external: String = sqlx::query_scalar(
            "SELECT unique_uri FROM server_resources WHERE server_id = 'server-legacy' AND resource_uri = 'file:///guide.md'",
        )
        .fetch_one(&pool)
        .await
        .expect("load repaired overlapping listed resource");
        assert_eq!(
            listed_external, issued_external,
            "overlapping listed and issued rows must share one canonical URI"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM server_issued_resources WHERE id = 'issued-1'",)
                .fetch_one(&pool)
                .await
                .expect("count repaired overlapping issued resource"),
            1,
            "namespace repair must retain the overlapping issued row"
        );

        let capability_rows = [
            (
                "server_tools",
                "tool_name",
                "unique_name",
                "get_sequential_thinking_status".to_string(),
                "sequential_thinking_get_status".to_string(),
            ),
            (
                "server_prompts",
                "prompt_name",
                "unique_name",
                "sequential_thinking_help".to_string(),
                "sequential_thinking_help".to_string(),
            ),
            (
                "server_resources",
                "resource_uri",
                "unique_uri",
                "file:///guide.md".to_string(),
                crate::core::capability::resource_uri::encode_resource_uri("sequential_thinking", "file:///guide.md")
                    .expect("encode repaired resource"),
            ),
            (
                "server_resource_templates",
                "uri_template",
                "unique_name",
                "demo://resource/lookup/{id}".to_string(),
                crate::core::capability::resource_uri::encode_resource_template(
                    "sequential_thinking",
                    "demo://resource/lookup/{id}",
                )
                .expect("encode repaired template"),
            ),
        ];
        for (table, upstream_column, external_column, upstream, external) in capability_rows {
            let query = format!(
                "SELECT id, {upstream_column}, {external_column} FROM {table} WHERE server_id = 'server-legacy'"
            );
            let (id, actual_upstream, actual_external): (String, String, String) = sqlx::query_as(&query)
                .fetch_one(&pool)
                .await
                .expect("load repaired capability");
            assert!(id.ends_with("-1"));
            assert_eq!(actual_upstream, upstream);
            assert_eq!(actual_external, external);
        }

        let profile_policy: String = sqlx::query_scalar(
            "SELECT new_ref_policy FROM profile_server_relationships WHERE profile_id = 'profile-1' AND server_id = 'server-legacy'",
        )
        .fetch_one(&pool)
        .await
        .expect("load profile server policy");
        assert_eq!(profile_policy, "review");

        let direct_policy: String =
            sqlx::query_scalar(
                "SELECT new_ref_policy FROM direct_exposure_servers WHERE consumer_id = 'test-client' AND server_id = 'server-legacy'",
            )
                .fetch_one(&pool)
                .await
                .expect("load direct exposure server policy");
        assert_eq!(direct_policy, "follow");

        let second = ensure_canonical_namespace_before_exposure(&pool, "server-legacy")
            .await
            .expect("repaired namespace should be idempotent");
        assert_eq!(
            second,
            NamespaceRepairOutcome::Noop {
                namespace: "sequential_thinking".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn unrelated_uncanonicalizable_server_does_not_block_repair() {
        let pool = test_pool().await;
        insert_legacy_fixture(&pool).await;
        insert_server(&pool, "server-blocked", "序列思考").await;
        sqlx::query(
            "INSERT INTO server_tools (id, server_id, server_name, tool_name, unique_name) VALUES ('blocked-tool', 'server-blocked', '序列思考', 'think', 'legacy_blocked_think')",
        )
        .execute(&pool)
        .await
        .expect("insert blocked server capability");

        ensure_canonical_namespace_before_exposure(&pool, "server-legacy")
            .await
            .expect("unrelated invalid namespace must not block a repairable server");

        let blocked: (String, String) =
            sqlx::query_as("SELECT server_name, unique_name FROM server_tools WHERE id = 'blocked-tool'")
                .fetch_one(&pool)
                .await
                .expect("load untouched blocked capability");
        assert_eq!(blocked, ("序列思考".to_string(), "legacy_blocked_think".to_string()));
    }

    #[tokio::test]
    async fn repair_failure_rolls_back_every_current_state_change() {
        let pool = test_pool().await;
        insert_legacy_fixture(&pool).await;
        sqlx::query(
            r#"
            CREATE TRIGGER fail_namespace_repair
            BEFORE UPDATE ON server_tools
            BEGIN
                SELECT RAISE(ABORT, 'forced namespace repair failure');
            END
            "#,
        )
        .execute(&pool)
        .await
        .expect("create failure trigger");

        ensure_canonical_namespace_before_exposure(&pool, "server-legacy")
            .await
            .expect_err("forced failure must abort repair");

        let namespace: String = sqlx::query_scalar("SELECT name FROM server_config WHERE id = 'server-legacy'")
            .fetch_one(&pool)
            .await
            .expect("load rolled back namespace");
        assert_eq!(namespace, "Sequential Thinking");
        let (server_name, unique_name): (String, String) =
            sqlx::query_as("SELECT server_name, unique_name FROM server_tools WHERE id = 'tool-1'")
                .fetch_one(&pool)
                .await
                .expect("load rolled back tool");
        assert_eq!(server_name, "Sequential Thinking");
        assert_eq!(unique_name, "old_tool");
        let direct_policy: String = sqlx::query_scalar(
            "SELECT new_ref_policy FROM direct_exposure_servers WHERE consumer_id = 'test-client' AND server_id = 'server-legacy'",
        )
        .fetch_one(&pool)
        .await
        .expect("load rolled back direct exposure policy");
        assert_eq!(direct_policy, "follow");
    }
}
