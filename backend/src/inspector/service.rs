use anyhow::anyhow;
use futures::StreamExt;
use rmcp::RoleClient;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, ClientRequest, ProgressNotificationParam, ProgressToken, RequestId,
};
use rmcp::service::{Peer, PeerRequestOptions};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;
use tokio::sync::oneshot;
use tokio::time::Duration;

use crate::api::handlers::ApiError;
use crate::api::models::inspector::{
    InspectorListQuery, InspectorMode, InspectorPromptGetReq, InspectorResourceReadQuery, InspectorSessionCloseReq,
    InspectorSessionOpenData, InspectorSessionOpenReq, InspectorSessionRefreshReq, InspectorTemplateReadReq,
    InspectorToolCallReq,
};
use crate::api::routes::AppState;
use crate::core::capability;
use crate::core::capability::resolver;
use crate::inspector::calls::{
    InspectorCallInfo, InspectorCallRegistry, InspectorResultProjection, InspectorTerminal, RegisteredCall,
};
use crate::inspector::sessions::{ActiveSession, InspectorSessionClosing, SessionLookup};

const NATIVE_VALIDATION_SESSION_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub struct ToolCallOutcome {
    pub result: Option<Value>,
    pub server_id: Option<String>,
    pub elapsed_ms: u64,
    pub message: Option<String>,
}

pub struct PreparedCall {
    pub info: InspectorCallInfo,
    pub completion: oneshot::Receiver<InspectorTerminal>,
    pub server_id: String,
    native_validation_session: Option<NativeValidationSessionGuard>,
}

static GLOBAL_CALL_REGISTRY: OnceLock<Arc<InspectorCallRegistry>> = OnceLock::new();

pub fn set_call_registry(registry: Arc<InspectorCallRegistry>) {
    let _ = GLOBAL_CALL_REGISTRY.set(registry);
}

#[derive(Clone, Copy)]
enum CapabilityKind {
    Tools,
    Prompts,
    Resources,
    ResourceTemplates,
}

impl CapabilityKind {
    fn capability_type(self) -> capability::CapabilityType {
        match self {
            CapabilityKind::Tools => capability::CapabilityType::Tools,
            CapabilityKind::Prompts => capability::CapabilityType::Prompts,
            CapabilityKind::Resources => capability::CapabilityType::Resources,
            CapabilityKind::ResourceTemplates => capability::CapabilityType::ResourceTemplates,
        }
    }

    fn response_key(self) -> &'static str {
        match self {
            CapabilityKind::Tools => "tools",
            CapabilityKind::Prompts => "prompts",
            CapabilityKind::Resources => "resources",
            CapabilityKind::ResourceTemplates => "templates",
        }
    }

    fn label(self) -> &'static str {
        match self {
            CapabilityKind::Tools => "tools",
            CapabilityKind::Prompts => "prompts",
            CapabilityKind::Resources => "resources",
            CapabilityKind::ResourceTemplates => "resource templates",
        }
    }

    fn extractor(self) -> fn(capability::runtime::ListResult) -> Result<Vec<Value>, ApiError> {
        match self {
            CapabilityKind::Tools => extract_tools,
            CapabilityKind::Prompts => extract_prompts,
            CapabilityKind::Resources => extract_resources,
            CapabilityKind::ResourceTemplates => extract_resource_templates,
        }
    }
}

struct CapabilityPayload {
    mode: String,
    items: Vec<Value>,
    meta: Vec<Value>,
}

struct NativeValidationSessionGuard {
    connection_pool: Arc<tokio::sync::Mutex<crate::core::pool::UpstreamConnectionPool>>,
    lease: crate::core::pool::ValidationReservationLease,
}

impl NativeValidationSessionGuard {
    fn from_lease(
        state: &AppState,
        lease: crate::core::pool::ValidationReservationLease,
    ) -> Self {
        Self {
            connection_pool: state.connection_pool.clone(),
            lease,
        }
    }

    fn reservation(&self) -> &crate::core::pool::ValidationReservationToken {
        self.lease.token()
    }

    async fn cleanup(self) {
        if let Err(error) = destroy_validation_session(&self.connection_pool, self.lease.token()).await {
            tracing::warn!(error = %error, "Native validation session cleanup failed");
        }
    }
}

pub async fn list_tools(
    state: &AppState,
    query: &InspectorListQuery,
) -> Result<Value, ApiError> {
    list_capability_response(state, query, CapabilityKind::Tools).await
}

pub async fn list_prompts(
    state: &AppState,
    query: &InspectorListQuery,
) -> Result<Value, ApiError> {
    list_capability_response(state, query, CapabilityKind::Prompts).await
}

pub async fn list_resources(
    state: &AppState,
    query: &InspectorListQuery,
) -> Result<Value, ApiError> {
    list_capability_response(state, query, CapabilityKind::Resources).await
}

pub async fn list_templates(
    state: &AppState,
    query: &InspectorListQuery,
) -> Result<Value, ApiError> {
    list_capability_response(state, query, CapabilityKind::ResourceTemplates).await
}

