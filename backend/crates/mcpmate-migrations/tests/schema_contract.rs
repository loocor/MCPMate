#[path = "support/memory.rs"]
mod memory_support;

use mcpmate_migrations::{DatabaseSource, prepare_audit_database, prepare_config_database};
use sha2::{Digest, Sha256};

async fn table_exists(
    pool: &sqlx::SqlitePool,
    table: &str,
) -> bool {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?)")
        .bind(table)
        .fetch_one(pool)
        .await
        .expect("inspect SQLite table")
}

async fn table_columns(
    pool: &sqlx::SqlitePool,
    table: &str,
) -> Vec<String> {
    sqlx::query_scalar(&format!("SELECT name FROM pragma_table_info('{table}')"))
        .fetch_all(pool)
        .await
        .expect("inspect SQLite columns")
}

async fn rewind_config_to_version(
    pool: &sqlx::SqlitePool,
    version: i64,
) {
    sqlx::query("DELETE FROM mcpmate_schema_migrations WHERE target = 'config' AND version > ?")
        .bind(version)
        .execute(pool)
        .await
        .expect("rewind config migration ledger");
    sqlx::query(
        "UPDATE mcpmate_schema_migration_state
         SET version = ?,
             checksum = (
                 SELECT checksum FROM mcpmate_schema_migrations
                 WHERE target = 'config' AND version = ?
             )
         WHERE target = 'config'",
    )
    .bind(version)
    .bind(version)
    .execute(pool)
    .await
    .expect("rewind config migration state");
}

#[tokio::test]
async fn fresh_config_schema_contains_profile_authoring_generation() {
    let pool = memory_support::pool().await;
    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("prepare fresh config database");

    let column: (String, i64, Option<String>) = sqlx::query_as(
        "SELECT name, [notnull], dflt_value
         FROM pragma_table_info('profile')
         WHERE name = 'authoring_generation'",
    )
    .fetch_one(&pool)
    .await
    .expect("load Profile authoring generation column");

    assert_eq!(column, ("authoring_generation".into(), 1, Some("0".into())));
}

#[tokio::test]
async fn upgrades_workflow_steps_with_standard_uuid_ids() {
    let pool = memory_support::pool().await;
    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("prepare current config database");
    sqlx::raw_sql(
        "DROP TABLE workflow_profile_external_guides;
         DROP TABLE workflow_profile_skill_projections;
         DROP TABLE workflow_profile_package_files;
         DROP TABLE workflow_profile_guides;
         DROP TABLE workflow_profile_step_materials;
         DROP TABLE workflow_profile_materials;
         DROP TABLE workflow_profile_material_libraries;
         DROP TABLE workflow_profile_skills;
         DROP TRIGGER validate_workflow_profile_step_id_insert;
         DROP TRIGGER validate_workflow_profile_step_id_update;
         DROP INDEX idx_workflow_profile_steps_step_id;
         ALTER TABLE workflow_profile_steps DROP COLUMN step_id;",
    )
    .execute(&pool)
    .await
    .expect("restore the version fourteen workflow step schema");
    rewind_config_to_version(&pool, 14).await;

    sqlx::query(
        "INSERT INTO profile (id, name, description, type, role, profile_mode)
         VALUES ('workflow-profile', 'Workflow Profile', '', 'shared', 'user', 'workflow')",
    )
    .execute(&pool)
    .await
    .expect("insert Workflow Profile");
    sqlx::query(
        "INSERT INTO workflow_profile_specifications (profile_id)
         VALUES ('workflow-profile')",
    )
    .execute(&pool)
    .await
    .expect("insert Workflow specification");
    sqlx::query(
        "INSERT INTO workflow_profile_steps (profile_id, step_index, title)
         VALUES ('workflow-profile', 0, 'Existing step')",
    )
    .execute(&pool)
    .await
    .expect("insert pre-material Workflow step");

    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("upgrade Workflow step storage");

    let step_id: String = sqlx::query_scalar(
        "SELECT step_id FROM workflow_profile_steps
         WHERE profile_id = 'workflow-profile' AND step_index = 0",
    )
    .fetch_one(&pool)
    .await
    .expect("load upgraded Workflow step ID");
    assert_eq!(step_id.len(), 36);
    assert_eq!(
        [8, 13, 18, 23]
            .into_iter()
            .map(|index| step_id.as_bytes()[index])
            .collect::<Vec<_>>(),
        vec![b'-'; 4]
    );
    assert!(
        step_id
            .chars()
            .all(|character| character == '-' || character.is_ascii_hexdigit())
    );
}

