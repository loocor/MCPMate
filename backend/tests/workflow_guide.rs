use std::collections::{BTreeMap, BTreeSet};

use mcpmate::core::profile::guide::{
    WorkflowGuideCapability, WorkflowGuideError, WorkflowGuidePackageCategory, WorkflowGuidePackageFile,
    WorkflowGuidePackageFileRevision, WorkflowGuidePackageFileSaveCommand, WorkflowGuidePreviewCommand,
    WorkflowGuideReclamationConfirmation, WorkflowGuideReclamationPlan, WorkflowGuideSaveCommand, WorkflowGuideService,
    effective_exposure_by_name, parse_workflow_guide, render_workflow_skill,
    test_support::{
        ExternalGuideRow, allocate_package_path, build_guide_document_graph, format_skill_definition,
        normalize_main_guide_markdown, validate_package_file_command, verify_reclamation_confirmation,
    },
};
use mcpmate::core::profile::materials::WorkflowMaterialsService;
use mcpmate::core::profile::workflow::WorkflowBindingPolicy;
use sha2::{Digest, Sha256};

#[path = "support/database.rs"]
mod database_support;

fn external_row(
    path: &str,
    markdown: &str,
) -> ExternalGuideRow {
    ExternalGuideRow {
        package_file_id: format!("file-{path}"),
        file_revision: 1,
        title: path.to_string(),
        relative_path: path.to_string(),
        markdown: markdown.to_string(),
    }
}

fn package_file(path: &str) -> WorkflowGuidePackageFile {
    WorkflowGuidePackageFile {
        package_file_id: format!("file-{path}"),
        file_revision: 1,
        title: path.to_string(),
        category: if path.starts_with("references/") {
            WorkflowGuidePackageCategory::Reference
        } else {
            WorkflowGuidePackageCategory::Asset
        },
        relative_path: path.to_string(),
        mime_type: None,
        extension: None,
        file_size: None,
    }
}

#[test]
fn parses_readable_blocks_and_references() {
    let guide = parse_workflow_guide(
            "# Investigate a release\n\n:::capability {\"name\":\"search-release-logs\",\"exposure\":\"direct\"}\nUse it to collect the release evidence.\n:::\n\nRead [policy](references/release-policy.md).\n",
        )
        .expect("valid Guide");

    assert_eq!(guide.headings[0].text, "Investigate a release");
    assert_eq!(guide.capabilities[0].name, "search-release-logs");
    assert_eq!(guide.capabilities[0].exposure, WorkflowBindingPolicy::Direct);
    assert_eq!(guide.capabilities[0].guide, "Use it to collect the release evidence.");
    assert_eq!(
        guide.package_paths,
        BTreeSet::from(["references/release-policy.md".to_string()])
    );
}

#[test]
fn document_graph_traverses_recursive_references_and_terminates_cycles() {
    let graph = build_guide_document_graph(
        "# Root\n\n[A](references/a.md)\n",
        vec![
            external_row("references/a.md", "# A\n\n[B](b.md#details)\n"),
            external_row(
                "references/b.md",
                "# B\n\n[A](references/a.md)\n\n[Diagram](assets/diagram.png)\n",
            ),
        ],
        &BTreeMap::new(),
    )
    .expect("build recursive document graph");

    assert_eq!(
        graph
            .documents
            .iter()
            .map(|document| document.relative_path.as_str())
            .collect::<Vec<_>>(),
        vec!["SKILL.md", "references/a.md", "references/b.md"]
    );
    assert_eq!(
        graph.combined.package_paths,
        BTreeSet::from([
            "assets/diagram.png".to_string(),
            "references/a.md".to_string(),
            "references/b.md".to_string(),
        ])
    );
}

#[test]
fn document_graph_orders_capabilities_at_recursive_reference_positions() {
    let graph = build_guide_document_graph(
            "# Root\n\n:::capability {\"name\":\"first\",\"exposure\":\"meta_on_demand\"}\nFirst.\n:::\n\n[External](references/external.md)\n\n:::capability {\"name\":\"last\",\"exposure\":\"direct\"}\nLast.\n:::\n",
            vec![external_row(
                "references/external.md",
                "# External\n\n:::capability {\"name\":\"middle\",\"exposure\":\"direct\"}\nMiddle.\n:::\n",
            )],
            &BTreeMap::new(),
        )
        .expect("build recursively ordered document graph");

    assert_eq!(
        graph
            .combined
            .capabilities
            .iter()
            .map(|capability| capability.name.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "middle", "last"]
    );
}

#[test]
fn document_graph_only_follows_standalone_sibling_markdown_references() {
    let graph = build_guide_document_graph(
        "# Root\n\n[A](references/a.md)\n",
        vec![
            external_row("references/a.md", "# A\n\nSee [B](b.md#details) inline.\n\n[C](c.md)\n"),
            external_row("references/b.md", "# B\n"),
            external_row("references/c.md", "# C\n"),
        ],
        &BTreeMap::new(),
    )
    .expect("build document graph");

    assert_eq!(
        graph
            .documents
            .iter()
            .map(|document| document.relative_path.as_str())
            .collect::<Vec<_>>(),
        vec!["SKILL.md", "references/a.md", "references/c.md"]
    );
}

#[test]
fn document_graph_override_reports_only_newly_unreachable_files() {
    let rows = vec![
        external_row("references/a.md", "# A\n\n[B](references/b.md)\n"),
        external_row("references/b.md", "# B\n\n[Diagram](assets/diagram.png)\n"),
    ];
    let persisted = build_guide_document_graph("# Root\n\n[A](references/a.md)\n", rows.clone(), &BTreeMap::new())
        .expect("build persisted graph");
    let candidate = build_guide_document_graph(
        "# Root\n\n[A](references/a.md)\n",
        rows,
        &BTreeMap::from([("references/a.md".to_string(), "# A\n".to_string())]),
    )
    .expect("build candidate graph");
    let package_files = [
        package_file("references/a.md"),
        package_file("references/b.md"),
        package_file("assets/diagram.png"),
    ];

    assert_eq!(
        persisted
            .orphaned_package_files(&candidate, &package_files)
            .into_iter()
            .map(|file| file.relative_path)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["assets/diagram.png".to_string(), "references/b.md".to_string(),])
    );
}

