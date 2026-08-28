use std::sync::Arc;

use axum::{
    Json,
    extract::{
        Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_stream::wrappers::BroadcastStream;

use crate::api::handlers::ApiError;
// use crate::api::models::inspector::InspectorMode;
use crate::api::models::inspector::{
    InspectorCallEventsQuery, InspectorListQuery, InspectorPromptGetData, InspectorPromptGetReq,
    InspectorPromptGetResp, InspectorPromptsListData, InspectorPromptsListResp, InspectorResourceReadData,
    InspectorResourceReadQuery, InspectorResourceReadResp, InspectorResourcesListData, InspectorResourcesListResp,
    InspectorSessionCloseData, InspectorSessionCloseReq, InspectorSessionCloseResp, InspectorSessionOpenReq,
    InspectorSessionOpenResp, InspectorSessionRefreshReq, InspectorSessionRefreshResp, InspectorTemplateReadData,
    InspectorTemplateReadReq, InspectorTemplateReadResp, InspectorTemplatesListData, InspectorTemplatesListResp,
    InspectorToolCallCancelData, InspectorToolCallCancelReq, InspectorToolCallCancelResp, InspectorToolCallData,
    InspectorToolCallReq, InspectorToolCallResp, InspectorToolCallStartData, InspectorToolCallStartResp,
    InspectorToolsListData, InspectorToolsListResp,
};
use crate::api::routes::AppState;
use crate::inspector::{calls::CallSubscription, service};

// ==============================
// Tools
// ==============================

pub async fn tools_list(
    State(state): State<Arc<AppState>>,
    Query(query): Query<InspectorListQuery>,
) -> Result<Json<InspectorToolsListResp>, ApiError> {
    let data_value = service::list_tools(&state, &query).await?;
    // Convert Value into strongly typed data
    let mode = data_value
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let tools: Vec<Value> = match data_value.get("tools").cloned() {
        Some(Value::Array(v)) => v,
        _ => Vec::new(),
    };
    let total = data_value
        .get("total")
        .and_then(|v| v.as_u64())
        .unwrap_or(tools.len() as u64) as usize;
    let meta = data_value.get("meta").and_then(|v| v.as_array()).cloned();
    let elapsed_ms = data_value
        .get("elapsed_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or_default();
    let data = InspectorToolsListData {
        mode,
        tools,
        total,
        meta,
        elapsed_ms,
    };
    Ok(Json(InspectorToolsListResp::success(data)))
}

pub async fn tool_call(
    State(state): State<Arc<AppState>>,
    Json(req): Json<InspectorToolCallReq>,
) -> Result<Json<InspectorToolCallResp>, ApiError> {
    // Preflight: When neither server_id nor server_name is provided, avoid hitting naming resolver
    if req.server_id.is_none() && req.server_name.is_none() {
        let resp = InspectorToolCallResp::error_simple("bad_request", "server_id or server_name is required");
        return Ok(Json(resp));
    }

    // Execute and normalize errors as 200 with structured error payload (Inspector UX contract)
    match service::call_tool(&state, &req).await {
        Ok(outcome) => {
            let data = InspectorToolCallData {
                message: outcome.message.unwrap_or_else(|| "completed".to_string()),
                result: outcome.result,
                server_id: outcome.server_id,
                elapsed_ms: Some(outcome.elapsed_ms),
            };
            Ok(Json(InspectorToolCallResp::success(data)))
        }
        Err(err) => {
            // Map handler error to client model without changing HTTP status
            let (code, message) = match &err {
                ApiError::NotFound(m) => ("not_found", m.as_str()),
                ApiError::BadRequest(m) => ("bad_request", m.as_str()),
                ApiError::InternalError(m) => ("internal_error", m.as_str()),
                ApiError::ServiceUnavailable(m) => ("service_unavailable", m.as_str()),
                ApiError::Conflict(m) => ("conflict", m.as_str()),
                ApiError::ProfileConflict(_) => ("conflict", "Profile authoring conflict"),
                ApiError::WorkflowGuideReclamationRequired(_) => {
                    ("conflict", "Workflow Guide save requires reclamation confirmation")
                }
                ApiError::WorkflowGuideTrashCleanupPending { .. } => {
                    ("internal_error", "Workflow Guide Trash cleanup is pending")
                }
                ApiError::InvalidProfileTarget(_) => ("invalid_target", "Profile target does not exist"),
                ApiError::Forbidden(m) => ("forbidden", m.as_str()),
                ApiError::Timeout(m) => ("timeout", m.as_str()),
                ApiError::GatewayTimeout(m) => ("gateway_timeout", m.as_str()),
                ApiError::Unauthorized(m) => ("unauthorized", m.as_str()),
                ApiError::BadGateway(m) => ("bad_gateway", m.as_str()),
            };
            let resp = InspectorToolCallResp::error_simple(code, message);
            Ok(Json(resp))
        }
    }
}

pub async fn tool_call_start(
    State(state): State<Arc<AppState>>,
    Json(req): Json<InspectorToolCallReq>,
) -> Result<Json<InspectorToolCallStartResp>, ApiError> {
    tracing::info!(
        tool = %req.tool,
        mode = ?req.mode,
        server_id = ?req.server_id,
        server_name = ?req.server_name,
        session_id = ?req.session_id,
        has_arguments = req.arguments.is_some(),
        "Inspector tool_call_start request received"
    );

    let info = service::start_tool_call(&state, &req).await?;

    tracing::info!(
        call_id = %info.call_id,
        "Inspector tool call started successfully"
    );

    let data = InspectorToolCallStartData {
        call_id: info.call_id,
        server_id: info.server_id,
        mode: info.mode,
        session_id: info.session_id,
        request_id: info.request_id,
        progress_token: info.progress_token,
    };
    Ok(Json(InspectorToolCallStartResp::success(data)))
}

pub async fn tool_call_events_ws(
    State(state): State<Arc<AppState>>,
    ws: WebSocketUpgrade,
    Query(query): Query<InspectorCallEventsQuery>,
) -> impl IntoResponse {
    tracing::info!(
        call_id = %query.call_id,
        "WebSocket connection request received for inspector call"
    );

    let call_id = query.call_id.clone();
    let subscription = state.inspector_calls.subscribe(&query.call_id).await;

    match subscription {
        Some(sub) => {
            tracing::info!(call_id = %call_id, "WebSocket subscription established");
            ws.on_upgrade(move |socket| handle_ws_events(socket, call_id, sub))
        }
        None => {
            tracing::warn!(
                call_id = %query.call_id,
                "Inspector call not found for WebSocket subscription"
            );
            ws.on_upgrade(move |mut socket| async move {
                let _ = socket.close().await;
            })
        }
    }
}

/// Handle WebSocket events for a specific call_id
async fn handle_ws_events(
    socket: WebSocket,
    call_id: String,
    subscription: crate::inspector::calls::CallSubscription,
) {
    use tokio::sync::broadcast;

    let (mut sender, _receiver) = socket.split();

    // Subscribe to broadcast channel based on subscription type
    let rx: broadcast::Receiver<crate::inspector::calls::InspectorEvent> = match subscription {
        CallSubscription::Active(rx) => rx,
        CallSubscription::Completed(event) => {
            // Send the completed event immediately and close
            if let Ok(json) = serde_json::to_string(&event) {
                let _ = sender.send(Message::Text(json.into())).await;
            }
            let _ = sender.close().await;
            return;
        }
    };

    // Use BroadcastStream to convert broadcast to Stream
    let mut stream = BroadcastStream::new(rx);

    // Send events from broadcast channel to WebSocket
    while let Some(result) = stream.next().await {
        match result {
            Ok(event) => {
                match serde_json::to_string(&event) {
                    Ok(json) => {
                        if sender.send(Message::Text(json.into())).await.is_err() {
                            // Client disconnected
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to serialize inspector event for WS");
                    }
                }
            }
            Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(_)) => {
                // Skip lagged messages
                continue;
            }
        }
    }

    tracing::info!(call_id = %call_id, "WebSocket connection closed");
}

pub async fn tool_call_cancel(
    State(state): State<Arc<AppState>>,
    Json(req): Json<InspectorToolCallCancelReq>,
) -> Result<Json<InspectorToolCallCancelResp>, ApiError> {
    state
        .inspector_calls
        .cancel_call(&req.call_id, req.reason.clone())
        .await
        .map_err(ApiError::BadRequest)?;

    Ok(Json(InspectorToolCallCancelResp::success(
        InspectorToolCallCancelData { cancelled: true },
    )))
}

pub async fn session_open(
    State(state): State<Arc<AppState>>,
    Json(req): Json<InspectorSessionOpenReq>,
) -> Result<Json<InspectorSessionOpenResp>, ApiError> {
    let data = service::open_session(&state, &req).await?;
    Ok(Json(InspectorSessionOpenResp::success(data)))
}

pub async fn session_refresh(
    State(state): State<Arc<AppState>>,
    Json(req): Json<InspectorSessionRefreshReq>,
) -> Result<Json<InspectorSessionRefreshResp>, ApiError> {
    let data = service::refresh_session(&state, &req).await?;
    Ok(Json(InspectorSessionRefreshResp::success(data)))
}

pub async fn session_close(
    State(state): State<Arc<AppState>>,
    Json(req): Json<InspectorSessionCloseReq>,
) -> Result<Json<InspectorSessionCloseResp>, ApiError> {
    let closed = service::close_session(&state, &req).await?;
    Ok(Json(InspectorSessionCloseResp::success(InspectorSessionCloseData {
        closed,
    })))
}

// ==============================
// Prompts
// ==============================

pub async fn prompts_list(
    State(state): State<Arc<AppState>>,
    Query(query): Query<InspectorListQuery>,
) -> Result<Json<InspectorPromptsListResp>, ApiError> {
    let data_value = service::list_prompts(&state, &query).await?;
    let mode = data_value
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let prompts: Vec<Value> = match data_value.get("prompts").cloned() {
        Some(Value::Array(v)) => v,
        _ => Vec::new(),
    };
    let total = data_value
        .get("total")
        .and_then(|v| v.as_u64())
        .unwrap_or(prompts.len() as u64) as usize;
    let meta = data_value.get("meta").and_then(|v| v.as_array()).cloned();
    let elapsed_ms = data_value
        .get("elapsed_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or_default();
    let data = InspectorPromptsListData {
        mode,
        prompts,
        total,
        meta,
        elapsed_ms,
    };
    Ok(Json(InspectorPromptsListResp::success(data)))
    // legacy code kept in VCS history
    // legacy code below kept for reference; will be removed after stabilization
    /*
    let refresh = if query.refresh {
        Some(crate::core::capability::runtime::RefreshStrategy::Force)
    } else {
        Some(crate::core::capability::runtime::RefreshStrategy::CacheFirst)
    };
    let mut items: Vec<rmcp::model::Prompt> = Vec::new();

    if query.server_id.is_some() || query.server_name.is_some() {
        let server_id = resolve_server(&query.server_id, &query.server_name).await?;
        let ctx = crate::core::capability::runtime::ListCtx {
            capability: crate::core::capability::CapabilityType::Prompts,
            server_id,
            refresh,
            timeout: Some(Duration::from_secs(10)),
            validation_session: None,
            runtime_identity: None,
            connection_selection: None,
            visibility_snapshot: None,
            name_domain: crate::core::capability::runtime::NameDomain::External,
        };
        let res = crate::core::capability::runtime::list(
            &ctx,
            &state.connection_pool,
            &state
                .database
                .as_ref()
                .ok_or(ApiError::InternalError("Database not available".into()))?
                .clone(),
        )
        .await
        .map_err(map_anyhow)?;
        items = res.items.into_prompts().unwrap_or_default();
    } else {
        let db = state
            .database
            .as_ref()
            .ok_or(ApiError::InternalError("Database not available".into()))?;
        let enabled_servers: Vec<(String, String)> = sqlx::query_as(
            r#"
            SELECT sc.id, sc.name
            FROM server_config sc
            WHERE sc.enabled = 1
              AND (
                EXISTS (
                  SELECT 1
                  FROM profile_server_relationships psr
                  JOIN profile p ON p.id = psr.profile_id
                  WHERE psr.server_id = sc.id
                    AND psr.enabled = 1
                    AND p.is_active = 1 AND p.profile_mode != 'workflow'
                )
                OR EXISTS (
                  SELECT 1
                  FROM profile_capability_refs pcr
                  JOIN profile p ON p.id = pcr.profile_id
                  JOIN capability_refs cr ON cr.ref_id = pcr.ref_id
                  WHERE cr.server_id = sc.id
                    AND pcr.enabled = 1
                    AND p.is_active = 1 AND p.profile_mode != 'workflow'
                    AND NOT EXISTS (
                      SELECT 1
                      FROM profile_server_relationships gate
                      WHERE gate.profile_id = pcr.profile_id
                        AND gate.server_id = cr.server_id
                        AND gate.enabled = 0
                    )
                )
              )
            GROUP BY sc.id, sc.name
            "#,
        )
        .fetch_all(&db.pool)
        .await
        .unwrap_or_default();

        let mut tasks = Vec::new();
        for (server_id, _name) in enabled_servers {
            let ctx = crate::core::capability::runtime::ListCtx {
                capability: crate::core::capability::CapabilityType::Prompts,
                server_id: server_id.clone(),
                refresh,
                timeout: Some(Duration::from_secs(10)),
                validation_session: None,
                runtime_identity: None,
                connection_selection: None,
                visibility_snapshot: None,
                name_domain: crate::core::capability::runtime::NameDomain::External,
            };
            let pool = state.connection_pool.clone();
            let db_arc = state.database.as_ref().unwrap().clone();
            tasks.push(async move {
                match crate::core::capability::runtime::list(&ctx, &pool, &db_arc).await {
                    Ok(result) => result.items.into_prompts().unwrap_or_default(),
                    Err(_) => Vec::new(),
                }
            });
        }
        for mut v in futures::stream::iter(tasks)
            .buffer_unordered(crate::core::capability::facade::concurrency_limit())
            .collect::<Vec<_>>()
            .await
        {
            items.append(&mut v);
        }
    }

    let data = json!({ "mode": format!("{:?}", query.mode).to_lowercase(), "prompts": items, "total": items.len() });
    Ok(Json(json!({"success": true, "data": data})))
    */
}

pub async fn prompt_get(
    State(state): State<Arc<AppState>>,
    Json(req): Json<InspectorPromptGetReq>,
) -> Result<Json<InspectorPromptGetResp>, ApiError> {
    let v = service::prompt_get(&state, &req).await?;
    let result: Value = v.get("result").cloned().unwrap_or_else(|| json!({}));
    let server_id = v.get("server_id").cloned().and_then(|vv| {
        if vv.is_null() {
            None
        } else {
            vv.as_str().map(|s| s.to_string())
        }
    });
    let elapsed_ms = v.get("elapsed_ms").and_then(|v| v.as_u64()).unwrap_or_default();
    Ok(Json(InspectorPromptGetResp::success(InspectorPromptGetData {
        result,
        server_id,
        elapsed_ms,
    })))
}

// ==============================
// Resources
// ==============================

pub async fn resources_list(
    State(state): State<Arc<AppState>>,
    Query(query): Query<InspectorListQuery>,
) -> Result<Json<InspectorResourcesListResp>, ApiError> {
    let v = service::list_resources(&state, &query).await?;
    let mode = v.get("mode").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let resources: Vec<Value> = match v.get("resources").cloned() {
        Some(Value::Array(v)) => v,
        _ => Vec::new(),
    };
    let total = v
        .get("total")
        .and_then(|v| v.as_u64())
        .unwrap_or(resources.len() as u64) as usize;
    let meta = v.get("meta").and_then(|v| v.as_array()).cloned();
    let elapsed_ms = v.get("elapsed_ms").and_then(|v| v.as_u64()).unwrap_or_default();
    Ok(Json(InspectorResourcesListResp::success(InspectorResourcesListData {
        mode,
        resources,
        total,
        meta,
        elapsed_ms,
    })))
}

pub async fn resource_read(
    State(state): State<Arc<AppState>>,
    Query(req): Query<InspectorResourceReadQuery>,
) -> Result<Json<InspectorResourceReadResp>, ApiError> {
    let v = service::resource_read(&state, &req).await?;
    let result: Value = v.get("result").cloned().unwrap_or_else(|| json!({}));
    let server_id = v.get("server_id").cloned().and_then(|vv| {
        if vv.is_null() {
            None
        } else {
            vv.as_str().map(|s| s.to_string())
        }
    });
    let elapsed_ms = v.get("elapsed_ms").and_then(|v| v.as_u64()).unwrap_or_default();
    Ok(Json(InspectorResourceReadResp::success(InspectorResourceReadData {
        result,
        server_id,
        elapsed_ms,
    })))
}

pub async fn template_read(
    State(state): State<Arc<AppState>>,
    Json(req): Json<InspectorTemplateReadReq>,
) -> Result<Json<InspectorTemplateReadResp>, ApiError> {
    let v = service::template_read(&state, &req).await?;
    let result = v
        .get("result")
        .cloned()
        .ok_or_else(|| ApiError::InternalError("Inspector template read did not return a result".into()))?;
    let server_id = v.get("server_id").and_then(Value::as_str).map(str::to_string);
    let elapsed_ms = v
        .get("elapsed_ms")
        .and_then(Value::as_u64)
        .ok_or_else(|| ApiError::InternalError("Inspector template read did not return elapsed_ms".into()))?;
    let expanded_uri = v
        .get("expanded_uri")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::InternalError("Inspector template read did not return an expanded URI".into()))?
        .to_string();
    Ok(Json(InspectorTemplateReadResp::success(InspectorTemplateReadData {
        result,
        server_id,
        elapsed_ms,
        expanded_uri,
    })))
}

// ==============================
// Templates
// ==============================

pub async fn templates_list(
    State(state): State<Arc<AppState>>,
    Query(query): Query<InspectorListQuery>,
) -> Result<Json<InspectorTemplatesListResp>, ApiError> {
    let data_value = service::list_templates(&state, &query).await?;
    let mode = data_value
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let templates: Vec<Value> = match data_value.get("templates").cloned() {
        Some(Value::Array(v)) => v,
        _ => Vec::new(),
    };
    let total = data_value
        .get("total")
        .and_then(|v| v.as_u64())
        .unwrap_or(templates.len() as u64) as usize;
    let meta = data_value.get("meta").and_then(|v| v.as_array()).cloned();
    let elapsed_ms = data_value
        .get("elapsed_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or_default();
    let data = InspectorTemplatesListData {
        mode,
        templates,
        total,
        meta,
        elapsed_ms,
    };
    Ok(Json(InspectorTemplatesListResp::success(data)))
}