#[tokio::test]
async fn creates_guide_schema_without_copying_existing_workflow_authoring() {
    let pool = memory_support::pool().await;
    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("prepare current config database");
    sqlx::raw_sql(
        "DROP TABLE workflow_profile_skill_projections;
         DROP TABLE workflow_profile_external_guides;
         DROP TABLE workflow_profile_package_files;
         DROP TABLE workflow_profile_guides;",
    )
    .execute(&pool)
    .await
    .expect("restore the version fifteen Workflow schema");
    rewind_config_to_version(&pool, 15).await;

    sqlx::query(
        "INSERT INTO profile (id, name, description, type, role, profile_mode)
         VALUES ('workflow-profile', 'Release investigation', '', 'shared', 'user', 'workflow')",
    )
    .execute(&pool)
    .await
    .expect("insert Workflow Profile");
    sqlx::query("INSERT INTO workflow_profile_specifications (profile_id) VALUES ('workflow-profile')")
        .execute(&pool)
        .await
        .expect("insert Workflow specification");
    sqlx::query(
        "INSERT INTO workflow_profile_steps (profile_id, step_index, step_id, title, description)
         VALUES ('workflow-profile', 0, '550e8400-e29b-41d4-a716-446655440000', 'Collect evidence', 'Read the logs first.')",
    )
    .execute(&pool)
    .await
    .expect("insert legacy Workflow step");
    sqlx::query(
        "INSERT INTO workflow_profile_materials (
            material_id, profile_id, ordinal, title, kind, external_url
         ) VALUES ('660e8400-e29b-41d4-a716-446655440000', 'workflow-profile', 0, 'Release notes', 'external_url', 'https://example.com/release-notes')",
    )
    .execute(&pool)
    .await
    .expect("insert legacy URL Material");
    sqlx::query(
        "INSERT INTO workflow_profile_step_materials (profile_id, step_id, material_id, ordinal)
         VALUES (
            'workflow-profile', '550e8400-e29b-41d4-a716-446655440000',
            '660e8400-e29b-41d4-a716-446655440000', 0
         )",
    )
    .execute(&pool)
    .await
    .expect("associate legacy URL Material with Workflow step");

    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("create Workflow Guide schema");

    for table in [
        "workflow_profile_guides",
        "workflow_profile_package_files",
        "workflow_profile_external_guides",
        "workflow_profile_skill_projections",
    ] {
        let row_count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .expect("count newly created Guide records");
        assert_eq!(row_count, 0, "v0016 must not copy legacy data into {table}");
    }

    let material_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM workflow_profile_materials WHERE profile_id = 'workflow-profile'")
            .fetch_one(&pool)
            .await
            .expect("retain existing Materials rows");
    assert_eq!(material_count, 1);
}

#[tokio::test]
async fn external_guide_foreign_keys_preserve_profile_ownership() {
    let pool = memory_support::pool().await;
    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("prepare current config database");

    for profile_id in ["profile-a", "profile-b"] {
        sqlx::query(
            "INSERT INTO profile (id, name, description, type, role, profile_mode)
             VALUES (?, ?, '', 'shared', 'user', 'workflow')",
        )
        .bind(profile_id)
        .bind(profile_id)
        .execute(&pool)
        .await
        .expect("insert Workflow Profile");
    }
    sqlx::query(
        "INSERT INTO workflow_profile_package_files (
            package_file_id, profile_id, ordinal, title, category, relative_path
         ) VALUES ('package-a', 'profile-a', 0, 'Reference', 'reference', 'references/reference.md')",
    )
    .execute(&pool)
    .await
    .expect("insert package file for Profile A");

    let external_error = sqlx::query(
        "INSERT INTO workflow_profile_external_guides (package_file_id, profile_id, markdown)
         VALUES ('package-a', 'profile-b', '# Invalid')",
    )
    .execute(&pool)
    .await
    .expect_err("an external guide cannot claim another Profile's package file");
    assert!(external_error.to_string().contains("FOREIGN KEY constraint failed"));
}

#[tokio::test]
async fn rejects_incomplete_existing_workflow_guide_storage() {
    let pool = memory_support::pool().await;
    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("prepare current config database");
    sqlx::raw_sql(
        "DROP TABLE workflow_profile_guides;
         CREATE TABLE workflow_profile_guides (profile_id TEXT PRIMARY KEY);",
    )
    .execute(&pool)
    .await
    .expect("replace Guide storage with an incomplete table");
    rewind_config_to_version(&pool, 15).await;

    let error = prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect_err("incomplete Workflow Guide storage must be rejected");
    assert!(error.to_string().contains("does not match the versioned contract"));
}

#[tokio::test]
async fn rejects_incomplete_existing_workflow_materials_storage() {
    let pool = memory_support::pool().await;
    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("prepare current config database");
    sqlx::query("DROP INDEX idx_workflow_profile_materials_ordinal")
        .execute(&pool)
        .await
        .expect("remove a required Workflow Materials index");
    rewind_config_to_version(&pool, 14).await;

    let error = prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect_err("incomplete Workflow Materials storage must be rejected");
    assert!(error.to_string().contains("does not match the versioned contract"));
}