#[test]
fn document_graph_keeps_shared_and_unlinked_external_references_out_of_orphans() {
    let rows = vec![
        external_row("references/a.md", "# A\n\n[B](references/b.md)\n"),
        external_row("references/c.md", "# C\n\n[B](references/b.md)\n"),
        external_row("references/b.md", "# B\n"),
        external_row("references/unlinked.md", "# Unlinked\n\n[B](references/b.md)\n"),
    ];
    let root = "# Root\n\n[A](references/a.md)\n\n[C](references/c.md)\n";
    let persisted = build_guide_document_graph(root, rows.clone(), &BTreeMap::new()).expect("build persisted graph");
    let candidate = build_guide_document_graph(
        root,
        rows.clone(),
        &BTreeMap::from([("references/a.md".to_string(), "# A\n".to_string())]),
    )
    .expect("build shared-reference candidate graph");
    let package_files = [package_file("references/b.md")];
    assert!(persisted.orphaned_package_files(&candidate, &package_files).is_empty());

    let unlinked_persisted =
        build_guide_document_graph("# Root\n", rows.clone(), &BTreeMap::new()).expect("build unlinked persisted graph");
    let unlinked_candidate = build_guide_document_graph(
        "# Root\n",
        rows,
        &BTreeMap::from([("references/unlinked.md".to_string(), "# Changed\n".to_string())]),
    )
    .expect("build unlinked candidate graph");
    assert!(
        unlinked_persisted
            .orphaned_package_files(&unlinked_candidate, &package_files)
            .is_empty()
    );
}

#[test]
fn reclamation_confirmation_rejects_stale_and_duplicate_candidates() {
    let plan = WorkflowGuideReclamationPlan {
        package_files: vec![package_file("references/a.md")],
        capabilities: vec![WorkflowGuideCapability {
            name: "lookup".to_string(),
            exposure: WorkflowBindingPolicy::Direct,
            guide: "Look up the record.".to_string(),
            start_line: 1,
            end_line: 3,
        }],
    };
    let stale = WorkflowGuideReclamationConfirmation {
        package_files: vec![WorkflowGuidePackageFileRevision {
            package_file_id: "file-references/a.md".to_string(),
            file_revision: 2,
        }],
        capability_names: vec!["lookup".to_string()],
    };
    assert!(matches!(
        verify_reclamation_confirmation(&plan, Some(&stale)),
        Err(WorkflowGuideError::ReclamationConfirmationChanged)
    ));

    let duplicate = WorkflowGuideReclamationConfirmation {
        package_files: vec![
            WorkflowGuidePackageFileRevision {
                package_file_id: "file-references/a.md".to_string(),
                file_revision: 1,
            },
            WorkflowGuidePackageFileRevision {
                package_file_id: "file-references/a.md".to_string(),
                file_revision: 1,
            },
        ],
        capability_names: vec!["lookup".to_string(), "lookup".to_string()],
    };
    assert!(matches!(
        verify_reclamation_confirmation(&plan, Some(&duplicate)),
        Err(WorkflowGuideError::ReclamationConfirmationChanged)
    ));
}