pub async fn prompt_get(
    state: &AppState,
    req: &InspectorPromptGetReq,
) -> Result<Value, ApiError> {
    let timeout = inspector_timeout(state, req.timeout_ms).await;
    let start = Instant::now();
    match req.mode {
        InspectorMode::Proxy => {
            let route = capability::naming::resolve_capability_route(capability::naming::NamingKind::Prompt, &req.name)
                .await
                .map_err(map_anyhow)?;
            let server_id = route.server_id;
            let namespace = route.server_name;
            let upstream_name = route.upstream_value;
            let server_filter = HashSet::from([server_id.clone()]);

            ensure_proxy_connection(state, &server_id).await?;

            let operation_result = run_inspector_operation(timeout, "prompts/get", async {
                let mapping =
                    capability::facade::build_prompt_mapping_filtered(&state.connection_pool, Some(&server_filter))
                        .await
                        .map_err(map_anyhow)?;
                capability::facade::get_upstream_prompt(
                    &state.connection_pool,
                    &mapping,
                    &upstream_name,
                    req.arguments.clone(),
                    Some(&server_id),
                    None,
                )
                .await
                .map_err(map_anyhow)
            })
            .await;
            let mut res = operation_result?;
            let database = state
                .database
                .as_ref()
                .ok_or_else(|| ApiError::InternalError("Proxy Inspector projection requires database access".into()))?;
            crate::inspector::calls::project_prompt_result(
                &InspectorResultProjection::ExternalResourceUris {
                    registry_pool: database.pool.clone(),
                    server_id: server_id.clone(),
                    namespace,
                },
                &mut res,
            )
            .await
            .map_err(map_anyhow)?;

            Ok(json!({
                "result": res,
                "server_id": server_id,
                "elapsed_ms": start.elapsed().as_millis() as u64,
            }))
        }
        InspectorMode::Native => {
            ensure_native_allowed()?;
            let server_id = resolve_server(&req.server_id, &req.server_name).await?;
            let (session_id, cleanup) = native_validation_scope(state, &server_id, req.session_id.as_deref()).await?;
            let res = run_inspector_operation(timeout, "prompts/get", async {
                let conn = clone_native_validation_connection(state, &server_id, &session_id).await?;
                crate::core::capability::prompts::get_upstream_prompt_direct(&conn, &req.name, req.arguments.clone())
                    .await
                    .map_err(map_anyhow)
            })
            .await;
            if let Some(cleanup) = cleanup {
                cleanup.cleanup().await;
            }
            let mut res = res?;
            crate::inspector::calls::project_prompt_result(&InspectorResultProjection::Upstream, &mut res)
                .await
                .map_err(map_anyhow)?;
            Ok(json!({
                "result": res,
                "server_id": server_id,
                "elapsed_ms": start.elapsed().as_millis() as u64,
            }))
        }
    }
}

pub async fn resource_read(
    state: &AppState,
    req: &InspectorResourceReadQuery,
) -> Result<Value, ApiError> {
    let timeout = inspector_timeout(state, req.timeout_ms).await;
    let start = Instant::now();
    match req.mode {
        InspectorMode::Proxy => {
            let database = state.database.as_ref().ok_or_else(|| {
                ApiError::InternalError("Proxy Inspector resource routing requires database access".into())
            })?;
            let route = capability::resource_registry::resolve_resource_route(&database.pool, &req.uri)
                .await
                .map_err(map_anyhow)?;
            let server_id = route.server_id.clone();
            let upstream_uri = route.upstream_uri.clone();

            ensure_proxy_connection(state, &server_id).await?;

            let operation_result = run_inspector_operation(timeout, "resources/read", async {
                capability::facade::read_routed_resource(&state.connection_pool, &server_id, &upstream_uri, None)
                    .await
                    .map_err(map_anyhow)
            })
            .await;
            let mut res = operation_result?;
            capability::resource_registry::rewrite_read_resource_result(&database.pool, &route, &mut res)
                .await
                .map_err(map_anyhow)?;
            Ok(json!({
                "result": res,
                "server_id": server_id,
                "elapsed_ms": start.elapsed().as_millis() as u64,
            }))
        }
        InspectorMode::Native => {
            ensure_native_allowed()?;
            let server_id = resolve_server(&req.server_id, &req.server_name).await?;
            let (session_id, cleanup) = native_validation_scope(state, &server_id, req.session_id.as_deref()).await?;
            let res = run_inspector_operation(timeout, "resources/read", async {
                let conn = clone_native_validation_connection(state, &server_id, &session_id).await?;
                crate::core::capability::resources::read_upstream_resource_direct(&conn, &req.uri)
                    .await
                    .map_err(map_anyhow)
            })
            .await;
            if let Some(cleanup) = cleanup {
                cleanup.cleanup().await;
            }
            let res = res?;
            Ok(json!({
                "result": res,
                "server_id": server_id,
                "elapsed_ms": start.elapsed().as_millis() as u64,
            }))
        }
    }
}

pub async fn template_read(
    state: &AppState,
    req: &InspectorTemplateReadReq,
) -> Result<Value, ApiError> {
    if req.mode == InspectorMode::Proxy {
        let database = state.database.as_ref().ok_or_else(|| {
            ApiError::InternalError("Proxy Inspector template routing requires database access".into())
        })?;
        let registered: Option<String> =
            sqlx::query_scalar("SELECT uri_template FROM server_resource_templates WHERE unique_name = ?")
                .bind(&req.uri_template)
                .fetch_optional(&database.pool)
                .await
                .map_err(|error| map_anyhow(error.into()))?;
        if registered.is_none() {
            return Err(ApiError::BadRequest(format!(
                "Proxy Inspector template '{}' is not registered",
                req.uri_template
            )));
        }
    }

    let expanded_uri = crate::inspector::template::expand_resource_template(&req.uri_template, req.arguments.as_ref())
        .map_err(|error| ApiError::BadRequest(error.to_string()))?;

    let result = resource_read(
        state,
        &InspectorResourceReadQuery {
            uri: expanded_uri.clone(),
            server_id: req.server_id.clone(),
            server_name: req.server_name.clone(),
            session_id: req.session_id.clone(),
            mode: req.mode,
            timeout_ms: req.timeout_ms,
        },
    )
    .await?;
    let mut response = result
        .as_object()
        .cloned()
        .ok_or_else(|| ApiError::InternalError("Inspector resource read did not return an object response".into()))?;
    response.insert("expanded_uri".to_string(), Value::String(expanded_uri));
    Ok(Value::Object(response))
}

pub async fn call_tool(
    state: &Arc<AppState>,
    req: &InspectorToolCallReq,
) -> Result<ToolCallOutcome, ApiError> {
    let prepared = start_tool_call_internal(state, req).await?;
    let PreparedCall {
        completion,
        mut native_validation_session,
        ..
    } = prepared;

    let result = match completion.await {
        Ok(result) => result,
        Err(_) => {
            if let Some(cleanup) = native_validation_session.take() {
                cleanup.cleanup().await;
            }
            return Err(ApiError::InternalError("Inspector call channel dropped".into()));
        }
    };

    if let Some(cleanup) = native_validation_session {
        cleanup.cleanup().await;
    }

    map_tool_call_terminal(result)
}