#[tokio::test]
async fn creates_config_and_audit_schema_through_independent_ledgers() {
    let config = memory_support::pool().await;
    prepare_config_database(&config, DatabaseSource::InMemory)
        .await
        .expect("prepare config database");
    for table in [
        "llm_provider",
        "server_config",
        "client",
        "secure_store_secrets",
        "profile",
        "profile_server_relationships",
        "workflow_profile_specifications",
        "workflow_profile_steps",
        "workflow_profile_step_bindings",
        "profile_capability_refs",
        "direct_exposure_refs",
        "direct_exposure_servers",
        "capability_server_snapshots",
        "capability_refs",
        "surface_manifests",
    ] {
        assert!(table_exists(&config, table).await, "missing config table {table}");
    }
    let workflow_columns = table_columns(&config, "workflow_profile_specifications").await;
    for column in ["validation_notes", "avoid_rules"] {
        assert!(
            workflow_columns.contains(&column.to_string()),
            "missing Workflow specification guidance column {column}"
        );
    }
    for legacy in [
        "profile_tool",
        "profile_prompt",
        "profile_resource",
        "profile_resource_template",
    ] {
        assert!(!table_exists(&config, legacy).await, "unexpected legacy table {legacy}");
    }

    let audit = memory_support::pool().await;
    prepare_audit_database(&audit, DatabaseSource::InMemory)
        .await
        .expect("prepare audit database");
    assert!(table_exists(&audit, "audit_events").await);
    assert!(table_exists(&audit, "audit_policy").await);
    let audit_versions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM mcpmate_schema_migrations WHERE target = 'audit'")
            .fetch_one(&audit)
            .await
            .expect("count audit migrations");
    assert_eq!(audit_versions, 1);
}