#[test]
fn rejects_reserved_workflow_syntax_in_fenced_code() {
    let errors = parse_workflow_guide(
        "```markdown\n:::capability {\"name\":\"fake\",\"exposure\":\"direct\"}\n[Fake](references/fake.md)\n:::\n```",
    )
    .expect_err("fenced pseudo-references must fail");
    assert_eq!(
        errors
            .iter()
            .map(|error| (error.line, error.message.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (
                2,
                "Workflow Guide directives and references are not allowed in fenced code"
            ),
            (
                3,
                "Workflow Guide directives and references are not allowed in fenced code"
            ),
            (
                4,
                "Workflow Guide directives and references are not allowed in fenced code"
            ),
        ]
    );
}

#[test]
fn rejects_reserved_workflow_syntax_in_tilde_fenced_code() {
    let errors = parse_workflow_guide("~~~markdown\n:::capability {\"name\":\"fake\",\"exposure\":\"direct\"}\n~~~")
        .expect_err("tilde fenced pseudo-references must fail");
    assert_eq!(errors[0].line, 2);
    assert_eq!(
        errors[0].message,
        "Workflow Guide directives and references are not allowed in fenced code"
    );
}

#[test]
fn keeps_shorter_fence_markers_inside_a_longer_fence() {
    let errors = parse_workflow_guide(
        "````markdown\n```\n:::capability {\"name\":\"lookup\",\"exposure\":\"direct\"}\n:::\n```\n````",
    )
    .expect_err("reserved syntax inside the outer fence must remain inert");

    assert!(
        errors
            .iter()
            .any(|error| { error.line == 3 && error.message.contains("not allowed in fenced code") })
    );
    assert!(
        errors
            .iter()
            .any(|error| { error.line == 4 && error.message.contains("not allowed in fenced code") })
    );
}

#[test]
fn normalizes_imported_skill_front_matter_before_projection() {
    let body = normalize_main_guide_markdown(
        "---\nname: imported-skill\ndescription: Imported description\n---\n\n# Imported heading\n",
    )
    .expect("valid imported Skill");
    let guide = parse_workflow_guide(&body).expect("normalized Guide");
    let rendered = render_workflow_skill(&body, &guide, &effective_exposure_by_name(&guide.capabilities));
    let skill = format_skill_definition(
        "profile-skill",
        "Profile",
        "Profile description",
        None,
        &rendered.markdown,
    );

    assert_eq!(skill.matches("name:").count(), 1);
    assert_eq!(skill.matches("description:").count(), 1);
    assert!(skill.contains("# Imported heading"));
    assert!(!skill.contains("imported-skill"));
}

#[test]
fn reports_malformed_directives_and_opaque_identifiers() {
    let errors = parse_workflow_guide(
        ":::capability {\"name\":\"lookup\"}\n\n550e8400-e29b-41d4-a716-446655440000\nskill://internal",
    )
    .expect_err("invalid Guide");

    assert_eq!(errors.len(), 3);
    assert!(
        errors
            .iter()
            .any(|error| error.message.contains("invalid Capability directive"))
    );
    assert!(errors.iter().any(|error| error.message.contains("opaque identifiers")));
    assert!(errors.iter().any(|error| error.message.contains("skill://")));
}

#[test]
fn projects_external_directives_and_standalone_markdown_references() {
    let directive = concat!(
        "# Investigate\n\n",
        ":::external {\"title\":\"Evidence index\",\"path\":\"references/evidence-index.md\"}\n",
        "Open this when you need the evidence checklist.\n",
        ":::\n"
    );
    let directive_guide = parse_workflow_guide(directive).expect("valid external directive");
    let directive_rendered = render_workflow_skill(
        directive,
        &directive_guide,
        &effective_exposure_by_name(&directive_guide.capabilities),
    );
    assert!(
        directive_rendered
            .markdown
            .contains("Open this when you need the evidence checklist.")
    );
    assert!(
        directive_rendered
            .markdown
            .contains("[Evidence index](references/evidence-index.md)")
    );
    assert!(!directive_rendered.markdown.contains("**External document:"));
    assert!(!directive_rendered.markdown.contains("Consult `"));
    assert!(!directive_rendered.markdown.contains(":::external"));

    let standalone = "# Investigate\n\n[Evidence index](references/evidence-index.md)\n";
    let standalone_guide = parse_workflow_guide(standalone).expect("valid standalone external link");
    let standalone_rendered = render_workflow_skill(
        standalone,
        &standalone_guide,
        &effective_exposure_by_name(&standalone_guide.capabilities),
    );
    assert!(
        standalone_rendered
            .markdown
            .contains("[Evidence index](references/evidence-index.md)")
    );
    assert!(!standalone_rendered.markdown.contains("**External document:"));
    assert!(!standalone_rendered.markdown.contains("Consult `"));

    let anchored = "# Investigate\n\n[API](references/api.md#auth)\n";
    let anchored_guide = parse_workflow_guide(anchored).expect("valid anchored standalone link");
    let anchored_rendered = render_workflow_skill(
        anchored,
        &anchored_guide,
        &effective_exposure_by_name(&anchored_guide.capabilities),
    );
    assert!(anchored_rendered.markdown.contains("[API](references/api.md#auth)"));
    assert!(!anchored_rendered.markdown.contains("[API](references/api.md)\n"));
}

#[test]
fn projects_only_standard_markdown_and_readable_names() {
    let markdown = "# Investigate\n\n:::capability {\"name\":\"search-release-logs\",\"exposure\":\"direct\"}\nUse it to search release logs.\n:::\n";
    let guide = parse_workflow_guide(markdown).expect("valid Guide");
    let rendered = render_workflow_skill(markdown, &guide, &effective_exposure_by_name(&guide.capabilities));

    assert_eq!(
        rendered.markdown,
        concat!(
            "# Investigate\n\n",
            "The following steps are MCP server capability invocations. ",
            "Steps marked `(direct)` can be invoked by tool name.\n\n",
            "`search-release-logs` (direct): Use it to search release logs."
        )
    );
    assert!(!rendered.markdown.contains(":::capability"));
    assert!(!rendered.markdown.contains("Exposure:"));
}

#[test]
fn normalizes_mixed_occurrence_exposure_to_direct_invocation() {
    let markdown = concat!(
        "# Investigate\n\n",
        ":::capability {\"name\":\"search-release-logs\",\"exposure\":\"meta_on_demand\"}\n",
        "Inspect first.\n",
        ":::\n\n",
        ":::capability {\"name\":\"search-release-logs\",\"exposure\":\"direct\"}\n",
        "Then capture the screenshot.\n",
        ":::\n"
    );
    let guide = parse_workflow_guide(markdown).expect("valid Guide");
    let rendered = render_workflow_skill(markdown, &guide, &effective_exposure_by_name(&guide.capabilities));

    assert_eq!(
        rendered.markdown,
        concat!(
            "# Investigate\n\n",
            "The following steps are MCP server capability invocations. ",
            "Steps marked `(direct)` can be invoked by tool name.\n\n",
            "- `search-release-logs` (direct): Inspect first.\n",
            "- `search-release-logs` (direct): Then capture the screenshot."
        )
    );
    assert!(!rendered.markdown.contains("mcpmate_ucan_details"));
    assert!(!rendered.markdown.contains("Exposure:"));
}

#[test]
fn projects_meta_on_demand_invocation_without_direct_labels() {
    let markdown = concat!(
        "# Investigate\n\n",
        ":::capability {\"name\":\"search-release-logs\",\"exposure\":\"meta_on_demand\"}\n",
        "Inspect first.\n",
        ":::\n\n",
        ":::capability {\"name\":\"search-release-logs\",\"exposure\":\"meta_on_demand\"}\n",
        "Then summarize.\n",
        ":::\n"
    );
    let guide = parse_workflow_guide(markdown).expect("valid Guide");
    let rendered = render_workflow_skill(markdown, &guide, &effective_exposure_by_name(&guide.capabilities));

    assert!(rendered.markdown.contains("mcpmate_ucan_details"));
    assert!(!rendered.markdown.contains("Use `search-release-logs` directly."));
    assert!(!rendered.markdown.contains("**Capability:"));
    assert_eq!(
        rendered.markdown,
        concat!(
            "# Investigate\n\n",
            "The following steps are MCP server capability invocations. ",
            "Steps marked `(on-demand)` must be inspected with `mcpmate_ucan_details`, ",
            "then invoked with `mcpmate_ucan_call`.\n\n",
            "- `search-release-logs` (on-demand): Inspect first.\n",
            "- `search-release-logs` (on-demand): Then summarize."
        )
    );
}

#[test]
fn projects_multiple_capabilities_as_a_markdown_list() {
    let markdown = concat!(
        "# Investigate\n\n",
        ":::capability {\"name\":\"open-page\",\"exposure\":\"direct\"}\n",
        "Open the target URL.\n",
        ":::\n\n",
        ":::capability {\"name\":\"capture-shot\",\"exposure\":\"direct\"}\n",
        "Capture the screenshot.\n",
        ":::\n"
    );
    let guide = parse_workflow_guide(markdown).expect("valid Guide");
    let rendered = render_workflow_skill(markdown, &guide, &effective_exposure_by_name(&guide.capabilities));

    assert_eq!(
        rendered.markdown,
        concat!(
            "# Investigate\n\n",
            "The following steps are MCP server capability invocations. ",
            "Steps marked `(direct)` can be invoked by tool name.\n\n",
            "- `open-page` (direct): Open the target URL.\n",
            "- `capture-shot` (direct): Capture the screenshot."
        )
    );
}

#[tokio::test]
async fn initializes_a_new_workflow_profile_with_a_readable_guide() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("open in-memory database");
    database_support::prepare_config(&pool).await;
    sqlx::query(
        "INSERT INTO profile (id, name, description, type, role, profile_mode)
             VALUES ('workflow-profile', 'Release investigation', '', 'shared', 'user', 'workflow')",
    )
    .execute(&pool)
    .await
    .expect("insert Workflow Profile");

    let view = WorkflowGuideService::new(pool)
        .view("workflow-profile")
        .await
        .expect("view Guide");

    assert_eq!(view.guide_revision, 0);
    assert_eq!(view.markdown, "# Release investigation");
    assert!(view.capabilities.is_empty());
    assert!(view.package_files.is_empty());
}

