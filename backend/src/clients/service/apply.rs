use super::core::{ApplyOutcome, ClientConfigService, ClientRenderOptions, ClientRenderResult, PreviewOutcome};
use crate::clients::TemplateExecutionResult;
use crate::clients::document::parse_config;
use crate::clients::engine::RenderRequest;
use crate::clients::error::{ConfigError, ConfigResult};
use crate::clients::models::{
    CONFIG_TRANSPORT_PRIORITY, ClientConfigFileParse, ConfigMode, ContainerType, FormatRule, MergeStrategy,
    TemplateFormat,
};
use std::collections::HashMap;
use std::collections::HashSet;

fn available_managed_transports(transports: Option<&HashMap<String, FormatRule>>) -> Vec<&'static str> {
    let Some(rules) = transports else {
        return Vec::new();
    };

    CONFIG_TRANSPORT_PRIORITY
        .into_iter()
        .filter(|transport| rules.contains_key(*transport))
        .collect()
}

fn select_managed_transport(
    supported_transports: &[&'static str],
    selected_transport: Option<&'static str>,
) -> (Option<String>, bool) {
    if let Some(transport) = selected_transport {
        return (Some(transport.to_string()), false);
    }

    let chosen_transport = supported_transports.first().map(|transport| (*transport).to_string());
    let auto_selected = chosen_transport.is_some();

    (chosen_transport, auto_selected)
}

fn selected_managed_transport(transports: Option<&HashMap<String, FormatRule>>) -> Option<&'static str> {
    let rules = transports?;
    CONFIG_TRANSPORT_PRIORITY
        .into_iter()
        .find(|transport| rules.get(*transport).is_some_and(|rule| rule.selected == Some(true)))
}

impl ClientConfigService {
    /// Execute configuration generation (dry-run or apply)
    pub async fn execute_render(
        &self,
        options: ClientRenderOptions,
    ) -> ConfigResult<ClientRenderResult> {
        let system_override = self.default_merge_strategy_override().await?;
        self.execute_render_with_system_override(options, system_override).await
    }

    async fn execute_render_with_system_override(
        &self,
        options: ClientRenderOptions,
        system_override: Option<MergeStrategy>,
    ) -> ConfigResult<ClientRenderResult> {
        if !self.is_client_approved(&options.client_id).await? {
            return Err(ConfigError::ClientDisabled {
                identifier: options.client_id.clone(),
            });
        }

        if matches!(options.mode, ConfigMode::Managed | ConfigMode::Native)
            && self.verified_local_config_target(&options.client_id).await?.is_none()
        {
            return Err(ConfigError::DataAccessError(format!(
                "Client '{}' has no configured local config target; cannot render {:?} mode",
                options.client_id, options.mode
            )));
        }

        // Get client state for configuration metadata
        let state = self
            .fetch_state(&options.client_id)
            .await?
            .ok_or_else(|| ConfigError::DataAccessError(format!("Client state not found: {}", options.client_id)))?;
        let render_definition = Self::build_render_definition_from_state(&state, system_override)?;
        let transports = (!render_definition.config_mapping.format_rules.is_empty())
            .then(|| render_definition.config_mapping.format_rules.clone());

        // Validate transports exists for Managed mode
        if matches!(options.mode, ConfigMode::Managed) && transports.is_none() {
            return Err(ConfigError::DataAccessError(format!(
                "Client '{}' has no transports (likely legacy record); re-detect this client to populate transport configuration",
                options.client_id
            )));
        }

        // Select transport for managed mode if applicable
        let mut chosen_transport: Option<String> = None;
        let mut auto_selected = false;
        if matches!(options.mode, ConfigMode::Managed) {
            let supported_transports = available_managed_transports(transports.as_ref());
            let selected_transport = selected_managed_transport(transports.as_ref());
            (chosen_transport, auto_selected) = select_managed_transport(&supported_transports, selected_transport);
        }
        let mut servers = self.prepare_servers(&options).await?;
        let backup_policy = self.get_backup_policy(&options.client_id).await?;

        if matches!(options.mode, ConfigMode::Native) {
            if let Some(ref rules) = transports {
                let supported: HashSet<String> = rules.keys().map(|transport| transport.to_string()).collect();
                let before = servers.len();
                servers.retain(|server| supported.contains(&server.transport));
                if before != servers.len() {
                    tracing::debug!(
                        "Filtered unsupported servers for client {} (native mode): {} -> {}",
                        options.client_id,
                        before,
                        servers.len()
                    );
                }
            }
        }

        let mut warnings = Vec::new();
        let config_path = self.resolved_config_path(&options.client_id).await?.ok_or_else(|| {
            ConfigError::PathResolutionError(format!("No config_path for client {}", options.client_id))
        })?;
        let request = RenderRequest {
            client_id: &options.client_id,
            servers: &servers,
            mode: options.mode.clone(),
            definition: &render_definition,
            config_path: &config_path,
            profile_id: options.profile_id.as_deref(),
            dry_run: options.dry_run,
            backup_policy: &backup_policy,
            warnings: &mut warnings,
            preferred_transport: chosen_transport.clone(),
        };

        let execution = self.template_engine.render_config(request).await?;
        let target_path = self.resolved_config_path(&options.client_id).await?;

        Ok(ClientRenderResult {
            execution,
            target_path,
            servers,
            warnings,
            chosen_transport,
            auto_selected,
        })
    }