fn map_tool_call_terminal(result: InspectorTerminal) -> Result<ToolCallOutcome, ApiError> {
    match result {
        InspectorTerminal::Result {
            result,
            elapsed_ms,
            server_id,
        } => Ok(ToolCallOutcome {
            result: Some(result),
            server_id: Some(server_id),
            elapsed_ms,
            message: Some("completed".to_string()),
        }),
        InspectorTerminal::Error { message, .. } => Err(ApiError::InternalError(message)),
        InspectorTerminal::Timeout { .. } => Err(ApiError::Timeout("Inspector tools/call exceeded its timeout".into())),
        InspectorTerminal::Cancelled { reason, .. } => Err(ApiError::Conflict(
            reason.unwrap_or_else(|| "Inspector call cancelled".to_string()),
        )),
    }
}

pub async fn start_tool_call(
    state: &Arc<AppState>,
    req: &InspectorToolCallReq,
) -> Result<InspectorCallInfo, ApiError> {
    let prepared = start_tool_call_internal(state, req).await?;
    let info = prepared.info.clone();

    tokio::spawn(async move {
        let PreparedCall {
            completion,
            native_validation_session,
            ..
        } = prepared;

        let _ = completion.await;

        if let Some(cleanup) = native_validation_session {
            cleanup.cleanup().await;
        }
    });

    Ok(info)
}

pub async fn open_session(
    state: &Arc<AppState>,
    req: &InspectorSessionOpenReq,
) -> Result<InspectorSessionOpenData, ApiError> {
    if matches!(req.mode, InspectorMode::Native) {
        ensure_native_allowed()?;
    }

    let server_id = resolve_server(&req.server_id, &req.server_name).await?;
    let session_id = crate::generate_id!("inspses");
    let (peer, validation_reservation) = if matches!(req.mode, InspectorMode::Native) {
        let validation_session = native_session_id_for_inspector_session(&session_id);
        let lease = ensure_native_session(state, &server_id, &validation_session).await?;
        acquire_validation_peer(state, &server_id, lease.token())
            .await
            .map_err(|e| ApiError::InternalError(e.to_string()))?;
        (None, Some(lease))
    } else {
        ensure_proxy_connection(state, &server_id).await?;
        let peer = acquire_peer_for_call(state, &server_id, None)
            .await
            .map_err(|e| ApiError::InternalError(e.to_string()))?;
        (Some(peer), None)
    };

    let info = state
        .inspector_sessions
        .open_session(
            session_id.clone(),
            server_id.clone(),
            req.mode,
            peer,
            validation_reservation,
        )
        .await;

    Ok(InspectorSessionOpenData {
        session_id: info.session_id,
        server_id: info.server_id,
        mode: info.mode,
        expires_at_epoch_ms: info.expires_at_epoch_ms,
    })
}

pub async fn close_session(
    state: &Arc<AppState>,
    req: &InspectorSessionCloseReq,
) -> Result<bool, ApiError> {
    let Some(closing) = state.inspector_sessions.begin_close(&req.session_id).await else {
        return Ok(false);
    };
    release_inspector_session(state, closing).await?;
    Ok(true)
}

pub async fn refresh_session(
    state: &Arc<AppState>,
    req: &InspectorSessionRefreshReq,
) -> Result<InspectorSessionOpenData, ApiError> {
    let session = get_active_session(state, &req.session_id).await?;

    if matches!(session.mode, InspectorMode::Native) {
        let reservation = session.validation_reservation.as_ref().ok_or_else(|| {
            ApiError::InternalError("Native Inspector session is missing validation ownership".into())
        })?;
        let mut pool = state.connection_pool.lock().await;
        let connected = pool
            .validation_sessions
            .get(reservation.session_id())
            .and_then(|servers| servers.get(&session.server_id))
            .and_then(|connection| connection.service.as_ref())
            .is_some();
        if !connected {
            return Err(ApiError::NotFound(format!(
                "Native Inspector session '{}' is no longer connected for server '{}'",
                req.session_id, session.server_id
            )));
        }
        if !pool.refresh_validation_reservation(reservation, NATIVE_VALIDATION_SESSION_TTL) {
            return Err(ApiError::NotFound(format!(
                "Native Inspector validation session '{}' not found or expired",
                req.session_id
            )));
        }
    }

    Ok(InspectorSessionOpenData {
        session_id: req.session_id.clone(),
        server_id: session.server_id,
        mode: session.mode,
        expires_at_epoch_ms: session.expires_at_epoch_ms,
    })
}

async fn release_inspector_session(
    state: &AppState,
    closing: InspectorSessionClosing,
) -> Result<(), ApiError> {
    let info = closing.info().clone();
    if matches!(info.mode, InspectorMode::Native)
        && let Some(reservation) = info.validation_reservation.as_ref()
    {
        crate::core::pool::UpstreamConnectionPool::release_validation_reservation(&state.connection_pool, reservation)
            .await
            .map_err(|error| ApiError::InternalError(error.to_string()))?;
    }
    if !closing.complete().await {
        return Err(ApiError::Conflict("Inspector session changed while closing".into()));
    }
    Ok(())
}

async fn get_active_session(
    state: &AppState,
    session_id: &str,
) -> Result<ActiveSession, ApiError> {
    match state.inspector_sessions.get_session(session_id).await {
        SessionLookup::Active(session) => Ok(session),
        SessionLookup::Expired(closing) => {
            release_inspector_session(state, closing).await?;
            Err(ApiError::NotFound(format!(
                "Inspector session '{}' not found or expired",
                session_id
            )))
        }
        SessionLookup::Missing => Err(ApiError::NotFound(format!(
            "Inspector session '{}' not found or expired",
            session_id
        ))),
    }
}

async fn get_inspector_timeout_default(state: &AppState) -> Option<u64> {
    let db = state.database.as_ref()?;
    crate::system::settings::get_inspector_timeout_ms(&db.pool).await.ok()
}

async fn inspector_timeout(
    state: &AppState,
    requested_ms: Option<u64>,
) -> Duration {
    let timeout_ms = match requested_ms {
        Some(timeout_ms) => timeout_ms,
        None => get_inspector_timeout_default(state).await.unwrap_or(60_000),
    };
    Duration::from_millis(timeout_ms)
}