#[tokio::test]
async fn creates_one_tagged_transport_draft_per_server() {
    let pool = memory_support::pool().await;
    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("prepare fresh config database");

    assert!(table_exists(&pool, "server_transport").await);
    let columns = table_columns(&pool, "server_transport").await;
    for column in ["server_id", "draft_json", "created_at", "updated_at"] {
        assert!(columns.contains(&column.to_string()), "missing {column} column");
    }

    sqlx::query("INSERT INTO server_config (id, name, server_type) VALUES ('server-a', 'A', 'stdio')")
        .execute(&pool)
        .await
        .expect("insert server identity");
    sqlx::query("INSERT INTO server_transport (server_id, draft_json) VALUES ('server-a', ?)")
        .bind(r#"{"kind":"stdio","command":"echo","args":[],"env":{}}"#)
        .execute(&pool)
        .await
        .expect("insert transport draft");

    let duplicate = sqlx::query("INSERT INTO server_transport (server_id, draft_json) VALUES ('server-a', ?)")
        .bind(r#"{"kind":"stdio","command":"other","args":[],"env":{}}"#)
        .execute(&pool)
        .await;
    assert!(duplicate.is_err(), "a server may own only one transport draft");

    sqlx::query("INSERT INTO server_config (id, name, server_type) VALUES ('server-b', 'B', 'stdio')")
        .execute(&pool)
        .await
        .expect("insert second server identity");
    let missing_kind = sqlx::query("INSERT INTO server_transport (server_id, draft_json) VALUES ('server-b', '{}')")
        .execute(&pool)
        .await;
    assert!(missing_kind.is_err(), "transport draft must declare a supported kind");

    sqlx::query("DELETE FROM server_config WHERE id = 'server-a'")
        .execute(&pool)
        .await
        .expect("delete server identity");
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM server_transport WHERE server_id = 'server-a'")
        .fetch_one(&pool)
        .await
        .expect("count transport drafts");
    assert_eq!(remaining, 0, "transport draft must not outlive its server");
}

#[tokio::test]
async fn upgrades_legacy_server_into_a_draft_and_redacted_audit() {
    let pool = memory_support::pool().await;
    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("prepare v12 config database");

    rewind_config_to_version(&pool, 11).await;
    sqlx::query("DROP TABLE server_config_migration_audit")
        .execute(&pool)
        .await
        .expect("remove v12 audit table");
    sqlx::query("DROP TABLE server_transport")
        .execute(&pool)
        .await
        .expect("remove v12 transport table");

    sqlx::query(
        "INSERT INTO server_config (id, name, server_type, command, url)
         VALUES ('legacy-stdio', 'Legacy stdio', 'stdio', NULL, 'https://ignored.example/mcp')",
    )
    .execute(&pool)
    .await
    .expect("insert legacy server");
    sqlx::query(
        "INSERT INTO server_args (id, server_id, server_name, arg_index, arg_value)
         VALUES ('arg-1', 'legacy-stdio', 'Legacy stdio', 0, '--flag')",
    )
    .execute(&pool)
    .await
    .expect("insert legacy argument");
    sqlx::query(
        "INSERT INTO server_env (id, server_id, server_name, env_key, env_value)
         VALUES ('env-1', 'legacy-stdio', 'Legacy stdio', 'TOKEN', '[[secret:token]]')",
    )
    .execute(&pool)
    .await
    .expect("insert secret reference");
    sqlx::query(
        "INSERT INTO server_headers (id, server_id, header_key, header_value)
         VALUES ('header-1', 'legacy-stdio', 'authorization', 'Bearer should-not-leak')",
    )
    .execute(&pool)
    .await
    .expect("insert conflicting header");

    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("upgrade v11 server configuration");

    let draft: String = sqlx::query_scalar("SELECT draft_json FROM server_transport WHERE server_id = 'legacy-stdio'")
        .fetch_one(&pool)
        .await
        .expect("load structured transport draft");
    assert_eq!(
        draft,
        r#"{"args":["--flag"],"command":null,"env":{"TOKEN":{"alias":"token","kind":"secret_ref"}},"kind":"stdio"}"#,
    );

    let audit: (String, String, String) = sqlx::query_as(
        "SELECT original_shape_json, ignored_field_names_json, diagnostic_codes_json
         FROM server_config_migration_audit WHERE server_id = 'legacy-stdio'",
    )
    .fetch_one(&pool)
    .await
    .expect("load migration audit");
    assert_eq!(
        audit.0,
        r#"{"arg_count":1,"command_present":false,"env_keys":["TOKEN"],"header_keys":["authorization"],"server_type":"stdio","url_present":true}"#,
    );
    assert_eq!(audit.1, r#"["headers","url"]"#);
    assert_eq!(audit.2, r#"["stdio_command_missing","transport_field_conflict"]"#);
    assert!(!audit.0.contains("should-not-leak"));
}

#[tokio::test]
async fn audits_a_legacy_http_endpoint_that_cannot_reach_runtime() {
    let pool = memory_support::pool().await;
    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("prepare v12 config database");
    rewind_config_to_version(&pool, 11).await;
    sqlx::query("DROP TABLE server_config_migration_audit")
        .execute(&pool)
        .await
        .expect("remove v12 audit table");
    sqlx::query("DROP TABLE server_transport")
        .execute(&pool)
        .await
        .expect("remove v12 transport table");
    sqlx::query(
        "INSERT INTO server_config (id, name, server_type, url)
         VALUES ('legacy-http', 'Legacy HTTP', 'streamable_http', 'ftp://invalid.example/mcp')",
    )
    .execute(&pool)
    .await
    .expect("insert legacy HTTP server");

    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("upgrade v11 server configuration");

    let diagnostics: String = sqlx::query_scalar(
        "SELECT diagnostic_codes_json FROM server_config_migration_audit WHERE server_id = 'legacy-http'",
    )
    .fetch_one(&pool)
    .await
    .expect("load invalid URL audit");
    assert_eq!(diagnostics, r#"["url_invalid"]"#);
}

#[tokio::test]
async fn canonicalizes_audited_unrecognized_transport_projection() {
    let pool = memory_support::pool().await;
    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("prepare v13 config database");
    rewind_config_to_version(&pool, 12).await;

    sqlx::query("PRAGMA ignore_check_constraints = ON")
        .execute(&pool)
        .await
        .expect("allow historical unknown transport projection");
    sqlx::query(
        "INSERT INTO server_config (id, name, server_type, command, url)
         VALUES ('legacy-websocket', 'Legacy WebSocket', 'websocket', 'old-command', 'wss://legacy.example/mcp')",
    )
    .execute(&pool)
    .await
    .expect("insert unknown historical transport projection");
    sqlx::query("PRAGMA ignore_check_constraints = OFF")
        .execute(&pool)
        .await
        .expect("restore transport projection constraint checks");

    let draft = r#"{"kind":"unrecognized","declared_type":"websocket"}"#;
    let audit = r#"{"server_type":"websocket","command_present":true,"url_present":true,"arg_count":0,"env_keys":[],"header_keys":[]}"#;
    sqlx::query("INSERT INTO server_transport (server_id, draft_json) VALUES ('legacy-websocket', ?)")
        .bind(draft)
        .execute(&pool)
        .await
        .expect("insert unrecognized transport draft");
    sqlx::query(
        "INSERT INTO server_config_migration_audit (
            server_id, original_shape_json, ignored_field_names_json, diagnostic_codes_json
         ) VALUES ('legacy-websocket', ?, '[]', '[\"transport_unrecognized\"]')",
    )
    .bind(audit)
    .execute(&pool)
    .await
    .expect("insert v12 migration audit");

    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("canonicalize historical unknown transport projection");

    let projection: (String, String, String, String) = sqlx::query_as(
        "SELECT server_config.server_type, server_config.command, server_config.url,
                server_transport.draft_json
         FROM server_config
         JOIN server_transport ON server_transport.server_id = server_config.id
         WHERE server_config.id = 'legacy-websocket'",
    )
    .fetch_one(&pool)
    .await
    .expect("load canonicalized transport projection");
    assert_eq!(projection.0, "stdio");
    assert_eq!(projection.1, "old-command");
    assert_eq!(projection.2, "wss://legacy.example/mcp");
    assert_eq!(projection.3, draft);
    let preserved_audit: String = sqlx::query_scalar(
        "SELECT original_shape_json FROM server_config_migration_audit WHERE server_id = 'legacy-websocket'",
    )
    .fetch_one(&pool)
    .await
    .expect("load preserved v12 migration audit");
    assert_eq!(preserved_audit, audit);
}

#[tokio::test]
async fn rejects_unrecognized_projection_without_matching_audit() {
    let pool = memory_support::pool().await;
    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("prepare v13 config database");
    rewind_config_to_version(&pool, 12).await;

    sqlx::query("PRAGMA ignore_check_constraints = ON")
        .execute(&pool)
        .await
        .expect("allow historical unknown transport projection");
    sqlx::query("INSERT INTO server_config (id, name, server_type) VALUES ('unknown', 'Unknown', 'websocket')")
        .execute(&pool)
        .await
        .expect("insert unknown historical transport projection");
    sqlx::query("PRAGMA ignore_check_constraints = OFF")
        .execute(&pool)
        .await
        .expect("restore transport projection constraint checks");
    sqlx::query("INSERT INTO server_transport (server_id, draft_json) VALUES ('unknown', ?)")
        .bind(r#"{"kind":"unrecognized","declared_type":"websocket"}"#)
        .execute(&pool)
        .await
        .expect("insert unrecognized transport draft");
    sqlx::query(
        "INSERT INTO server_config_migration_audit (
            server_id, original_shape_json, ignored_field_names_json, diagnostic_codes_json
         ) VALUES ('unknown', '{\"server_type\":\"websocket\"}', '[]', '[]')",
    )
    .execute(&pool)
    .await
    .expect("insert incompatible v12 migration audit");

    let error = prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect_err("unknown projection with incompatible v12 audit must fail closed");
    assert!(error.to_string().contains("incompatible migration audit"));
    let projection: String = sqlx::query_scalar("SELECT server_type FROM server_config WHERE id = 'unknown'")
        .fetch_one(&pool)
        .await
        .expect("load unmodified transport projection");
    assert_eq!(projection, "websocket");
    let recorded: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM mcpmate_schema_migrations WHERE target = 'config' AND version = 13")
            .fetch_one(&pool)
            .await
            .expect("inspect rolled-back v13 ledger record");
    assert_eq!(recorded, 0);
}

#[tokio::test]
async fn creates_resource_registry_routes_and_indexes() {
    let pool = memory_support::pool().await;
    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("prepare config database");

    let template_columns = table_columns(&pool, "server_resource_templates").await;
    assert!(template_columns.iter().any(|column| column == "route_uri"));
    let issued_columns = table_columns(&pool, "server_issued_resources").await;
    for column in [
        "id",
        "server_id",
        "server_name",
        "resource_uri",
        "unique_uri",
        "created_at",
        "last_seen_at",
    ] {
        assert!(issued_columns.iter().any(|existing| existing == column));
    }
    let indexes: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_index_list('server_issued_resources')")
        .fetch_all(&pool)
        .await
        .expect("inspect issued-resource indexes");
    for index in [
        "idx_server_issued_resources_lookup",
        "idx_server_issued_resources_unique_uri",
    ] {
        assert!(indexes.iter().any(|existing| existing == index));
    }
}

#[tokio::test]
async fn upgrades_legacy_llm_server_and_client_fields() {
    let pool = memory_support::pool().await;
    sqlx::query("CREATE TABLE llm_provider (id TEXT PRIMARY KEY, name TEXT NOT NULL, provider_type TEXT NOT NULL, base_url TEXT NOT NULL, model_id TEXT NOT NULL, secret_alias TEXT, default_params_json TEXT, created_at TIMESTAMP NOT NULL, updated_at TIMESTAMP NOT NULL)")
        .execute(&pool)
        .await
        .expect("create legacy LLM provider table");
    sqlx::query(
        "CREATE TABLE server_meta (
            id TEXT PRIMARY KEY,
            server_id TEXT NOT NULL UNIQUE,
            server_name TEXT NOT NULL,
            registry_version TEXT,
            registry_meta_json TEXT,
            extras_json TEXT
        )",
    )
    .execute(&pool)
    .await
    .expect("create legacy server metadata table");
    sqlx::query(
        "CREATE TABLE client (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            identifier TEXT NOT NULL UNIQUE,
            config_path TEXT,
            config_mode TEXT,
            transport TEXT NOT NULL DEFAULT 'auto',
            backup_policy TEXT NOT NULL DEFAULT 'keep_n',
            backup_limit INTEGER DEFAULT 5,
            connection_mode TEXT NOT NULL DEFAULT 'manual',
            registration_origin TEXT NOT NULL DEFAULT 'manual',
            runtime_observed INTEGER NOT NULL DEFAULT 0,
            format_rules TEXT,
            created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .execute(&pool)
    .await
    .expect("create legacy client table");
    sqlx::query(
        "INSERT INTO client (id, name, identifier, connection_mode)
         VALUES ('remote', 'Remote', 'remote', 'remote_http')",
    )
    .execute(&pool)
    .await
    .expect("insert remote legacy client");
    let format_rules = r#"{"stdio":{"command_field":"command"}}"#;
    sqlx::query(
        "INSERT INTO client (
            id, name, identifier, config_path, connection_mode, format_rules
         ) VALUES ('local', 'Local', 'local', '/tmp/client.json', 'manual', ?)",
    )
    .bind(format_rules)
    .execute(&pool)
    .await
    .expect("insert local legacy client");

    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("upgrade legacy config database");

    assert!(
        table_columns(&pool, "llm_provider")
            .await
            .contains(&"is_default".into())
    );
    let server_columns = table_columns(&pool, "server_meta").await;
    assert!(server_columns.contains(&"upstream_name".into()));
    assert!(server_columns.contains(&"upstream_title".into()));
    let remote: (String, String, i64) = sqlx::query_as(
        "SELECT connection_mode, registration_origin, runtime_observed
         FROM client WHERE id = 'remote'",
    )
    .fetch_one(&pool)
    .await
    .expect("load normalized remote client");
    assert_eq!(remote, ("manual".into(), "runtime_initialize".into(), 1));
    let local: (String, String, String) = sqlx::query_as(
        "SELECT connection_mode, registration_origin, transports
         FROM client WHERE id = 'local'",
    )
    .fetch_one(&pool)
    .await
    .expect("load normalized local client");
    assert_eq!(
        local,
        (
            "local_config_detected".into(),
            "config_detection".into(),
            format_rules.into(),
        )
    );
}

#[tokio::test]
async fn capability_migration_checksum_covers_the_rust_artifact() {
    let pool = memory_support::pool().await;
    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("prepare config database");
    let recorded: String = sqlx::query_scalar(
        "SELECT checksum FROM mcpmate_schema_migrations
         WHERE target = 'config' AND version = 10",
    )
    .fetch_one(&pool)
    .await
    .expect("load capability migration checksum");
    let source = include_str!("../src/migrations/config/v0010_create_capability_catalog.rs");
    let mut digest = Sha256::new();
    digest.update((source.len() as u64).to_be_bytes());
    digest.update(source.as_bytes());

    assert_eq!(recorded, format!("{:x}", digest.finalize()));
}

#[tokio::test]
async fn rejects_unversioned_and_unknown_epoch_capability_schemas() {
    let unversioned = memory_support::pool().await;
    sqlx::query("CREATE TABLE capability_records (id TEXT PRIMARY KEY)")
        .execute(&unversioned)
        .await
        .expect("create unversioned capability table");
    let unversioned_error = prepare_config_database(&unversioned, DatabaseSource::InMemory)
        .await
        .expect_err("unversioned capability storage must be rejected");
    assert!(unversioned_error.to_string().contains("clean rebuild is required"));
    assert!(table_exists(&unversioned, "capability_records").await);
    assert!(!table_exists(&unversioned, "mcpmate_schema_migrations").await);

    let unknown_epoch = memory_support::pool().await;
    sqlx::query(
        "CREATE TABLE capability_schema_metadata (
            singleton INTEGER PRIMARY KEY,
            schema_epoch INTEGER NOT NULL
        )",
    )
    .execute(&unknown_epoch)
    .await
    .expect("create capability schema metadata");
    sqlx::query("INSERT INTO capability_schema_metadata (singleton, schema_epoch) VALUES (1, 999)")
        .execute(&unknown_epoch)
        .await
        .expect("insert unknown capability epoch");
    let epoch_error = prepare_config_database(&unknown_epoch, DatabaseSource::InMemory)
        .await
        .expect_err("unknown capability epoch must be rejected");
    assert!(epoch_error.to_string().contains("epoch 999 is not supported"));
    assert!(!table_exists(&unknown_epoch, "mcpmate_schema_migrations").await);
}

#[tokio::test]
async fn rejects_partial_or_unversioned_current_capability_storage() {
    let unversioned = memory_support::pool().await;
    sqlx::query("CREATE TABLE surface_reconciliation_jobs (job_id TEXT PRIMARY KEY)")
        .execute(&unversioned)
        .await
        .expect("create unversioned current capability table");
    let error = prepare_config_database(&unversioned, DatabaseSource::InMemory)
        .await
        .expect_err("unversioned current capability storage must be rejected");
    assert!(error.to_string().contains("clean rebuild is required"));
    assert!(!table_exists(&unversioned, "mcpmate_schema_migrations").await);

    let partial_epoch = memory_support::pool().await;
    sqlx::query(
        "CREATE TABLE capability_schema_metadata (
            singleton INTEGER PRIMARY KEY,
            schema_epoch INTEGER NOT NULL
        )",
    )
    .execute(&partial_epoch)
    .await
    .expect("create capability schema metadata");
    sqlx::query("INSERT INTO capability_schema_metadata (singleton, schema_epoch) VALUES (1, 4)")
        .execute(&partial_epoch)
        .await
        .expect("insert current capability epoch");
    let error = prepare_config_database(&partial_epoch, DatabaseSource::InMemory)
        .await
        .expect_err("incomplete current capability storage must be rejected");
    assert!(error.to_string().contains("incomplete capability schema epoch 4"));
    assert!(!table_exists(&partial_epoch, "mcpmate_schema_migrations").await);
}

async fn convert_prepared_database_to_epoch_four(pool: &sqlx::SqlitePool) {
    sqlx::query("ALTER TABLE profile DROP COLUMN authoring_generation")
        .execute(pool)
        .await
        .expect("restore the pre-v11 Profile shape");
    sqlx::query(
        "CREATE TABLE capability_schema_metadata (
            singleton INTEGER PRIMARY KEY,
            schema_epoch INTEGER NOT NULL
        )",
    )
    .execute(pool)
    .await
    .expect("create current capability metadata");
    sqlx::query("INSERT INTO capability_schema_metadata (singleton, schema_epoch) VALUES (1, 4)")
        .execute(pool)
        .await
        .expect("record current capability epoch");
    sqlx::query("DELETE FROM mcpmate_schema_migrations WHERE target = 'config'")
        .execute(pool)
        .await
        .expect("remove config ledger to model pre-ledger storage");
    sqlx::query("DELETE FROM mcpmate_schema_migration_state WHERE target = 'config'")
        .execute(pool)
        .await
        .expect("remove config ledger state to model pre-ledger storage");
}

