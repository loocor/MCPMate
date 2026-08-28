use std::path::Path;

use crate::clients::error::{ConfigError, ConfigResult};
use crate::clients::service::ClientConfigService;
use crate::common::paths::global_paths;
use crate::core::profile::publication::{
    PublishedSkillPackage, SkillPackageDistribution, load_published_skill_packages, skill_package_dir,
};

impl ClientConfigService {
    pub(super) async fn mount_published_skill_packages(
        &self,
        client_id: &str,
    ) -> ConfigResult<()> {
        let effective_mode = self.get_effective_config_mode(client_id).await?;
        if effective_mode != "unify" {
            return Ok(());
        }
        let Some(config_path) = self.resolved_config_path(client_id).await? else {
            return Err(ConfigError::PathResolutionError(format!(
                "Client '{client_id}' has no config path; cannot mount Skill packages"
            )));
        };
        let client_skills_dir = Path::new(&config_path)
            .parent()
            .ok_or_else(|| {
                ConfigError::PathResolutionError(format!("Client '{client_id}' config path has no parent directory"))
            })?
            .join("skills");
        let skills_root = global_paths()
            .database_path()
            .parent()
            .unwrap_or(Path::new("."))
            .join("skills");
        let packages = load_published_skill_packages(&self.db_pool)
            .await
            .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
        mount_explicit_skill_packages(effective_mode.as_str(), &client_skills_dir, &skills_root, &packages)
    }
}

pub(crate) fn mount_explicit_skill_packages(
    effective_mode: &str,
    client_skills_dir: &Path,
    skills_root: &Path,
    packages: &[PublishedSkillPackage],
) -> ConfigResult<()> {
    if effective_mode != "unify" {
        return Ok(());
    }
    for package in packages {
        let Some(distribution) = package.distribution else {
            continue;
        };
        let source = skill_package_dir(skills_root, &package.skill_name);
        if !source.is_dir() {
            return Err(ConfigError::DataAccessError(format!(
                "Published skill package '{}' is missing at {}",
                package.skill_name,
                source.display()
            )));
        }
        let destination = client_skills_dir.join(&package.skill_name);
        mount_package(&source, &destination, distribution)?;
    }
    Ok(())
}

fn mount_package(
    source: &Path,
    destination: &Path,
    distribution: SkillPackageDistribution,
) -> ConfigResult<()> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            ConfigError::DataAccessError(format!(
                "Failed to create Skill mount directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    if destination.exists() || destination.symlink_metadata().is_ok() {
        if destination.is_dir() && !destination.is_symlink() {
            std::fs::remove_dir_all(destination)
        } else {
            std::fs::remove_file(destination)
        }
        .map_err(|error| {
            ConfigError::DataAccessError(format!(
                "Failed to replace Skill mount {}: {error}",
                destination.display()
            ))
        })?;
    }
    match distribution {
        SkillPackageDistribution::Symlink => {
            symlink_dir(source, destination)?;
        }
        SkillPackageDistribution::Copy => {
            copy_dir(source, destination)?;
        }
    }
    Ok(())
}

fn symlink_dir(
    source: &Path,
    destination: &Path,
) -> ConfigResult<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source, destination).map_err(|error| {
            ConfigError::DataAccessError(format!(
                "Failed to symlink Skill package {} -> {}: {error}",
                source.display(),
                destination.display()
            ))
        })
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(source, destination).map_err(|error| {
            ConfigError::DataAccessError(format!(
                "Failed to symlink Skill package {} -> {}: {error}",
                source.display(),
                destination.display()
            ))
        })
    }
}

fn copy_dir(
    source: &Path,
    destination: &Path,
) -> ConfigResult<()> {
    std::fs::create_dir_all(destination).map_err(|error| {
        ConfigError::DataAccessError(format!(
            "Failed to copy Skill package into {}: {error}",
            destination.display()
        ))
    })?;
    for entry in std::fs::read_dir(source).map_err(|error| ConfigError::DataAccessError(error.to_string()))? {
        let entry = entry.map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to).map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn symlink_and_copy_mount_explicit_packages() {
        let root = tempfile::tempdir().expect("temp dir");
        let source = root.path().join("source");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("SKILL.md"), "# skill").unwrap();
        fs::write(source.join("nested/note.md"), "note").unwrap();

        let linked = root.path().join("linked");
        mount_package(&source, &linked, SkillPackageDistribution::Symlink).unwrap();
        assert!(linked.is_symlink() || linked.is_dir());
        assert_eq!(fs::read_to_string(linked.join("SKILL.md")).unwrap(), "# skill");

        let copied = root.path().join("copied");
        mount_package(&source, &copied, SkillPackageDistribution::Copy).unwrap();
        assert!(!copied.is_symlink());
        assert_eq!(fs::read_to_string(copied.join("nested/note.md")).unwrap(), "note");
    }

    #[test]
    fn copy_mount_fails_when_source_is_missing() {
        let root = tempfile::tempdir().expect("temp dir");
        let error = mount_package(
            &root.path().join("missing"),
            &root.path().join("dest"),
            SkillPackageDistribution::Copy,
        )
        .expect_err("missing package must fail");
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn transparent_mode_does_not_mount_explicit_packages() {
        let root = tempfile::tempdir().expect("temp dir");
        let skills_root = root.path().join("skills");
        let source = skills_root.join("release-flow");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("SKILL.md"), "# skill").unwrap();
        let client_skills_dir = root.path().join("client/skills");
        let packages = [crate::core::profile::publication::PublishedSkillPackage {
            profile_id: "wf".into(),
            skill_name: "release-flow".into(),
            description: None,
            step_titles: Vec::new(),
            direct_capabilities: Vec::new(),
            meta_capabilities: Vec::new(),
            distribution: Some(SkillPackageDistribution::Copy),
        }];

        mount_explicit_skill_packages("transparent", &client_skills_dir, &skills_root, &packages).unwrap();
        assert!(
            !client_skills_dir.join("release-flow").exists(),
            "Transparent clients must not receive a Skill mount"
        );

        mount_explicit_skill_packages("hosted", &client_skills_dir, &skills_root, &packages).unwrap();
        assert!(
            !client_skills_dir.join("release-flow").exists(),
            "Hosted clients must not receive a Skill mount in this slice"
        );

        mount_explicit_skill_packages("unify", &client_skills_dir, &skills_root, &packages).unwrap();
        assert_eq!(
            fs::read_to_string(client_skills_dir.join("release-flow/SKILL.md")).unwrap(),
            "# skill"
        );
    }
}