async fn run_inspector_operation<T, F>(
    timeout: Duration,
    operation: &'static str,
    future: F,
) -> Result<T, ApiError>
where
    F: Future<Output = Result<T, ApiError>>,
{
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| ApiError::Timeout(format!("Inspector {operation} exceeded {} ms", timeout.as_millis())))?
}

async fn ensure_proxy_connection(
    state: &AppState,
    server_id: &str,
) -> Result<(), ApiError> {
    let selection = crate::core::capability::ConnectionSelection {
        server_id: server_id.to_string(),
        affinity_key: crate::core::capability::AffinityKey::Default,
    };
    crate::core::pool::UpstreamConnectionPool::ensure_connected_coordinated(&state.connection_pool, &selection)
        .await
        .map_err(map_anyhow)?;
    Ok(())
}

async fn start_tool_call_internal(
    state: &Arc<AppState>,
    req: &InspectorToolCallReq,
) -> Result<PreparedCall, ApiError> {
    let timeout = inspector_timeout(state, req.timeout_ms).await;
    start_tool_call_internal_with_timeout(state, req, timeout).await
}

async fn start_tool_call_internal_with_timeout(
    state: &Arc<AppState>,
    req: &InspectorToolCallReq,
    timeout: Duration,
) -> Result<PreparedCall, ApiError> {
    if matches!(req.mode, InspectorMode::Native) {
        ensure_native_allowed()?;
    }

    let active_session = if let Some(session_id) = req.session_id.as_ref() {
        Some(get_active_session(state, session_id).await?)
    } else {
        None
    };

    let (server_id, upstream_tool_name, result_projection) = match req.mode {
        InspectorMode::Proxy => {
            let route = capability::naming::resolve_capability_route(capability::naming::NamingKind::Tool, &req.tool)
                .await
                .map_err(map_anyhow)?;
            let database = state
                .database
                .as_ref()
                .ok_or_else(|| ApiError::InternalError("Proxy Inspector projection requires database access".into()))?;
            (
                route.server_id.clone(),
                route.upstream_value,
                InspectorResultProjection::ExternalResourceUris {
                    registry_pool: database.pool.clone(),
                    server_id: route.server_id,
                    namespace: route.server_name,
                },
            )
        }
        InspectorMode::Native => {
            let sid = if let Some(session) = &active_session {
                session.server_id.clone()
            } else {
                resolve_server(&req.server_id, &req.server_name).await?
            };
            (sid.clone(), req.tool.clone(), InspectorResultProjection::Upstream)
        }
    };

    if let Some(session) = &active_session {
        if session.server_id != server_id {
            return Err(ApiError::BadRequest(
                "Inspector session is bound to a different server".into(),
            ));
        }
        if session.mode != req.mode {
            return Err(ApiError::BadRequest(
                "Inspector session is bound to a different mode".into(),
            ));
        }
    }

    let (peer, native_cleanup_session) = match (req.mode, active_session) {
        (InspectorMode::Native, Some(session)) => {
            let reservation = session.validation_reservation.as_ref().ok_or_else(|| {
                ApiError::InternalError("Native Inspector session is missing validation ownership".into())
            })?;
            let peer = acquire_validation_peer(state, &server_id, reservation)
                .await
                .map_err(|e| ApiError::InternalError(e.to_string()))?;
            (peer, None)
        }
        (InspectorMode::Native, None) => {
            let validation_session = native_temporary_session_id();
            let lease = ensure_native_session(state, &server_id, &validation_session).await?;
            let cleanup = NativeValidationSessionGuard::from_lease(state, lease);
            let peer = acquire_validation_peer(state, &server_id, cleanup.reservation())
                .await
                .map_err(|e| ApiError::InternalError(e.to_string()))?;
            (peer, Some(cleanup))
        }
        (_, Some(session)) => {
            let peer = session
                .peer
                .ok_or_else(|| ApiError::InternalError("Inspector session peer is not available".into()))?;
            (peer, None)
        }
        _ => {
            ensure_proxy_connection(state, &server_id).await?;
            let peer = acquire_peer_for_call(state, &server_id, None)
                .await
                .map_err(|e| ApiError::InternalError(e.to_string()))?;
            (peer, None)
        }
    };

    let params =
        CallToolRequestParams::new(upstream_tool_name).with_arguments(req.arguments.clone().unwrap_or_default());
    let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));

    let mut options = PeerRequestOptions::no_options();
    options.timeout = Some(timeout);

    let call_id = crate::generate_id!("inspcall");
    let mut native_cleanup_session = native_cleanup_session;
    let handle = match peer.send_cancellable_request(request, options).await {
        Ok(handle) => handle,
        Err(err) => {
            if let Some(cleanup) = native_cleanup_session.take() {
                cleanup.cleanup().await;
            }
            return Err(ApiError::InternalError(err.to_string()));
        }
    };

    let RegisteredCall { info, completion } = state
        .inspector_calls
        .start_call(
            call_id,
            server_id.clone(),
            req.mode,
            req.session_id.clone(),
            handle,
            result_projection,
        )
        .await;

    Ok(PreparedCall {
        info,
        completion,
        server_id,
        native_validation_session: native_cleanup_session,
    })
}

fn extract_tools(result: capability::runtime::ListResult) -> Result<Vec<Value>, ApiError> {
    result
        .items
        .into_tools()
        .unwrap_or_default()
        .into_iter()
        .map(|tool| {
            serde_json::to_value(tool)
                .map_err(|error| ApiError::InternalError(format!("Failed to serialize inspector tool item: {error}")))
        })
        .collect()
}

fn extract_prompts(result: capability::runtime::ListResult) -> Result<Vec<Value>, ApiError> {
    result
        .items
        .into_prompts()
        .unwrap_or_default()
        .into_iter()
        .map(|prompt| {
            serde_json::to_value(prompt)
                .map_err(|error| ApiError::InternalError(format!("Failed to serialize inspector prompt item: {error}")))
        })
        .collect()
}

