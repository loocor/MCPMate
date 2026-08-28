#[path = "support/memory.rs"]
mod memory_support;

use mcpmate_migrations::{DatabaseSource, prepare_config_database, verify_config_database};

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

#[tokio::test]
async fn verification_does_not_initialize_an_empty_database() {
    let pool = memory_support::pool().await;

    let error = verify_config_database(&pool)
        .await
        .expect_err("an empty database is not prepared");

    assert!(
        error.to_string().contains("migration ledger"),
        "unexpected verification error: {error:#}"
    );
    assert!(!table_exists(&pool, "mcpmate_schema_migrations").await);
}

#[tokio::test]
async fn preparation_applies_the_complete_config_stream_once() {
    let pool = memory_support::pool().await;

    let backup = prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("prepare in-memory config database");
    assert!(backup.is_none());
    verify_config_database(&pool)
        .await
        .expect("verify prepared config database");

    let applied: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mcpmate_schema_migrations WHERE target = 'config'")
        .fetch_one(&pool)
        .await
        .expect("count applied config migrations");
    assert_eq!(applied, 17);

    for table in [
        "workflow_profile_skills",
        "workflow_profile_material_libraries",
        "workflow_profile_materials",
        "workflow_profile_step_materials",
    ] {
        assert!(table_exists(&pool, table).await, "{table} should be created by v0015");
    }
    for table in [
        "workflow_profile_guides",
        "workflow_profile_package_files",
        "workflow_profile_external_guides",
        "workflow_profile_skill_projections",
    ] {
        assert!(table_exists(&pool, table).await, "{table} should be created by v0016");
    }
    for referenced_table in ["workflow_profile_steps", "workflow_profile_materials"] {
        let foreign_key_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_foreign_key_list('workflow_profile_step_materials') WHERE \"table\" = ?",
        )
        .bind(referenced_table)
        .fetch_one(&pool)
        .await
        .expect("inspect Step-Material foreign keys");
        assert!(
            foreign_key_count > 0,
            "Step-Material rows must reference {referenced_table}"
        );
    }

    let second_backup = prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("prepare config database again");
    assert!(second_backup.is_none());
}

#[tokio::test]
async fn verification_rejects_a_deleted_migration_record() {
    let pool = memory_support::pool().await;
    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("prepare in-memory config database");
    sqlx::query("DELETE FROM mcpmate_schema_migrations WHERE target = 'config' AND version = 5")
        .execute(&pool)
        .await
        .expect("delete migration record");

    let error = verify_config_database(&pool)
        .await
        .expect_err("deleted migration history must be rejected");
    assert!(
        error.to_string().contains("valid migration prefix")
            || error.to_string().contains("does not match its history"),
        "unexpected verification error: {error:#}"
    );
}

#[tokio::test]
async fn verification_rejects_an_unknown_migration_record() {
    let pool = memory_support::pool().await;
    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("prepare in-memory config database");
    sqlx::query(
        "INSERT INTO mcpmate_schema_migrations (target, version, name, checksum)
         VALUES ('config', 999, 'unknown migration', 'unknown')",
    )
    .execute(&pool)
    .await
    .expect("insert unknown migration record");

    let error = verify_config_database(&pool)
        .await
        .expect_err("unknown migration history must be rejected");
    assert!(
        error.to_string().contains("unknown migration") || error.to_string().contains("valid migration prefix"),
        "unexpected verification error: {error:#}"
    );
}

#[tokio::test]
async fn verification_rejects_a_missing_ledger_state() {
    let pool = memory_support::pool().await;
    prepare_config_database(&pool, DatabaseSource::InMemory)
        .await
        .expect("prepare in-memory config database");
    sqlx::query("DROP TABLE mcpmate_schema_migration_state")
        .execute(&pool)
        .await
        .expect("remove ledger state");

    let error = verify_config_database(&pool)
        .await
        .expect_err("missing ledger state must be rejected");
    assert!(
        error.to_string().contains("ledger state") && error.to_string().contains("missing"),
        "unexpected verification error: {error:#}"
    );
}
