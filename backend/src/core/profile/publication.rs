use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sqlx::{Pool, Sqlite, Transaction};

use super::workflow::WorkflowBindingPolicy;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PublishedWorkflowDirectSet {
    pub tools: HashSet<(String, String)>,
    pub prompts: HashSet<(String, String)>,
    pub resources: HashSet<(String, String)>,
    pub templates: HashSet<(String, String)>,
}

impl PublishedWorkflowDirectSet {
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.prompts.is_empty() && self.resources.is_empty() && self.templates.is_empty()
    }

    pub fn contains_tool(
        &self,
        server_id: &str,
        tool_name: &str,
    ) -> bool {
        self.tools.contains(&(server_id.to_string(), tool_name.to_string()))
    }

    pub fn contains_prompt(
        &self,
        server_id: &str,
        prompt_name: &str,
    ) -> bool {
        self.prompts.contains(&(server_id.to_string(), prompt_name.to_string()))
    }

    pub fn contains_resource(
        &self,
        server_id: &str,
        resource_uri: &str,
    ) -> bool {
        self.resources
            .contains(&(server_id.to_string(), resource_uri.to_string()))
    }

    pub fn contains_template(
        &self,
        server_id: &str,
        uri_template: &str,
    ) -> bool {
        self.templates
            .contains(&(server_id.to_string(), uri_template.to_string()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishedWorkflowDirectRef {
    pub profile_id: String,
    pub ref_id: String,
    pub kind: String,
    pub server_id: String,
    pub external_key: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct PublishedSkillCapability {
    pub kind: String,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishedSkillPackage {
    pub profile_id: String,
    pub skill_name: String,
    pub description: Option<String>,
    pub step_titles: Vec<String>,
    pub direct_capabilities: Vec<PublishedSkillCapability>,
    pub meta_capabilities: Vec<PublishedSkillCapability>,
    pub distribution: Option<SkillPackageDistribution>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkillPackageDistribution {
    Symlink,
    Copy,
}

impl SkillPackageDistribution {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Symlink => "symlink",
            Self::Copy => "copy",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "symlink" => Some(Self::Symlink),
            "copy" => Some(Self::Copy),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, sqlx::FromRow)]
struct BindingRow {
    profile_id: String,
    ref_id: String,
    binding_policy: String,
    kind: String,
    server_id: String,
    origin_key: String,
}

pub fn effective_direct_ref_ids(bindings: &[PublishedWorkflowDirectRef]) -> Vec<PublishedWorkflowDirectRef> {
    let mut effective: BTreeMap<(String, String), PublishedWorkflowDirectRef> = BTreeMap::new();
    for binding in bindings {
        let key = (binding.kind.clone(), binding.external_key.clone());
        match effective.get(&key) {
            Some(existing) if existing.ref_id == binding.ref_id => {}
            _ => {
                effective.insert(key, binding.clone());
            }
        }
    }
    effective.into_values().collect()
}

const PUBLISHED_WORKFLOW_BINDINGS_SQL: &str = r#"
        SELECT binding.profile_id, binding.ref_id, binding.binding_policy, capability.kind,
               capability.server_id, capability.origin_key
        FROM workflow_profile_step_bindings binding
        JOIN profile ON profile.id = binding.profile_id
        JOIN capability_refs capability ON capability.ref_id = binding.ref_id
        WHERE profile.is_active = 1
          AND profile.profile_mode = 'workflow'
          AND capability.state <> 'retired'
        ORDER BY binding.profile_id, binding.ref_id
        "#;

pub async fn load_published_workflow_direct_refs(pool: &Pool<Sqlite>) -> Result<Vec<PublishedWorkflowDirectRef>> {
    load_published_workflow_direct_refs_on(pool).await
}

pub async fn load_published_workflow_direct_refs_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>
) -> Result<Vec<PublishedWorkflowDirectRef>> {
    load_published_workflow_direct_refs_on(&mut **transaction).await
}

async fn load_published_workflow_direct_refs_on<'e, E>(executor: E) -> Result<Vec<PublishedWorkflowDirectRef>>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    let rows = sqlx::query_as::<_, BindingRow>(PUBLISHED_WORKFLOW_BINDINGS_SQL)
        .fetch_all(executor)
        .await
        .context("load published workflow bindings")?;
    Ok(effective_direct_from_rows(rows))
}

pub fn published_direct_set(refs: &[PublishedWorkflowDirectRef]) -> PublishedWorkflowDirectSet {
    let mut set = PublishedWorkflowDirectSet::default();
    for item in refs {
        let key = (item.server_id.clone(), item.external_key.clone());
        match item.kind.as_str() {
            "tools" => {
                set.tools.insert(key);
            }
            "prompts" => {
                set.prompts.insert(key);
            }
            "resources" => {
                set.resources.insert(key);
            }
            "resource_templates" => {
                set.templates.insert(key);
            }
            _ => {}
        }
    }
    set
}

pub async fn load_published_direct_set(pool: &Pool<Sqlite>) -> Result<PublishedWorkflowDirectSet> {
    let refs = load_published_workflow_direct_refs(pool).await?;
    Ok(published_direct_set(&refs))
}

pub async fn load_published_skill_packages(pool: &Pool<Sqlite>) -> Result<Vec<PublishedSkillPackage>> {
    let rows: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        r#"
        SELECT profile.id, skill.skill_name, profile.description, settings.package_distribution
        FROM workflow_profile_skills skill
        JOIN profile ON profile.id = skill.profile_id
        LEFT JOIN workflow_profile_skill_settings settings ON settings.profile_id = skill.profile_id
        WHERE profile.is_active = 1
          AND profile.profile_mode = 'workflow'
        ORDER BY skill.skill_name
        "#,
    )
    .fetch_all(pool)
    .await
    .context("load published skill packages")?;
    let step_rows: Vec<(String, String)> = sqlx::query_as(
        r#"
        SELECT step.profile_id, step.title
        FROM workflow_profile_steps step
        JOIN profile ON profile.id = step.profile_id
        WHERE profile.is_active = 1
          AND profile.profile_mode = 'workflow'
        ORDER BY step.profile_id, step.step_index
        "#,
    )
    .fetch_all(pool)
    .await
    .context("load published skill step titles")?;
    let mut step_titles: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (profile_id, title) in step_rows {
        step_titles.entry(profile_id).or_default().push(title);
    }
    let binding_rows = sqlx::query_as::<_, BindingRow>(PUBLISHED_WORKFLOW_BINDINGS_SQL)
        .fetch_all(pool)
        .await
        .context("load published skill bindings")?;
    let mut reachability: BTreeMap<String, (Vec<PublishedSkillCapability>, Vec<PublishedSkillCapability>)> =
        BTreeMap::new();
    let mut policy_by_name: BTreeMap<(String, String, String), WorkflowBindingPolicy> = BTreeMap::new();
    for row in &binding_rows {
        let key = (row.profile_id.clone(), row.kind.clone(), row.origin_key.clone());
        let policy = WorkflowBindingPolicy::from_str_or_meta(&row.binding_policy);
        match policy_by_name.get(&key) {
            Some(WorkflowBindingPolicy::Direct) => {}
            _ => {
                policy_by_name.insert(key, policy);
            }
        }
    }
    for ((profile_id, kind, name), policy) in policy_by_name {
        let entry = reachability.entry(profile_id).or_default();
        let capability = PublishedSkillCapability { kind, name };
        match policy {
            WorkflowBindingPolicy::Direct => entry.0.push(capability),
            WorkflowBindingPolicy::MetaOnDemand => entry.1.push(capability),
        }
    }
    Ok(rows
        .into_iter()
        .map(|(profile_id, skill_name, description, distribution)| {
            let (direct_capabilities, meta_capabilities) = reachability.remove(&profile_id).unwrap_or_default();
            PublishedSkillPackage {
                step_titles: step_titles.remove(&profile_id).unwrap_or_default(),
                profile_id,
                skill_name,
                description,
                direct_capabilities,
                meta_capabilities,
                distribution: distribution.as_deref().and_then(SkillPackageDistribution::parse),
            }
        })
        .collect())
}

pub fn skill_catalog_summary(
    description: Option<&str>,
    step_titles: &[String],
) -> String {
    let mut parts = Vec::new();
    if let Some(description) = description.map(str::trim).filter(|value| !value.is_empty()) {
        parts.push(description.to_string());
    }
    if !step_titles.is_empty() {
        parts.push(step_titles.join(" "));
    }
    parts.join(" — ")
}

pub fn skill_package_dir(
    skills_root: &Path,
    skill_name: &str,
) -> PathBuf {
    skills_root.join(skill_name)
}

pub fn skill_resource_uri(skill_name: &str) -> String {
    format!("skills://{skill_name}/SKILL.md")
}

const APPROVED_CLIENT_MODES_SQL: &str = r#"
        SELECT identifier, config_mode
        FROM client
        WHERE approval_status = 'approved'
        ORDER BY identifier
        "#;

pub async fn load_unify_consumer_ids(
    pool: &Pool<Sqlite>,
    default_config_mode: &str,
) -> Result<Vec<String>> {
    load_unify_consumer_ids_on(pool, default_config_mode).await
}

pub async fn load_unify_consumer_ids_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    default_config_mode: &str,
) -> Result<Vec<String>> {
    load_unify_consumer_ids_on(&mut **transaction, default_config_mode).await
}

async fn load_unify_consumer_ids_on<'e, E>(
    executor: E,
    default_config_mode: &str,
) -> Result<Vec<String>>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(APPROVED_CLIENT_MODES_SQL)
        .fetch_all(executor)
        .await
        .context("load unify consumers")?;
    Ok(filter_unify_consumers(rows, default_config_mode))
}