#[tokio::test]
async fn adopts_complete_epoch_four_capability_storage_without_losing_data() {
    let pool = memory_support::pool().await;
    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("prepare current schema fixture");
    sqlx::query(
        "INSERT INTO capability_server_snapshots (
            server_id, server_name, config_fingerprint, record_format_version,
            catalog_revision, snapshot_state, initialize_payload, observed_at,
            committed_at
        ) VALUES (
            'server-a', 'Docs', 'fingerprint', 1, 7, 'ready', '{}',
            '2026-08-06T00:00:00Z', '2026-08-06T00:00:00Z'
        )",
    )
    .execute(&pool)
    .await
    .expect("insert current epoch catalog data");
    convert_prepared_database_to_epoch_four(&pool).await;

    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("adopt complete epoch-four catalog");

    let revision: i64 =
        sqlx::query_scalar("SELECT catalog_revision FROM capability_server_snapshots WHERE server_id = 'server-a'")
            .fetch_one(&pool)
            .await
            .expect("load preserved catalog data");
    assert_eq!(revision, 7);
    let version: i64 = sqlx::query_scalar(
        "SELECT version FROM mcpmate_schema_migrations WHERE target = 'config' ORDER BY version DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("load adopted migration version");
    assert_eq!(version, 17);
}

#[tokio::test]
async fn rejects_epoch_four_catalog_with_complete_table_names_but_corrupt_structure() {
    let pool = memory_support::pool().await;
    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("prepare current schema fixture");
    convert_prepared_database_to_epoch_four(&pool).await;
    sqlx::query("ALTER TABLE capability_refs RENAME TO capability_refs_valid")
        .execute(&pool)
        .await
        .expect("retain valid table under a non-contract name");
    sqlx::query("CREATE TABLE capability_refs (ref_id TEXT PRIMARY KEY)")
        .execute(&pool)
        .await
        .expect("create corrupt current table");

    let error = prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect_err("corrupt epoch-four catalog must be rejected");
    assert!(error.to_string().contains("does not match the versioned contract"));
    let recorded: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mcpmate_schema_migrations WHERE target = 'config'")
        .fetch_one(&pool)
        .await
        .expect("count rolled-back migration records");
    assert_eq!(recorded, 0);
    assert!(table_exists(&pool, "capability_refs_valid").await);
}