    fn preview_from_execution(exec: &TemplateExecutionResult) -> PreviewOutcome {
        match exec {
            TemplateExecutionResult::DryRun { diff, .. } => PreviewOutcome {
                format: diff.format,
                before: diff.before.clone(),
                after: diff.after.clone(),
                summary: diff.summary.clone(),
            },
            _ => PreviewOutcome {
                format: TemplateFormat::Json,
                before: None,
                after: None,
                summary: None,
            },
        }
    }

    pub async fn apply_or_preview(
        &self,
        options: ClientRenderOptions,
    ) -> ConfigResult<ApplyOutcome> {
        let system_override = self.default_merge_strategy_override().await?;
        self.apply_or_preview_with_system_override(options, system_override)
            .await
    }

    async fn apply_or_preview_with_system_override(
        &self,
        options: ClientRenderOptions,
        system_override: Option<MergeStrategy>,
    ) -> ConfigResult<ApplyOutcome> {
        let result = self
            .execute_render_with_system_override(options.clone(), system_override)
            .await?;
        let mut outcome = ApplyOutcome {
            preview: Self::preview_from_execution(&result.execution),
            warnings: result.warnings.clone(),
            ..Default::default()
        };
        self.finalize_apply_outcome(&options.client_id, &result.execution, &mut outcome)
            .await?;
        Ok(outcome)
    }

    pub async fn apply_with_deferred(
        &self,
        options: ClientRenderOptions,
    ) -> ConfigResult<ApplyOutcome> {
        let system_override = self.default_merge_strategy_override().await?;

        // Always compute a preview via dry-run first for stable diff/preview fields.
        let mut preview_opts = options.clone();
        preview_opts.dry_run = true;
        let preview_outcome = self
            .apply_or_preview_with_system_override(preview_opts, system_override)
            .await?;

        // Temporary: write-probe logging before actual apply
        // Helps diagnose which transports are being generated and whether 'args' exist
        if let Some(ref after) = preview_outcome.preview.after {
            self.log_preview_probe(&options.client_id, after).await;
        }

        // If the original request is a preview, return directly
        if options.dry_run {
            return Ok(preview_outcome);
        }

        // If not a preview: try to write
        let exec = self
            .execute_render_with_system_override(options.clone(), system_override)
            .await?;
        let mut out = preview_outcome;
        self.finalize_apply_outcome(&options.client_id, &exec.execution, &mut out)
            .await?;
        Ok(out)
    }

    async fn finalize_apply_outcome(
        &self,
        client_id: &str,
        execution: &TemplateExecutionResult,
        outcome: &mut ApplyOutcome,
    ) -> ConfigResult<()> {
        if let TemplateExecutionResult::Applied { backup_path, .. } = execution {
            let backup_policy = self.get_backup_policy(client_id).await?;
            self.enforce_backup_retention(client_id, &backup_policy).await?;
            self.mount_published_skill_packages(client_id).await?;
            outcome.applied = true;
            outcome.backup_path = backup_path.clone();
        }

        Ok(())
    }

    async fn log_preview_probe(
        &self,
        client_id: &str,
        rendered_config: &str,
    ) {
        let parse_rule = self.fetch_state(client_id).await.ok().flatten().and_then(|state| {
            match state.effective_config_file_parse() {
                Ok(rule) => rule,
                Err(err) => {
                    tracing::debug!(
                        target: "mcpmate::client::apply_probe",
                        client = %client_id,
                        error = %err,
                        "write-probe: invalid persisted config_file_parse"
                    );
                    None
                }
            }
        });

        let Some(summary) =
            parse_rule
                .as_ref()
                .and_then(|rule| match Self::summarize_servers_for_probe(rendered_config, rule) {
                    Ok(result) => result,
                    Err(err) => {
                        tracing::debug!(
                            target: "mcpmate::client::apply_probe",
                            client = %client_id,
                            error = %err,
                            "write-probe: failed to parse rendered config for summary"
                        );
                        None
                    }
                })
        else {
            tracing::debug!(
                target: "mcpmate::client::apply_probe",
                client = %client_id,
                "write-probe: unable to summarize servers"
            );
            return;
        };

        for entry in summary.into_iter().take(5) {
            tracing::debug!(
                target: "mcpmate::client::apply_probe",
                client = %client_id,
                name = %entry.name,
                transport = %entry.transport,
                has_args = entry.has_args,
                args_len = entry.args_len,
                has_url = entry.has_url,
                has_command = entry.has_command,
                "write-probe: server entry"
            );
        }
    }