fn extract_resources(result: capability::runtime::ListResult) -> Result<Vec<Value>, ApiError> {
    result
        .items
        .into_resources()
        .unwrap_or_default()
        .into_iter()
        .map(|resource| {
            serde_json::to_value(resource).map_err(|error| {
                ApiError::InternalError(format!("Failed to serialize inspector resource item: {error}"))
            })
        })
        .collect()
}

fn extract_resource_templates(result: capability::runtime::ListResult) -> Result<Vec<Value>, ApiError> {
    result
        .items
        .into_resource_templates()
        .unwrap_or_default()
        .into_iter()
        .map(|template| {
            serde_json::to_value(template).map_err(|error| {
                ApiError::InternalError(format!("Failed to serialize inspector resource template item: {error}"))
            })
        })
        .collect()
}

async fn list_capability_response(
    state: &AppState,
    query: &InspectorListQuery,
    kind: CapabilityKind,
) -> Result<Value, ApiError> {
    let start = Instant::now();
    let payload = list_capability_payload(state, query, kind).await?;
    Ok(json!({
        "mode": payload.mode,
        kind.response_key(): payload.items,
        "total": payload.items.len(),
        "meta": payload.meta,
        "elapsed_ms": start.elapsed().as_millis() as u64,
    }))
}

async fn list_capability_payload(
    state: &AppState,
    query: &InspectorListQuery,
    kind: CapabilityKind,
) -> Result<CapabilityPayload, ApiError> {
    let timeout = inspector_timeout(state, query.timeout_ms).await;
    let refresh = if query.refresh {
        Some(capability::runtime::RefreshStrategy::Force)
    } else {
        Some(capability::runtime::RefreshStrategy::CacheFirst)
    };

    let extractor = kind.extractor();
    let capability_type = kind.capability_type();

    let mut items: Vec<Value> = Vec::new();
    let mut meta_entries: Vec<Value> = Vec::new();

    match query.mode {
        InspectorMode::Proxy => {
            if query.server_id.is_some() || query.server_name.is_some() {
                let server_id = resolve_server(&query.server_id, &query.server_name).await?;
                let pool = state.connection_pool.clone();
                let database = state
                    .database
                    .as_ref()
                    .ok_or(ApiError::InternalError("Database not available".into()))?
                    .clone();
                let (extracted, meta) = list_capability_via_components(
                    pool,
                    database,
                    &server_id,
                    refresh,
                    capability_type,
                    timeout,
                    None,
                    capability::runtime::NameDomain::External,
                    extractor,
                )
                .await?;
                items = extracted;
                meta_entries.push(meta);
            } else {
                let db = state
                    .database
                    .as_ref()
                    .ok_or(ApiError::InternalError("Database not available".into()))?;
                let enabled_servers: Vec<(String, String)> = sqlx::query_as(
                    r#"SELECT sc.id, sc.name FROM server_config sc
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
                       GROUP BY sc.id, sc.name"#,
                )
                .fetch_all(&db.pool)
                .await
                .map_err(|error| {
                    ApiError::InternalError(format!(
                        "Failed to load enabled servers for Inspector aggregate: {error}"
                    ))
                })?;

                let mut tasks = Vec::new();
                let pool = state.connection_pool.clone();
                for (server_id, server_name) in enabled_servers {
                    let db_arc = state.database.as_ref().unwrap().clone();
                    let pool_clone = pool.clone();
                    let db_clone = db_arc.clone();
                    tasks.push(async move {
                        let sid = server_id.clone();
                        list_capability_via_components(
                            pool_clone,
                            db_clone,
                            &sid,
                            refresh,
                            capability_type,
                            timeout,
                            None,
                            capability::runtime::NameDomain::External,
                            extractor,
                        )
                        .await
                        .map(|(values, meta)| (sid.clone(), server_name.clone(), values, meta))
                        .map_err(|err| (sid.clone(), server_name.clone(), err))
                    });
                }

                let mut aggregate = capability::aggregate::AggregateListStatus::new(kind.label());
                for outcome in futures::stream::iter(tasks)
                    .buffer_unordered(capability::facade::concurrency_limit())
                    .collect::<Vec<_>>()
                    .await
                {
                    match outcome {
                        Ok((_, _, mut values, meta)) => {
                            aggregate.record_success();
                            meta_entries.push(meta);
                            items.append(&mut values);
                        }
                        Err((server_id, server_name, error)) => {
                            aggregate.record_failure(&server_id, &server_name, &error);
                            meta_entries.push(meta_error(&server_id, &error.to_string()));
                        }
                    }
                }
                aggregate
                    .finish_for_result(!items.is_empty())
                    .map_err(|error| ApiError::ServiceUnavailable(error.to_string()))?;
            }
        }
        InspectorMode::Native => {
            ensure_native_allowed()?;
            let server_id = resolve_server(&query.server_id, &query.server_name).await?;
            let (session_id, cleanup) = native_validation_scope(state, &server_id, query.session_id.as_deref()).await?;
            let pool = state.connection_pool.clone();
            let database = match state.database.as_ref() {
                Some(database) => database.clone(),
                None => {
                    if let Some(cleanup) = cleanup {
                        cleanup.cleanup().await;
                    }
                    return Err(ApiError::InternalError("Database not available".into()));
                }
            };
            let result = list_capability_via_components(
                pool,
                database,
                &server_id,
                refresh,
                capability_type,
                timeout,
                Some(session_id.session_id().to_string()),
                capability::runtime::NameDomain::Upstream,
                extractor,
            )
            .await;
            if let Some(cleanup) = cleanup {
                cleanup.cleanup().await;
            }
            let (extracted, meta) = result?;
            items = extracted;
            meta_entries.push(meta);
        }
    }

    Ok(CapabilityPayload {
        mode: format!("{:?}", query.mode).to_lowercase(),
        items,
        meta: meta_entries,
    })
}