#[tokio::test]
async fn replaces_only_empty_legacy_secure_store_storage() {
    let empty = memory_support::pool().await;
    sqlx::query(
        "CREATE TABLE secure_store_secrets (
            alias TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            encrypted_value TEXT NOT NULL
        )",
    )
    .execute(&empty)
    .await
    .expect("create empty legacy secure store");
    prepare_config_database(&empty, DatabaseSource::InMemory)
        .await
        .expect("replace empty legacy secure store");
    let columns = table_columns(&empty, "secure_store_secrets").await;
    for required in ["provider_id", "provider_kind", "key_nonce", "encrypted_key"] {
        assert!(columns.iter().any(|column| column == required));
    }

    let nonempty = memory_support::pool().await;
    sqlx::query(
        "CREATE TABLE secure_store_secrets (
            alias TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            encrypted_value TEXT NOT NULL
        )",
    )
    .execute(&nonempty)
    .await
    .expect("create nonempty legacy secure store");
    sqlx::query(
        "INSERT INTO secure_store_secrets (alias, kind, encrypted_value)
         VALUES ('legacy', 'api_key', 'ciphertext')",
    )
    .execute(&nonempty)
    .await
    .expect("insert legacy secret");
    let error = prepare_config_database(&nonempty, DatabaseSource::InMemory)
        .await
        .expect_err("nonempty legacy secure store must not be replaced");
    assert!(error.to_string().contains("cannot be safely upgraded"));
    let retained: String =
        sqlx::query_scalar("SELECT encrypted_value FROM secure_store_secrets WHERE alias = 'legacy'")
            .fetch_one(&nonempty)
            .await
            .expect("retain legacy secret after rollback");
    assert_eq!(retained, "ciphertext");
    assert!(!table_exists(&nonempty, "mcpmate_schema_migrations").await);
}