    fn summarize_servers_for_probe(
        raw: &str,
        parse_rule: &ClientConfigFileParse,
    ) -> ConfigResult<Option<Vec<ProbeEntry>>> {
        use serde_json::Value;
        let doc = parse_config(raw, parse_rule)?.value;
        let Some(container) = parse_rule
            .container_keys
            .iter()
            .find_map(|key| crate::clients::utils::get_nested_value(&doc, key))
        else {
            return Ok(None);
        };
        let container = container.clone();

        let mut out: Vec<ProbeEntry> = Vec::new();
        match (parse_rule.container_type, container) {
            (ContainerType::Array, Value::Array(items)) => {
                for value in items {
                    let name = value
                        .get("name")
                        .and_then(|field| field.as_str())
                        .unwrap_or("")
                        .to_string();
                    out.push(Self::probe_entry(name, &value));
                }
            }
            (ContainerType::ObjectMap, Value::Object(map)) => {
                for (name, value) in map {
                    out.push(Self::probe_entry(name, &value));
                }
            }
            _ => {}
        }

        if out.is_empty() { Ok(None) } else { Ok(Some(out)) }
    }

    fn probe_entry(
        name: String,
        value: &serde_json::Value,
    ) -> ProbeEntry {
        let transport = value
            .get("type")
            .and_then(|field| field.as_str())
            .unwrap_or("")
            .to_string();
        let has_args = value.get("args").is_some();
        let args_len = value
            .get("args")
            .and_then(|field| field.as_array())
            .map(|args| args.len() as u32)
            .unwrap_or(0);
        let has_url = value.get("url").is_some() || value.get("baseUrl").is_some();
        let has_command = value.get("command").is_some();

        ProbeEntry {
            name,
            transport,
            has_args,
            args_len,
            has_url,
            has_command,
        }
    }
}

#[derive(Debug)]
struct ProbeEntry {
    name: String,
    transport: String,
    has_args: bool,
    args_len: u32,
    has_url: bool,
    has_command: bool,
}

#[cfg(test)]
mod tests {
    use super::{ClientConfigService, available_managed_transports, selected_managed_transport};
    use crate::clients::models::{ClientConfigFileParse, ContainerType, FormatRule, TemplateFormat};
    use std::collections::HashMap;

    #[test]
    fn available_managed_transports_returns_only_canonical_keys() {
        let mut transports = HashMap::new();
        transports.insert("http".to_string(), FormatRule::default());
        transports.insert("streamable_http".to_string(), FormatRule::default());
        transports.insert("stdio".to_string(), FormatRule::default());

        let available = available_managed_transports(Some(&transports));

        assert_eq!(available, vec!["streamable_http", "stdio"]);
    }

    #[test]
    fn selected_managed_transport_ignores_alias_keys() {
        let mut transports = HashMap::new();
        transports.insert(
            "http".to_string(),
            FormatRule {
                selected: Some(true),
                ..FormatRule::default()
            },
        );
        transports.insert(
            "streamable_http".to_string(),
            FormatRule {
                selected: Some(true),
                ..FormatRule::default()
            },
        );

        assert_eq!(selected_managed_transport(Some(&transports)), Some("streamable_http"));
    }

    #[test]
    fn probe_summary_declared_json_accepts_json5_style_config() {
        let parse_rule = ClientConfigFileParse {
            format: TemplateFormat::Json,
            container_type: ContainerType::ObjectMap,
            container_keys: vec!["context_servers".to_string()],
        };
        let entries = ClientConfigService::summarize_servers_for_probe(
            r#"{
                "context_servers": {
                    "MCPMate": {
                        "type": "stdio",
                        "command": "bridge",
                    },
                },
            }"#,
            &parse_rule,
        )
        .expect("probe summary should read JSON5-style declared JSON")
        .expect("probe summary should contain entries");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "MCPMate");
        assert_eq!(entries[0].transport, "stdio");
        assert!(entries[0].has_command);
    }
}