fn filter_unify_consumers(
    rows: Vec<(String, Option<String>)>,
    default_config_mode: &str,
) -> Vec<String> {
    rows.into_iter()
        .filter_map(|(consumer_id, config_mode)| {
            let effective =
                crate::config::client::init::effective_client_config_mode(config_mode.as_deref(), default_config_mode);
            (effective == "unify").then_some(consumer_id)
        })
        .collect()
}

fn effective_direct_from_rows(rows: Vec<BindingRow>) -> Vec<PublishedWorkflowDirectRef> {
    let mut by_name: BTreeMap<(String, String), WorkflowBindingPolicy> = BTreeMap::new();
    for row in &rows {
        let name = (row.kind.clone(), row.origin_key.clone());
        let policy = WorkflowBindingPolicy::from_str_or_meta(&row.binding_policy);
        match by_name.get(&name) {
            Some(WorkflowBindingPolicy::Direct) => {}
            _ => {
                by_name.insert(name, policy);
            }
        }
    }
    let mut seen = BTreeSet::new();
    let mut directs = Vec::new();
    for row in rows {
        let name = (row.kind.clone(), row.origin_key.clone());
        if by_name.get(&name) != Some(&WorkflowBindingPolicy::Direct) {
            continue;
        }
        if !seen.insert((row.ref_id.clone(), row.kind.clone())) {
            continue;
        }
        directs.push(PublishedWorkflowDirectRef {
            profile_id: row.profile_id,
            ref_id: row.ref_id,
            kind: row.kind,
            server_id: row.server_id,
            external_key: row.origin_key,
        });
    }
    directs
}