#[tokio::test]
async fn guide_package_paths_reserve_legacy_material_paths() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("open in-memory database");
    database_support::prepare_config(&pool).await;
    sqlx::query(
        "INSERT INTO profile (id, name, description, type, role, profile_mode)
             VALUES ('workflow-profile', 'Workflow', '', 'shared', 'user', 'workflow')",
    )
    .execute(&pool)
    .await
    .expect("insert Workflow Profile");
    sqlx::query(
        "INSERT INTO workflow_profile_materials (
                material_id, profile_id, ordinal, title, kind, relative_path
             ) VALUES ('legacy-material', 'workflow-profile', 0, 'Policy', 'uploaded_file', 'references/policy.md')",
    )
    .execute(&pool)
    .await
    .expect("reserve legacy Material path");
    let mut transaction = pool.begin().await.expect("begin allocation transaction");

    let allocated = allocate_package_path(
        &mut transaction,
        "workflow-profile",
        WorkflowGuidePackageCategory::Reference,
        "Policy",
        "md",
    )
    .await
    .expect("allocate Guide package path");

    assert_eq!(allocated, "references/policy-2.md");
}

#[tokio::test]
async fn projects_a_guide_atomically_without_internal_identifiers() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("open in-memory database");
    database_support::prepare_config(&pool).await;
    sqlx::query(
            "INSERT INTO profile (id, name, description, type, role, profile_mode)
             VALUES ('workflow-profile', 'Release investigation', 'Investigate production release regressions.', 'shared', 'user', 'workflow')",
        )
        .execute(&pool)
        .await
        .expect("insert Workflow Profile");
    sqlx::query(
            "INSERT INTO workflow_profile_skills (profile_id, skill_name) VALUES ('workflow-profile', 'release-investigation-guide')",
        )
        .execute(&pool)
        .await
        .expect("configure friendly Skill name");
    let service = WorkflowGuideService::new(pool.clone());
    service.view("workflow-profile").await.expect("initialize Guide");
    sqlx::query(
        "UPDATE workflow_profile_guides
             SET markdown = '# Release investigation\n\nUse this Guide to investigate a regression.'
             WHERE profile_id = 'workflow-profile'",
    )
    .execute(&pool)
    .await
    .expect("write Guide fixture");
    let temporary = tempfile::tempdir().expect("create skills directory");

    let projected = service
        .project("workflow-profile", temporary.path().to_path_buf())
        .await
        .expect("project Guide");

    assert!(projected.markdown.contains("name: release-investigation-guide"));
    assert!(projected.markdown.contains("# Release investigation"));
    assert!(!projected.markdown.contains("workflow-profile"));
    let skill = std::fs::read_to_string(temporary.path().join("release-investigation-guide/SKILL.md"))
        .expect("read projected Skill");
    assert_eq!(skill, projected.markdown);
    let skill_path = temporary.path().join("release-investigation-guide/SKILL.md");
    std::fs::remove_file(&skill_path).expect("remove stale projected Skill");
    let repaired = service
        .project("workflow-profile", temporary.path().to_path_buf())
        .await
        .expect("repair missing projected Skill");
    assert_eq!(
        std::fs::read_to_string(&skill_path).expect("read repaired Skill"),
        repaired.markdown
    );
    assert_eq!(repaired.markdown, projected.markdown);
    let asset_bytes = b"registered asset";
    let asset_path = temporary.path().join("release-investigation-guide/assets/evidence.bin");
    std::fs::write(&asset_path, asset_bytes).expect("write registered asset");
    sqlx::query(
        "INSERT INTO workflow_profile_package_files (
                package_file_id, profile_id, ordinal, title, category, relative_path,
                extension, file_size, checksum
             ) VALUES ('asset-file', 'workflow-profile', 0, 'Evidence', 'asset',
                'assets/evidence.bin', 'bin', ?, ?)",
    )
    .bind(asset_bytes.len() as i64)
    .bind(format!("{:x}", Sha256::digest(asset_bytes)))
    .execute(&pool)
    .await
    .expect("register asset");
    service
        .project("workflow-profile", temporary.path().to_path_buf())
        .await
        .expect("repair preserves a registered non-Markdown file");
    assert_eq!(std::fs::read(&asset_path).expect("read preserved asset"), asset_bytes);

    let materials = WorkflowMaterialsService::new(pool.clone(), temporary.path().to_path_buf());
    let residual_lease = materials
        .stage_package_file_deletion_lease("release-investigation-guide", "assets/evidence.bin")
        .await
        .expect("simulate an interrupted package-file deletion");
    drop(residual_lease);
    assert!(!asset_path.exists());
    service
        .project("workflow-profile", temporary.path().to_path_buf())
        .await
        .expect("repair restores a registered package-file deletion lease before checksum verification");
    assert_eq!(std::fs::read(&asset_path).expect("read restored asset"), asset_bytes);

    let interrupted_replacement = materials
        .stage_package_file_bytes(
            "release-investigation-guide",
            "assets/evidence.bin",
            b"uncommitted replacement",
        )
        .await
        .expect("simulate an interrupted package-file replacement");
    drop(interrupted_replacement);
    assert_eq!(
        std::fs::read(&asset_path).expect("read uncommitted replacement"),
        b"uncommitted replacement"
    );
    service
        .project("workflow-profile", temporary.path().to_path_buf())
        .await
        .expect("repair restores a registered package-file backup before checksum verification");
    assert_eq!(std::fs::read(&asset_path).expect("read recovered asset"), asset_bytes);

    std::fs::remove_file(&asset_path).expect("remove registered asset");
    let error = service
        .project("workflow-profile", temporary.path().to_path_buf())
        .await
        .expect_err("repair diagnoses a missing registered non-Markdown file");
    assert!(error.to_string().contains("cannot be repaired"));
    let fingerprint: Option<String> = sqlx::query_scalar(
        "SELECT input_fingerprint FROM workflow_profile_skill_projections WHERE profile_id = 'workflow-profile'",
    )
    .fetch_one(&pool)
    .await
    .expect("load projection fingerprint");
    assert!(fingerprint.is_some());
}