async fn list_capability_via_components(
    pool: Arc<tokio::sync::Mutex<crate::core::pool::UpstreamConnectionPool>>,
    database: Arc<crate::config::database::Database>,
    server_id: &str,
    refresh: Option<capability::runtime::RefreshStrategy>,
    capability_type: capability::CapabilityType,
    timeout: Duration,
    session_id: Option<String>,
    name_domain: capability::runtime::NameDomain,
    extractor: fn(capability::runtime::ListResult) -> Result<Vec<Value>, ApiError>,
) -> Result<(Vec<Value>, Value), ApiError> {
    let service = capability::read_service::CapabilityReadService::from_runtime(database, pool);

    let ctx = capability::runtime::ListCtx {
        capability: capability_type,
        server_id: server_id.to_string(),
        refresh,
        operation_timeout: timeout,
        validation_session: session_id,
        runtime_identity: None,
        connection_selection: None,
        visibility_snapshot: None,
        name_domain,
    };

    let result = service.list(&ctx).await.map_err(map_capability_read_error)?;
    let meta = meta_success(server_id, &result.meta);
    let items = extractor(result)?;
    Ok((items, meta))
}

async fn destroy_validation_session(
    connection_pool: &Arc<tokio::sync::Mutex<crate::core::pool::UpstreamConnectionPool>>,
    reservation: &crate::core::pool::ValidationReservationToken,
) -> Result<(), crate::core::pool::ValidationShutdownError> {
    crate::core::pool::UpstreamConnectionPool::release_validation_reservation(connection_pool, reservation).await
}

fn map_anyhow(e: anyhow::Error) -> ApiError {
    let mut message = e.to_string();
    for cause in e.chain().skip(1) {
        let cause = cause.to_string();
        if !cause.is_empty() && !message.contains(&cause) {
            message.push_str(": ");
            message.push_str(&cause);
        }
    }
    ApiError::InternalError(message)
}

fn map_capability_read_error(error: capability::read_service::CapabilityReadError) -> ApiError {
    if let Some(timeout_ms) = error.connection_timeout_ms() {
        return ApiError::GatewayTimeout(format!("Inspector server connect exceeded {timeout_ms} ms"));
    }
    if let Some(timeout_ms) = error.operation_timeout_ms() {
        return ApiError::Timeout(format!("Inspector capability operation exceeded {timeout_ms} ms"));
    }
    capability::service::map_capability_read_error(&error)
}

/// Mirrors `core::capability::service::map_capability_read_error`, which every other read
/// surface (tools/prompts/resources/templates/detail) already uses so a typed
/// `CapabilityReadError` reports a consistent status code and reason instead of collapsing to
/// a bare `InternalError`. The Inspector previously only recognized timeouts here and fell
/// back to a generic 500 for everything else, silently losing e.g. upstream authentication
/// failures (`Unauthorized`) and discovery failures (`BadGateway`) (Codex review follow-up,
/// PR #160).
/// Inspector tests still exercise the legacy anyhow error mapping path.
#[cfg(test)]
fn map_capability_list_error(error: anyhow::Error) -> ApiError {
    match error.downcast::<capability::read_service::CapabilityReadError>() {
        Ok(read_error) => map_capability_read_error(read_error),
        Err(error) => map_anyhow(error),
    }
}

fn meta_success(
    server_id: &str,
    meta: &capability::runtime::Meta,
) -> Value {
    json!({
        "server_id": server_id,
        "cache_hit": meta.cache_hit,
        "source": meta.source,
        "duration_ms": meta.duration_ms,
        "had_peer": meta.had_peer,
    })
}

fn meta_error(
    server_id: &str,
    message: &str,
) -> Value {
    json!({
        "server_id": server_id,
        "error": message,
    })
}

fn native_mode_enabled() -> bool {
    match std::env::var("MCPMATE_INSPECTOR_NATIVE") {
        Ok(val) => {
            let val_lower = val.trim().to_ascii_lowercase();
            // Explicit "off" values disable; everything else (including unknown) enables
            !matches!(val_lower.as_str(), "0" | "false" | "off" | "no")
        }
        // Default to enabled if not set
        Err(_) => true,
    }
}

fn ensure_native_allowed() -> Result<(), ApiError> {
    if native_mode_enabled() {
        Ok(())
    } else {
        Err(ApiError::Forbidden(
            "Native inspector mode is disabled; set MCPMATE_INSPECTOR_NATIVE=on (or unset) to enable".into(),
        ))
    }
}

async fn ensure_native_session(
    state: &AppState,
    server_id: &str,
    session_id: &str,
) -> Result<crate::core::pool::ValidationReservationLease, ApiError> {
    let lease = crate::core::pool::UpstreamConnectionPool::ensure_validation_instance(
        &state.connection_pool,
        server_id,
        session_id,
        NATIVE_VALIDATION_SESSION_TTL,
    )
    .await
    .map_err(map_anyhow)?;

    Ok(lease)
}

fn native_session_id_for_inspector_session(session_id: &str) -> String {
    format!("inspector_native_session::{session_id}")
}

fn native_temporary_session_id() -> String {
    crate::generate_id!("inspnative")
}

async fn native_validation_session_for_request(
    state: &AppState,
    server_id: &str,
    inspector_session_id: &str,
) -> Result<crate::core::pool::ValidationReservationToken, ApiError> {
    let session = get_active_session(state, inspector_session_id).await?;

    if session.mode != InspectorMode::Native {
        return Err(ApiError::BadRequest(
            "Inspector session is bound to a different mode".into(),
        ));
    }
    if session.server_id != server_id {
        return Err(ApiError::BadRequest(
            "Inspector session is bound to a different server".into(),
        ));
    }

    let reservation = session
        .validation_reservation
        .ok_or_else(|| ApiError::InternalError("Native Inspector session is missing validation ownership".into()))?;

    // Native Inspector reuse is intentionally scoped to the explicit inspector
    // session id. Do not reuse a live validation instance by server id alone:
    // service deployments can have different users or organizations inspecting
    // the same server concurrently, and MCP sessions can carry state. When
    // auth/RBAC lands, extend this ownership boundary with actor/org scope.
    let mut pool = state.connection_pool.lock().await;
    if pool.refresh_validation_reservation(&reservation, NATIVE_VALIDATION_SESSION_TTL) {
        Ok(reservation)
    } else {
        Err(ApiError::NotFound(format!(
            "Native Inspector validation session '{}' not found or expired",
            inspector_session_id
        )))
    }
}