impl WorkflowBindingPolicy {
    fn from_str_or_meta(value: &str) -> Self {
        match value {
            "direct" => Self::Direct,
            _ => Self::MetaOnDemand,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_direct_keeps_any_direct_name() {
        let bindings = vec![
            PublishedWorkflowDirectRef {
                profile_id: "wf-a".into(),
                ref_id: "ref-1".into(),
                kind: "tools".into(),
                server_id: "srv".into(),
                external_key: "lookup".into(),
            },
            PublishedWorkflowDirectRef {
                profile_id: "wf-a".into(),
                ref_id: "ref-1".into(),
                kind: "tools".into(),
                server_id: "srv".into(),
                external_key: "lookup".into(),
            },
        ];
        let effective = effective_direct_ref_ids(&bindings);
        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].external_key, "lookup");
    }

    #[test]
    fn any_direct_occurrence_makes_the_name_direct() {
        let directs = effective_direct_from_rows(vec![
            BindingRow {
                profile_id: "wf-a".into(),
                ref_id: "ref-meta".into(),
                binding_policy: "meta_on_demand".into(),
                kind: "tools".into(),
                server_id: "srv".into(),
                origin_key: "lookup".into(),
            },
            BindingRow {
                profile_id: "wf-a".into(),
                ref_id: "ref-direct".into(),
                binding_policy: "direct".into(),
                kind: "tools".into(),
                server_id: "srv".into(),
                origin_key: "lookup".into(),
            },
        ]);
        let set = published_direct_set(&directs);
        assert!(set.contains_tool("srv", "lookup"));
        assert_eq!(set.tools.len(), 1);
    }

    #[test]
    fn all_meta_occurrences_stay_out_of_direct_set() {
        let directs = effective_direct_from_rows(vec![BindingRow {
            profile_id: "wf-a".into(),
            ref_id: "ref-meta".into(),
            binding_policy: "meta_on_demand".into(),
            kind: "tools".into(),
            server_id: "srv".into(),
            origin_key: "lookup".into(),
        }]);
        assert!(published_direct_set(&directs).is_empty());
    }

    #[test]
    fn skill_catalog_summary_includes_description_and_step_titles() {
        assert_eq!(
            skill_catalog_summary(Some("Ship a tagged build"), &["Prepare".into(), "Publish".into()],),
            "Ship a tagged build — Prepare Publish"
        );
    }
}