#[tokio::test]
async fn repair_preserves_files_owned_by_legacy_material_rows() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("open in-memory database");
    database_support::prepare_config(&pool).await;
    sqlx::query(
        "INSERT INTO profile (id, name, description, type, role, profile_mode)
             VALUES ('workflow-profile', 'Release investigation', '', 'shared', 'user', 'workflow')",
    )
    .execute(&pool)
    .await
    .expect("insert Workflow Profile");
    sqlx::query(
        "INSERT INTO workflow_profile_skills (profile_id, skill_name)
             VALUES ('workflow-profile', 'release-investigation-guide')",
    )
    .execute(&pool)
    .await
    .expect("configure friendly Skill name");
    let service = WorkflowGuideService::new(pool.clone());
    service.view("workflow-profile").await.expect("initialize Guide");
    let temporary = tempfile::tempdir().expect("create skills directory");
    service
        .project("workflow-profile", temporary.path().to_path_buf())
        .await
        .expect("create Skill projection");

    let legacy_path = temporary
        .path()
        .join("release-investigation-guide/references/legacy-notes.md");
    std::fs::write(&legacy_path, "# Legacy notes\n").expect("write legacy Material file");
    sqlx::query(
        "INSERT INTO workflow_profile_materials (
                material_id, profile_id, ordinal, title, kind, relative_path
             ) VALUES (
                'legacy-material', 'workflow-profile', 0, 'Legacy notes',
                'uploaded_file', 'references/legacy-notes.md'
             )",
    )
    .execute(&pool)
    .await
    .expect("register legacy Material file");

    service
        .project("workflow-profile", temporary.path().to_path_buf())
        .await
        .expect("repair Skill projection");

    assert_eq!(
        std::fs::read_to_string(legacy_path).expect("read preserved legacy Material file"),
        "# Legacy notes\n"
    );
}

#[tokio::test]
async fn projection_fingerprint_tracks_package_file_revisions() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("open in-memory database");
    database_support::prepare_config(&pool).await;
    sqlx::query(
        "INSERT INTO profile (id, name, description, type, role, profile_mode)
             VALUES ('workflow-profile', 'Release investigation', '', 'shared', 'user', 'workflow')",
    )
    .execute(&pool)
    .await
    .expect("insert Workflow Profile");
    sqlx::query(
            "INSERT INTO workflow_profile_skills (profile_id, skill_name) VALUES ('workflow-profile', 'release-investigation-guide')",
        )
        .execute(&pool)
        .await
        .expect("configure friendly Skill name");
    sqlx::query(
        "INSERT INTO workflow_profile_package_files (
                package_file_id, profile_id, ordinal, title, category, relative_path, file_revision, checksum
             ) VALUES (
                'package-file', 'workflow-profile', 0, 'Release policy', 'reference',
                'references/release-policy.md', 0, 'first-checksum'
             )",
    )
    .execute(&pool)
    .await
    .expect("insert package file");
    sqlx::query(
        "INSERT INTO workflow_profile_external_guides (package_file_id, profile_id, markdown)
             VALUES ('package-file', 'workflow-profile', '# Release policy')",
    )
    .execute(&pool)
    .await
    .expect("register reconstructable external Guide source");
    let service = WorkflowGuideService::new(pool.clone());
    service.view("workflow-profile").await.expect("initialize Guide");
    let temporary = tempfile::tempdir().expect("create skills directory");

    service
        .project("workflow-profile", temporary.path().to_path_buf())
        .await
        .expect("project first package revision");
    let first: String = sqlx::query_scalar(
        "SELECT input_fingerprint FROM workflow_profile_skill_projections WHERE profile_id = 'workflow-profile'",
    )
    .fetch_one(&pool)
    .await
    .expect("load first fingerprint");
    sqlx::query(
        "UPDATE workflow_profile_package_files
             SET file_revision = 1, checksum = 'second-checksum'
             WHERE package_file_id = 'package-file'",
    )
    .execute(&pool)
    .await
    .expect("change package file revision");

    service
        .project("workflow-profile", temporary.path().to_path_buf())
        .await
        .expect("project changed package revision");
    let second: String = sqlx::query_scalar(
        "SELECT input_fingerprint FROM workflow_profile_skill_projections WHERE profile_id = 'workflow-profile'",
    )
    .fetch_one(&pool)
    .await
    .expect("load second fingerprint");
    assert_ne!(first, second);
}

#[tokio::test]
async fn manages_category_validated_package_files_with_the_skill_projection() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("open in-memory database");
    database_support::prepare_config(&pool).await;
    sqlx::query(
        "INSERT INTO profile (id, name, description, type, role, profile_mode)
             VALUES ('workflow-profile', 'Release investigation', '', 'shared', 'user', 'workflow')",
    )
    .execute(&pool)
    .await
    .expect("insert Workflow Profile");
    let service = WorkflowGuideService::new(pool);
    service.view("workflow-profile").await.expect("initialize Guide");
    let temporary = tempfile::tempdir().expect("create skills directory");

    let saved = service
        .save_package_file_and_project(
            WorkflowGuidePackageFileSaveCommand {
                profile_id: "workflow-profile".to_string(),
                package_file_id: None,
                expected_file_revision: None,
                expected_guide_revision: Some(0),
                title: "Release policy".to_string(),
                category: WorkflowGuidePackageCategory::Reference,
                original_filename: "release-policy.md".to_string(),
                bytes: b"# Release policy\n".to_vec(),
                reclamation_confirmation: None,
            },
            temporary.path().to_path_buf(),
        )
        .await
        .expect("save package file");
    let file = saved.guide.package_files.first().expect("saved package file").clone();
    assert_eq!(file.relative_path, "references/release-policy.md");
    assert_eq!(file.mime_type.as_deref(), Some("text/markdown"));
    assert!(!saved.projected_skill.markdown.contains(&file.package_file_id));
    assert_eq!(
        std::fs::read(
            temporary
                .path()
                .join("workflow-workflow-profile")
                .join(&file.relative_path)
        )
        .expect("read package file"),
        b"# Release policy\n"
    );

    let linked = service
        .save_and_project(
            WorkflowGuideSaveCommand {
                profile_id: "workflow-profile".to_string(),
                expected_guide_revision: saved.guide.guide_revision,
                markdown: format!("# Release investigation\n\n[{}]({})\n", file.title, file.relative_path),
                reclamation_confirmation: None,
            },
            temporary.path().to_path_buf(),
        )
        .await
        .expect("link package file");
    let error = service
        .save_and_project(
            WorkflowGuideSaveCommand {
                profile_id: "workflow-profile".to_string(),
                expected_guide_revision: linked.guide.guide_revision,
                markdown: "# Release investigation\n".to_string(),
                reclamation_confirmation: None,
            },
            temporary.path().to_path_buf(),
        )
        .await
        .expect_err("newly unreachable package file requires confirmation");
    let WorkflowGuideError::ReclamationConfirmationRequired(plan) = error else {
        panic!("expected reclamation confirmation requirement");
    };
    assert_eq!(plan.package_files, vec![file.clone()]);
    assert_eq!(
        service
            .view("workflow-profile")
            .await
            .expect("reload unchanged Guide")
            .guide_revision,
        linked.guide.guide_revision
    );
    let reclaimed = service
        .save_and_project(
            WorkflowGuideSaveCommand {
                profile_id: "workflow-profile".to_string(),
                expected_guide_revision: linked.guide.guide_revision,
                markdown: "# Release investigation\n".to_string(),
                reclamation_confirmation: Some(WorkflowGuideReclamationConfirmation {
                    package_files: vec![WorkflowGuidePackageFileRevision {
                        package_file_id: file.package_file_id,
                        file_revision: file.file_revision,
                    }],
                    capability_names: Vec::new(),
                }),
            },
            temporary.path().to_path_buf(),
        )
        .await
        .expect("save after exact reclamation confirmation");
    assert!(reclaimed.guide.package_files.is_empty());
    assert!(
        !temporary
            .path()
            .join("workflow-workflow-profile")
            .join(file.relative_path)
            .exists(),
        "confirmed reclamation moves the projected package file out of the Skill package"
    );
}