async fn native_validation_scope(
    state: &AppState,
    server_id: &str,
    inspector_session_id: Option<&str>,
) -> Result<
    (
        crate::core::pool::ValidationReservationToken,
        Option<NativeValidationSessionGuard>,
    ),
    ApiError,
> {
    if let Some(inspector_session_id) = inspector_session_id {
        let reservation = native_validation_session_for_request(state, server_id, inspector_session_id).await?;
        return Ok((reservation, None));
    }

    let validation_session = native_temporary_session_id();
    let lease = ensure_native_session(state, server_id, &validation_session).await?;
    let cleanup = NativeValidationSessionGuard::from_lease(state, lease);
    Ok((cleanup.reservation().clone(), Some(cleanup)))
}

async fn acquire_validation_peer(
    state: &AppState,
    server_id: &str,
    reservation: &crate::core::pool::ValidationReservationToken,
) -> Result<Peer<RoleClient>, anyhow::Error> {
    acquire_validation_peer_from_pool(&state.connection_pool, server_id, reservation).await
}

async fn acquire_validation_peer_from_pool(
    connection_pool: &Arc<tokio::sync::Mutex<crate::core::pool::UpstreamConnectionPool>>,
    server_id: &str,
    reservation: &crate::core::pool::ValidationReservationToken,
) -> Result<Peer<RoleClient>, anyhow::Error> {
    let mut pool_guard = connection_pool.lock().await;
    let peer = pool_guard
        .validation_sessions
        .get(reservation.session_id())
        .and_then(|session_servers| session_servers.get(server_id))
        .and_then(|conn| conn.service.as_ref())
        .map(|service| service.peer().clone());

    if let Some(peer) = peer {
        if pool_guard.refresh_validation_reservation(reservation, NATIVE_VALIDATION_SESSION_TTL) {
            return Ok(peer);
        }
    }

    Err(anyhow!(
        "Native Inspector session '{}' is no longer connected for server '{}'",
        reservation.session_id(),
        server_id
    ))
}

async fn clone_native_validation_connection(
    state: &AppState,
    server_id: &str,
    reservation: &crate::core::pool::ValidationReservationToken,
) -> Result<crate::core::pool::UpstreamConnection, ApiError> {
    let mut pool = state.connection_pool.lock().await;
    if !pool.refresh_validation_reservation(reservation, NATIVE_VALIDATION_SESSION_TTL) {
        return Err(ApiError::NotFound(format!(
            "Native Inspector validation session '{}' not found or expired",
            reservation.session_id()
        )));
    }
    pool.validation_sessions
        .get(reservation.session_id())
        .and_then(|session| session.get(server_id))
        .cloned()
        .ok_or_else(|| ApiError::InternalError("Validation connection not found".into()))
}

async fn acquire_peer_for_call(
    state: &AppState,
    server_id: &str,
    session_id: Option<&str>,
) -> Result<Peer<RoleClient>, anyhow::Error> {
    let pool_guard = state.connection_pool.lock().await;
    let snapshot = pool_guard.get_snapshot();
    if let Some(instances) = snapshot.get(server_id) {
        if let Some((_, _status, _, _, peer_opt)) = instances.iter().find(|(_, st, _, _, p)| {
            matches!(st, crate::core::foundation::types::ConnectionStatus::Ready) && p.is_some()
        }) {
            if let Some(peer) = peer_opt.clone() {
                return Ok(peer);
            }
        }
    }

    if let Some(session_id) = session_id {
        if let Some(session_servers) = pool_guard.validation_sessions.get(session_id) {
            if let Some(conn) = session_servers.get(server_id) {
                if let Some(service) = conn.service.as_ref() {
                    return Ok(service.peer().clone());
                }
            }
        }
    }

    Err(anyhow!(
        "No ready upstream instance available for server '{}'",
        server_id
    ))
}

async fn resolve_server(
    server_id: &Option<String>,
    server_name: &Option<String>,
) -> Result<String, ApiError> {
    if let Some(id) = server_id.clone() {
        return Ok(id);
    }
    if let Some(name) = server_name.clone() {
        return resolver::to_id(&name)
            .await
            .ok()
            .flatten()
            .ok_or_else(|| ApiError::BadRequest(format!("Server '{}' not found", name)));
    }
    Err(ApiError::BadRequest("server_id or server_name is required".into()))
}

pub(crate) async fn inspector_forward_progress(params: &ProgressNotificationParam) -> bool {
    if let Some(registry) = GLOBAL_CALL_REGISTRY.get() {
        registry.emit_progress(params).await;
    }
    false
}

#[expect(deprecated, reason = "Inspector preserves negotiated MCP logging events")]
pub(crate) async fn inspector_forward_log(
    token: Option<&ProgressToken>,
    params: &rmcp::model::LoggingMessageNotificationParam,
) -> bool {
    if let Some(registry) = GLOBAL_CALL_REGISTRY.get() {
        registry.emit_log(token, params).await;
    }
    false
}