#[tokio::test]
async fn rejects_nonempty_secure_store_with_incompatible_constraints() {
    let pool = memory_support::pool().await;
    sqlx::query(
        "CREATE TABLE secure_store_secrets (
            alias TEXT,
            kind TEXT,
            label TEXT,
            origin_server_id TEXT,
            origin_server_name TEXT,
            origin_server_kind TEXT,
            origin_source TEXT,
            origin_field_group TEXT,
            origin_field_key TEXT,
            origin_field_index INTEGER,
            origin_field_path TEXT,
            provider_id TEXT,
            provider_kind TEXT,
            version INTEGER,
            key_nonce TEXT,
            encrypted_key TEXT,
            nonce TEXT,
            encrypted_value TEXT,
            key_wrap_alg TEXT,
            encryption_alg TEXT,
            created_at TIMESTAMP,
            updated_at TIMESTAMP
        )",
    )
    .execute(&pool)
    .await
    .expect("create structurally incompatible secure store");
    sqlx::query(
        "INSERT INTO secure_store_secrets (
            alias, kind, provider_id, provider_kind, version, key_nonce,
            encrypted_key, nonce, encrypted_value, key_wrap_alg,
            encryption_alg, created_at, updated_at
        ) VALUES (
            'legacy', 'api_key', 'provider', 'local', 1, 'key-nonce',
            'encrypted-key', 'nonce', 'ciphertext', 'AES-256-GCM',
            'AES-256-GCM', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP
        )",
    )
    .execute(&pool)
    .await
    .expect("insert incompatible secure store record");

    let error = prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect_err("nonempty incompatible secure store must be rejected");
    assert!(error.to_string().contains("cannot be safely upgraded"));
    let retained: String =
        sqlx::query_scalar("SELECT encrypted_value FROM secure_store_secrets WHERE alias = 'legacy'")
            .fetch_one(&pool)
            .await
            .expect("retain secret after rollback");
    assert_eq!(retained, "ciphertext");
    assert!(!table_exists(&pool, "mcpmate_schema_migrations").await);
}

#[tokio::test]
async fn replaces_only_empty_incompatible_secure_store_companion_tables() {
    let pool = memory_support::pool().await;
    sqlx::query("CREATE TABLE secure_store_usages (id TEXT PRIMARY KEY, alias TEXT)")
        .execute(&pool)
        .await
        .expect("create incompatible secure store usage table");

    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("replace empty incompatible secure store tables");

    let columns = table_columns(&pool, "secure_store_usages").await;
    for required in ["server_id", "location_kind", "created_at", "updated_at"] {
        assert!(columns.iter().any(|column| column == required));
    }
    let foreign_keys: Vec<String> =
        sqlx::query_scalar("SELECT \"table\" FROM pragma_foreign_key_list('secure_store_usages')")
            .fetch_all(&pool)
            .await
            .expect("inspect secure store usage foreign keys");
    assert!(foreign_keys.iter().any(|table| table == "secure_store_secrets"));
}