#[tokio::test]
async fn reads_and_previews_external_markdown_without_persisting_the_draft() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("open in-memory database");
    database_support::prepare_config(&pool).await;
    sqlx::query(
            "INSERT INTO profile (id, name, description, type, role, profile_mode)
             VALUES ('workflow-profile', 'Release investigation', 'Investigate releases.', 'shared', 'user', 'workflow')",
        )
        .execute(&pool)
        .await
        .expect("insert Workflow Profile");
    sqlx::query(
        "INSERT INTO workflow_profile_skills (profile_id, skill_name)
             VALUES ('workflow-profile', 'release-investigation-guide')",
    )
    .execute(&pool)
    .await
    .expect("configure friendly Skill name");
    let service = WorkflowGuideService::new(pool.clone());
    service.view("workflow-profile").await.expect("initialize Guide");
    let temporary = tempfile::tempdir().expect("create skills directory");
    let saved = service
        .save_package_file_and_project(
            WorkflowGuidePackageFileSaveCommand {
                profile_id: "workflow-profile".to_string(),
                package_file_id: None,
                expected_file_revision: None,
                expected_guide_revision: Some(0),
                title: "Release policy".to_string(),
                category: WorkflowGuidePackageCategory::Reference,
                original_filename: "release-policy.md".to_string(),
                bytes: b"# Release policy\nDraft policy body.\n".to_vec(),
                reclamation_confirmation: None,
            },
            temporary.path().to_path_buf(),
        )
        .await
        .expect("save external Markdown document");
    let file = saved.guide.package_files.first().expect("package file").clone();
    let package_service = WorkflowMaterialsService::new(pool.clone(), temporary.path().to_path_buf());
    let package_guard = package_service
        .lock_skill_package("release-investigation-guide")
        .await
        .expect("hold package lock while opening external document");
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            service.read_external_document(
                "workflow-profile",
                &file.package_file_id,
                temporary.path().to_path_buf(),
            ),
        )
        .await
        .is_err(),
        "external document reads must wait for a package writer"
    );
    drop(package_guard);
    service
        .read_external_document(
            "workflow-profile",
            &file.package_file_id,
            temporary.path().to_path_buf(),
        )
        .await
        .expect("read resumes after package writer releases the lock");
    let document = service
        .read_external_document(
            "workflow-profile",
            &file.package_file_id,
            temporary.path().to_path_buf(),
        )
        .await
        .expect("read external Markdown document");
    assert_eq!(document.relative_path, "references/release-policy.md");
    assert_eq!(document.markdown, "# Release policy\nDraft policy body.\n");

    let with_asset = service
        .save_package_file_and_project(
            WorkflowGuidePackageFileSaveCommand {
                profile_id: "workflow-profile".to_string(),
                package_file_id: None,
                expected_file_revision: None,
                expected_guide_revision: None,
                title: "Policy diagram".to_string(),
                category: WorkflowGuidePackageCategory::Asset,
                original_filename: "policy-diagram.pdf".to_string(),
                bytes: b"diagram".to_vec(),
                reclamation_confirmation: None,
            },
            temporary.path().to_path_buf(),
        )
        .await
        .expect("save asset referenced by external Markdown");
    let asset = with_asset
        .guide
        .package_files
        .iter()
        .find(|candidate| candidate.category == WorkflowGuidePackageCategory::Asset)
        .expect("saved asset")
        .clone();

    let linked = service
        .save_and_project(
            WorkflowGuideSaveCommand {
                profile_id: "workflow-profile".to_string(),
                expected_guide_revision: saved.guide.guide_revision,
                markdown: format!("# Release investigation\n\n[{}]({})\n", file.title, file.relative_path),
                reclamation_confirmation: None,
            },
            temporary.path().to_path_buf(),
        )
        .await
        .expect("link external Markdown from the root Guide");

    let updated = service
        .save_package_file_and_project(
            WorkflowGuidePackageFileSaveCommand {
                profile_id: "workflow-profile".to_string(),
                package_file_id: Some(file.package_file_id.clone()),
                expected_file_revision: Some(file.file_revision),
                expected_guide_revision: Some(linked.guide.guide_revision),
                title: file.title.clone(),
                category: WorkflowGuidePackageCategory::Reference,
                original_filename: "release-policy.md".to_string(),
                bytes: format!(
                    "# Release policy\nCurrent policy body.\n\n[Policy diagram]({})\n",
                    asset.relative_path
                )
                .into_bytes(),
                reclamation_confirmation: None,
            },
            temporary.path().to_path_buf(),
        )
        .await
        .expect("save current external Markdown revision");
    let updated_file = updated
        .guide
        .package_files
        .iter()
        .find(|candidate| candidate.package_file_id == file.package_file_id)
        .expect("updated package file")
        .clone();
    let error = service
        .save_package_file_and_project(
            WorkflowGuidePackageFileSaveCommand {
                profile_id: "workflow-profile".to_string(),
                package_file_id: Some(updated_file.package_file_id.clone()),
                expected_file_revision: Some(updated_file.file_revision),
                expected_guide_revision: Some(linked.guide.guide_revision),
                title: updated_file.title.clone(),
                category: WorkflowGuidePackageCategory::Reference,
                original_filename: "release-policy.md".to_string(),
                bytes: b"# Release policy\nStale policy body.\n".to_vec(),
                reclamation_confirmation: None,
            },
            temporary.path().to_path_buf(),
        )
        .await
        .expect_err("stale external Markdown Guide revision must conflict");
    assert!(matches!(
        error,
        WorkflowGuideError::GuideChanged {
            current_guide_revision: 2
        }
    ));

    let before_revision: i64 =
        sqlx::query_scalar("SELECT guide_revision FROM workflow_profile_guides WHERE profile_id = 'workflow-profile'")
            .fetch_one(&pool)
            .await
            .expect("read revision before preview");
    let preview = service
        .preview(WorkflowGuidePreviewCommand {
            profile_id: "workflow-profile".to_string(),
            relative_path: Some(document.relative_path.clone()),
            markdown: "# Release policy\nUnsaved preview body.\n".to_string(),
        })
        .await
        .expect("preview external Markdown draft");
    assert_eq!(
        preview.active_document.markdown,
        "# Release policy\nUnsaved preview body."
    );
    assert!(
        preview
            .projected_skill
            .markdown
            .contains("name: release-investigation-guide")
    );
    assert!(preview.projected_skill.markdown.contains("# Release investigation"));
    assert_eq!(
        preview
            .orphaned_package_files
            .iter()
            .map(|file| file.relative_path.as_str())
            .collect::<Vec<_>>(),
        vec![asset.relative_path.as_str()]
    );
    let after_revision: i64 =
        sqlx::query_scalar("SELECT guide_revision FROM workflow_profile_guides WHERE profile_id = 'workflow-profile'")
            .fetch_one(&pool)
            .await
            .expect("read revision after preview");
    assert_eq!(after_revision, before_revision);
    assert_eq!(
        std::fs::read_to_string(
            temporary
                .path()
                .join("release-investigation-guide/references/release-policy.md"),
        )
        .expect("read persisted external Markdown"),
        format!(
            "# Release policy\nCurrent policy body.\n\n[Policy diagram]({})\n",
            asset.relative_path
        )
    );
    std::fs::write(
        temporary
            .path()
            .join("release-investigation-guide/references/release-policy.md"),
        "# Release policy\nUnexpected replacement.\n",
    )
    .expect("replace external Markdown outside the managed save path");
    let error = service
        .read_external_document(
            "workflow-profile",
            &file.package_file_id,
            temporary.path().to_path_buf(),
        )
        .await
        .expect_err("checksum mismatch must reject an unregistered file replacement");
    assert!(error.to_string().contains("registered checksum"));

    let external_path = temporary
        .path()
        .join("release-investigation-guide/references/release-policy.md");
    std::fs::remove_file(&external_path).expect("remove stale external projection");
    service
        .project("workflow-profile", temporary.path().to_path_buf())
        .await
        .expect("repair missing external Markdown projection");
    assert_eq!(
        std::fs::read_to_string(external_path).expect("read repaired external Markdown"),
        format!(
            "# Release policy\nCurrent policy body.\n\n[Policy diagram]({})\n",
            asset.relative_path
        )
    );

    let remove_asset = WorkflowGuidePackageFileSaveCommand {
        profile_id: "workflow-profile".to_string(),
        package_file_id: Some(updated_file.package_file_id.clone()),
        expected_file_revision: Some(updated_file.file_revision),
        expected_guide_revision: Some(updated.guide.guide_revision),
        title: updated_file.title.clone(),
        category: WorkflowGuidePackageCategory::Reference,
        original_filename: "release-policy.md".to_string(),
        bytes: b"# Release policy\nNo diagram is required.\n".to_vec(),
        reclamation_confirmation: None,
    };
    let error = service
        .save_package_file_and_project(remove_asset.clone(), temporary.path().to_path_buf())
        .await
        .expect_err("reachable external Markdown reclamation requires confirmation");
    let WorkflowGuideError::ReclamationConfirmationRequired(plan) = error else {
        panic!("expected external Markdown reclamation confirmation requirement");
    };
    assert_eq!(plan.package_files, vec![asset.clone()]);
    let reclaimed = service
        .save_package_file_and_project(
            WorkflowGuidePackageFileSaveCommand {
                reclamation_confirmation: Some(WorkflowGuideReclamationConfirmation {
                    package_files: vec![WorkflowGuidePackageFileRevision {
                        package_file_id: asset.package_file_id,
                        file_revision: asset.file_revision,
                    }],
                    capability_names: Vec::new(),
                }),
                ..remove_asset
            },
            temporary.path().to_path_buf(),
        )
        .await
        .expect("save external Markdown after exact reclamation confirmation");
    assert!(
        reclaimed
            .guide
            .package_files
            .iter()
            .all(|candidate| candidate.relative_path != asset.relative_path)
    );
    assert!(
        !temporary
            .path()
            .join("release-investigation-guide")
            .join(&asset.relative_path)
            .exists(),
        "external Markdown reclamation moves the orphaned asset out of the Skill package"
    );
    assert_eq!(
        std::fs::read_to_string(
            temporary
                .path()
                .join("release-investigation-guide")
                .join(&updated_file.relative_path),
        )
        .expect("read updated external Markdown after child reclamation"),
        "# Release policy\nNo diagram is required.\n",
        "saving the parent document must not reclaim or skip its replacement"
    );
}

