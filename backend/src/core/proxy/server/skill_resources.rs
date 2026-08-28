use std::path::{Path, PathBuf};

use rmcp::ErrorData;
use rmcp::model::{ReadResourceResult, Resource, ResourceContents};
use sqlx::Pool;
use sqlx::Sqlite;

use crate::core::profile::publication::{load_published_skill_packages, skill_package_dir, skill_resource_uri};

const SKILL_URI_PREFIX: &str = "skills://";

fn skill_resources_visible(config_mode: Option<&str>) -> bool {
    matches!(config_mode, Some("unify"))
}

fn is_published_skill_file(
    skill_name: &str,
    relative: &str,
) -> bool {
    !skill_name.is_empty()
        && Path::new(skill_name).file_name().is_some_and(|name| name == skill_name)
        && relative == "SKILL.md"
}

pub async fn listed_resources_for_client(
    pool: &Pool<Sqlite>,
    config_mode: Option<&str>,
) -> Result<Vec<Resource>, ErrorData> {
    if !skill_resources_visible(config_mode) {
        return Ok(Vec::new());
    }
    let packages = load_published_skill_packages(pool)
        .await
        .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
    Ok(packages
        .into_iter()
        .map(|package| {
            let mut resource = Resource::new(skill_resource_uri(&package.skill_name), package.skill_name.clone())
                .with_mime_type("text/markdown");
            if let Some(description) = package.description.filter(|value| !value.trim().is_empty()) {
                resource = resource.with_description(description);
            }
            resource
        })
        .collect())
}

pub async fn try_read_for_client(
    pool: &Pool<Sqlite>,
    skills_root: &Path,
    uri: &str,
    config_mode: Option<&str>,
) -> Option<Result<ReadResourceResult, ErrorData>> {
    if !skill_resources_visible(config_mode) {
        return None;
    }
    try_read(pool, skills_root, uri).await
}

async fn try_read(
    pool: &Pool<Sqlite>,
    skills_root: &Path,
    uri: &str,
) -> Option<Result<ReadResourceResult, ErrorData>> {
    let remainder = uri.strip_prefix(SKILL_URI_PREFIX)?;
    let (skill_name, relative) = remainder.split_once('/')?;
    if !is_published_skill_file(skill_name, relative) {
        return Some(Err(ErrorData::invalid_params(
            "Skill resource URI is invalid".to_string(),
            None,
        )));
    }
    let packages = match load_published_skill_packages(pool).await {
        Ok(packages) => packages,
        Err(error) => return Some(Err(ErrorData::internal_error(error.to_string(), None))),
    };
    if !packages.iter().any(|package| package.skill_name == skill_name) {
        return Some(Err(ErrorData::resource_not_found(
            format!("Published skill '{skill_name}' was not found"),
            None,
        )));
    }
    let path = skill_package_dir(skills_root, skill_name).join(relative);
    let markdown = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Some(Err(ErrorData::resource_not_found(
                format!("Skill file '{relative}' was not found in '{skill_name}'"),
                None,
            )));
        }
        Err(error) => return Some(Err(ErrorData::internal_error(error.to_string(), None))),
    };
    Some(Ok(ReadResourceResult::new(vec![
        ResourceContents::text(markdown, uri).with_mime_type("text/markdown"),
    ])))
}

pub fn skills_root_from_database_path(database_path: &Path) -> PathBuf {
    database_path.parent().unwrap_or(Path::new(".")).join("skills")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    #[tokio::test]
    async fn transparent_mode_does_not_list_or_read_skill_resources() {
        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .expect("connect");
        let listed = listed_resources_for_client(&pool, Some("transparent"))
            .await
            .expect("transparent list must not query Skill packages");
        assert!(listed.is_empty());
        assert!(
            try_read_for_client(
                &pool,
                Path::new("."),
                "skills://release-flow/SKILL.md",
                Some("transparent")
            )
            .await
            .is_none()
        );
        assert!(
            listed_resources_for_client(&pool, Some("hosted"))
                .await
                .unwrap()
                .is_empty()
        );
        listed_resources_for_client(&pool, Some("unify"))
            .await
            .expect_err("Unify must reach Skill package load");
    }

    #[tokio::test]
    async fn absolute_skill_paths_are_rejected_before_read() {
        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .expect("connect");
        let result = try_read_for_client(
            &pool,
            Path::new("/tmp"),
            "skills://release-flow//etc/passwd",
            Some("unify"),
        )
        .await
        .expect("unify read is handled");
        assert!(result.is_err(), "absolute Skill paths must not be read");
        assert!(!is_published_skill_file("release-flow", "/etc/passwd"));
        assert!(!is_published_skill_file("release-flow", "scripts/run.sh"));
        assert!(is_published_skill_file("release-flow", "SKILL.md"));
    }
}