pub(crate) async fn inspector_forward_cancel(
    request_id: &RequestId,
    reason: Option<String>,
) -> bool {
    if let Some(registry) = GLOBAL_CALL_REGISTRY.get() {
        registry.emit_cancelled(request_id, reason).await;
    }
    false
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use axum::{http::StatusCode, response::IntoResponse};
    use tokio::sync::Mutex;

    use super::{
        NativeValidationSessionGuard, acquire_validation_peer_from_pool, map_capability_list_error,
        map_tool_call_terminal, run_inspector_operation,
    };
    use crate::api::handlers::ApiError;
    use crate::core::capability::{
        CapabilityType,
        connection_provider::{CapabilityOwnerError, OwnerSource},
        read_service::{CapabilityAttemptError, CapabilityReadError, DiscoveryAttemptFailure},
        runtime::{RuntimeFailure, RuntimeFailureKind},
    };
    use crate::core::{models::Config, pool::UpstreamConnectionPool};
    use crate::inspector::calls::InspectorTerminal;
    use tokio::time::Duration;

    #[tokio::test]
    async fn inspector_operation_timeout_is_scoped_to_one_operation() {
        let error = run_inspector_operation(Duration::from_millis(5), "resources/read", async {
            tokio::time::sleep(Duration::from_millis(25)).await;
            Ok::<_, ApiError>(())
        })
        .await
        .expect_err("slow resource read should time out");

        assert!(matches!(error, ApiError::Timeout(message) if message.contains("resources/read")));

        run_inspector_operation(Duration::from_millis(25), "prompts/get", async {
            Ok::<_, ApiError>(())
        })
        .await
        .expect("a later operation receives its own timeout budget");
    }

    #[tokio::test]
    async fn cancelled_temporary_cleanup_keeps_lease_armed_until_detach() {
        let pool = Arc::new(Mutex::new(UpstreamConnectionPool::new(
            Arc::new(Config {
                mcp_servers: HashMap::new(),
                pagination: None,
            }),
            None,
        )));
        let lease =
            UpstreamConnectionPool::reserve_validation_session(&pool, "temporary-cancel", Duration::from_secs(60))
                .await;
        let guard = NativeValidationSessionGuard {
            connection_pool: pool.clone(),
            lease,
        };
        let pool_lock = pool.lock().await;
        let cleanup = tokio::spawn(guard.cleanup());
        tokio::task::yield_now().await;
        cleanup.abort();
        assert!(cleanup.await.expect_err("cleanup should be cancelled").is_cancelled());
        drop(pool_lock);

        tokio::time::timeout(Duration::from_secs(1), async {
            while pool.lock().await.validation_sessions.contains_key("temporary-cancel") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("temporary guard drop must retry token-scoped detach");
    }

    #[tokio::test]
    async fn cancelled_peer_acquisition_keeps_open_lease_armed() {
        let pool = Arc::new(Mutex::new(UpstreamConnectionPool::new(
            Arc::new(Config {
                mcp_servers: HashMap::new(),
                pagination: None,
            }),
            None,
        )));
        let lease =
            UpstreamConnectionPool::reserve_validation_session(&pool, "peer-cancel", Duration::from_secs(60)).await;
        let pool_lock = pool.lock().await;
        let task_pool = pool.clone();
        let acquisition =
            tokio::spawn(async move { acquire_validation_peer_from_pool(&task_pool, "server-1", lease.token()).await });
        tokio::task::yield_now().await;
        acquisition.abort();
        assert!(
            acquisition
                .await
                .expect_err("peer acquisition should be cancelled")
                .is_cancelled()
        );
        drop(pool_lock);

        tokio::time::timeout(Duration::from_secs(1), async {
            while pool.lock().await.validation_sessions.contains_key("peer-cancel") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled peer acquisition must release the armed lease");
    }

    #[test]
    fn capability_connection_timeout_returns_gateway_timeout() {
        let error = CapabilityReadError::DiscoveryFailed {
            server_id: "server-a".to_string(),
            server_name: "docs".to_string(),
            operation: "tools/list",
            kind: CapabilityType::Tools,
            catalog_error: None,
            existing: Some(Box::new(DiscoveryAttemptFailure {
                instance_id: None,
                connection_generation: None,
                source: OwnerSource::Existing,
                error: CapabilityAttemptError::Owner(CapabilityOwnerError::Timeout { timeout_ms: 250 }),
            })),
            fresh: None,
        };

        let response = map_capability_list_error(error.into()).into_response();

        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    }

    #[test]
    fn capability_operation_timeout_returns_request_timeout() {
        let error = CapabilityReadError::DiscoveryFailed {
            server_id: "server-a".to_string(),
            server_name: "docs".to_string(),
            operation: "tools/list",
            kind: CapabilityType::Tools,
            catalog_error: None,
            existing: Some(Box::new(DiscoveryAttemptFailure {
                instance_id: Some("instance-a".to_string()),
                connection_generation: None,
                source: OwnerSource::Existing,
                error: CapabilityAttemptError::Runtime(RuntimeFailure {
                    kind: RuntimeFailureKind::Timeout,
                    message: Some("request timeout".to_string()),
                    timeout_ms: Some(250),
                }),
            })),
            fresh: None,
        };

        let response = map_capability_list_error(error.into()).into_response();

        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    }

    #[test]
    fn upstream_authentication_failure_returns_unauthorized_instead_of_generic_500() {
        // Before this fix, `map_capability_list_error` only recognized timeouts and fell back
        // to a bare `InternalError` (500) for every other typed `CapabilityReadError`, silently
        // losing the more specific status the main proxy read paths already report via
        // `map_capability_read_error` (Codex review follow-up, PR #160).
        let error = CapabilityReadError::CleanupFailed {
            server_id: "server-a".to_string(),
            server_name: "docs".to_string(),
            operation: "tools/list",
            instance_id: "instance-a".to_string(),
            connection_generation: None,
            owner_source: OwnerSource::Existing,
            error: Box::new(CapabilityOwnerError::Authentication {
                code: crate::core::capability::connection_provider::CapabilityAuthenticationFailureCode::AuthRequired,
                reason: "token expired".to_string(),
            }),
        };

        let response = map_capability_list_error(error.into()).into_response();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn discovery_failure_without_a_timeout_returns_bad_gateway_not_internal_error() {
        let error = CapabilityReadError::DiscoveryFailed {
            server_id: "server-a".to_string(),
            server_name: "docs".to_string(),
            operation: "tools/list",
            kind: CapabilityType::Tools,
            catalog_error: None,
            existing: Some(Box::new(DiscoveryAttemptFailure {
                instance_id: None,
                connection_generation: None,
                source: OwnerSource::Existing,
                error: CapabilityAttemptError::Owner(CapabilityOwnerError::Missing {
                    reason: "no connection".to_string(),
                }),
            })),
            fresh: None,
        };

        let response = map_capability_list_error(error.into()).into_response();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn synchronous_tool_call_timeout_returns_request_timeout() {
        let error = map_tool_call_terminal(InspectorTerminal::Timeout {
            server_id: "server-a".to_string(),
        })
        .expect_err("timed out tool calls must fail");

        assert_eq!(error.into_response().status(), StatusCode::REQUEST_TIMEOUT);
    }
}