#[test]
fn rejects_package_file_extensions_outside_the_selected_category() {
    let error = validate_package_file_command(&WorkflowGuidePackageFileSaveCommand {
        profile_id: "workflow-profile".to_string(),
        package_file_id: None,
        expected_file_revision: None,
        expected_guide_revision: None,
        title: "Release policy".to_string(),
        category: WorkflowGuidePackageCategory::Script,
        original_filename: "release-policy.md".to_string(),
        bytes: b"# Release policy\n".to_vec(),
        reclamation_confirmation: None,
    })
    .expect_err("markdown is not a script file");
    assert!(error.to_string().contains("not allowed for script"));

    for (category, filename) in [
        (WorkflowGuidePackageCategory::Script, "setup.sh"),
        (WorkflowGuidePackageCategory::Script, "run.bat"),
        (WorkflowGuidePackageCategory::Asset, "diagram.png"),
        (WorkflowGuidePackageCategory::Asset, "rows.csv"),
        (WorkflowGuidePackageCategory::Asset, "schema.sql"),
    ] {
        validate_package_file_command(&WorkflowGuidePackageFileSaveCommand {
            profile_id: "workflow-profile".to_string(),
            package_file_id: None,
            expected_file_revision: None,
            expected_guide_revision: None,
            title: "Allowed package file".to_string(),
            category,
            original_filename: filename.to_string(),
            bytes: b"sample".to_vec(),
            reclamation_confirmation: None,
        })
        .unwrap_or_else(|_| panic!("{filename} should be allowed"));
    }
}

