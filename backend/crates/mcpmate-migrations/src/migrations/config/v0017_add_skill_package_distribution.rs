use anyhow::{Context, Result};
use async_trait::async_trait;
use sqlx::{Sqlite, Transaction};

use super::super::{Migration, MigrationStep};

const SCHEMA: &str = include_str!("v0017_add_skill_package_distribution.sql");

pub(super) fn migration() -> Migration {
    Migration::rust(
        17,
        "add skill package distribution",
        &[include_str!("v0017_add_skill_package_distribution.rs"), SCHEMA],
        AddSkillPackageDistribution,
    )
}

struct AddSkillPackageDistribution;

#[async_trait]
impl MigrationStep for AddSkillPackageDistribution {
    async fn apply(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
    ) -> Result<()> {
        sqlx::query(SCHEMA)
            .execute(&mut **transaction)
            .await
            .context("create workflow skill settings")?;
        Ok(())
    }
}