#[tokio::test]
async fn saves_plain_guide_without_fabricating_workflow_steps() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("open in-memory database");
    database_support::prepare_config(&pool).await;
    sqlx::query(
        "INSERT INTO profile (id, name, description, type, role, profile_mode)
             VALUES ('workflow-profile', 'Release investigation', '', 'shared', 'user', 'workflow')",
    )
    .execute(&pool)
    .await
    .expect("insert Workflow Profile");
    sqlx::query(
            "INSERT INTO workflow_profile_skills (profile_id, skill_name) VALUES ('workflow-profile', 'release-investigation-guide')",
        )
        .execute(&pool)
        .await
        .expect("configure friendly Skill name");
    let service = WorkflowGuideService::new(pool.clone());
    service.view("workflow-profile").await.expect("initialize Guide");

    let saved = service
        .save(WorkflowGuideSaveCommand {
            profile_id: "workflow-profile".to_string(),
            expected_guide_revision: 0,
            markdown: "# Release investigation\n\nRead the release logs before making a conclusion.".to_string(),
            reclamation_confirmation: None,
        })
        .await
        .expect("save Guide");

    assert_eq!(saved.guide_revision, 1);
    let step_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM workflow_profile_steps WHERE profile_id = 'workflow-profile'")
            .fetch_one(&pool)
            .await
            .expect("count Workflow steps");
    assert_eq!(step_count, 0);
}

#[tokio::test]
async fn saves_reachable_package_reference_without_fabricating_steps() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("open in-memory database");
    database_support::prepare_config(&pool).await;
    sqlx::query(
        "INSERT INTO profile (id, name, description, type, role, profile_mode)
             VALUES ('workflow-profile', 'Release investigation', '', 'shared', 'user', 'workflow')",
    )
    .execute(&pool)
    .await
    .expect("insert Workflow Profile");
    sqlx::query(
            "INSERT INTO workflow_profile_package_files (
                package_file_id, profile_id, ordinal, title, category, relative_path
             ) VALUES ('package-file', 'workflow-profile', 0, 'Release policy', 'reference', 'references/release-policy.md')",
        )
        .execute(&pool)
        .await
        .expect("insert package file");
    let service = WorkflowGuideService::new(pool.clone());
    service.view("workflow-profile").await.expect("initialize Guide");

    service
        .save(WorkflowGuideSaveCommand {
            profile_id: "workflow-profile".to_string(),
            expected_guide_revision: 0,
            markdown: "# Release investigation\n\nRead [Release policy](references/release-policy.md).".to_string(),
            reclamation_confirmation: None,
        })
        .await
        .expect("save Guide");

    let step_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM workflow_profile_steps WHERE profile_id = 'workflow-profile'")
            .fetch_one(&pool)
            .await
            .expect("count Workflow steps");
    assert_eq!(step_count, 0);
}

#[tokio::test]
async fn saves_and_projects_through_one_coordinated_operation() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("open in-memory database");
    database_support::prepare_config(&pool).await;
    sqlx::query(
        "INSERT INTO profile (id, name, description, type, role, profile_mode)
             VALUES ('workflow-profile', 'Release investigation', '', 'shared', 'user', 'workflow')",
    )
    .execute(&pool)
    .await
    .expect("insert Workflow Profile");
    sqlx::query(
            "INSERT INTO workflow_profile_skills (profile_id, skill_name) VALUES ('workflow-profile', 'release-investigation-guide')",
        )
        .execute(&pool)
        .await
        .expect("configure friendly Skill name");
    let service = WorkflowGuideService::new(pool.clone());
    service.view("workflow-profile").await.expect("initialize Guide");
    let temporary = tempfile::tempdir().expect("create skills directory");

    let saved = service
        .save_and_project(
            WorkflowGuideSaveCommand {
                profile_id: "workflow-profile".to_string(),
                expected_guide_revision: 0,
                markdown: "# Release investigation\n\nRead the logs.".to_string(),
                reclamation_confirmation: None,
            },
            temporary.path().to_path_buf(),
        )
        .await
        .expect("save and project Guide");

    assert_eq!(saved.guide.guide_revision, 1);
    assert!(saved.projected_skill.markdown.contains("Read the logs."));
    let skill_path = temporary.path().join("release-investigation-guide/SKILL.md");
    assert_eq!(
        std::fs::read_to_string(skill_path).expect("read Skill"),
        saved.projected_skill.markdown
    );
}

#[test]
fn skill_front_matter_preserves_the_configured_identity_and_validates_yaml() {
    let skill = format_skill_definition(
        "release-investigation-guide",
        "Release investigation",
        "Investigate: \"production\"\nwith care.",
        Some("Requires Playwright via MCPMate."),
        "# Release investigation",
    );

    let front_matter = skill
        .strip_prefix("---\n")
        .and_then(|value| value.split_once("---\n\n"))
        .map(|(front_matter, _)| front_matter)
        .expect("extract front matter");
    assert!(
        front_matter.starts_with("name: release-investigation-guide\n"),
        "name must precede description in front matter"
    );
    let metadata: BTreeMap<String, String> = serde_yaml::from_str(front_matter).expect("parse YAML front matter");

    assert_eq!(metadata.get("name"), Some(&"release-investigation-guide".to_string()));
    assert_eq!(
        metadata.get("description"),
        Some(&"Investigate: \"production\"\nwith care.".to_string())
    );
    assert_eq!(
        metadata.get("compatibility"),
        Some(&"Requires Playwright via MCPMate.".to_string())
    );
}
