#![expect(
    clippy::result_large_err,
    reason = "CapabilityReadError carries structured upstream diagnostics for API mapping."
)]

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex as StdMutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use anyhow::Context as _;
use async_trait::async_trait;
use dashmap::DashMap;
use mcpmate_capability_store::CatalogError;
use tokio::sync::Mutex;

use crate::config::database::Database;
use crate::core::capability::{
    CapabilityType,
    connection_provider::{
        CapabilityAuthenticationFailureCode, CapabilityConnectionProvider, CapabilityOwner, CapabilityOwnerError,
        DiscoveryRetryDisposition, OwnerSource, PoolCapabilityConnectionProvider,
    },
    runtime::{
        self, CapabilityDiscoveryObservation, CatalogReadFailure, ListCtx, ListResult, NameDomain, RefreshStrategy,
        RuntimeFailure, RuntimeFailureKind,
    },
};
use crate::core::pool::UpstreamConnectionPool;

#[derive(Debug, thiserror::Error)]
pub(crate) enum CapabilityReadError {
    #[error("capability catalog is untrusted for server '{server_id}': {source}")]
    CatalogUntrusted {
        server_id: String,
        #[source]
        source: CatalogError,
    },
    #[error("capability catalog read failed for server '{server_id}': {source}")]
    CatalogOperation {
        server_id: String,
        #[source]
        source: anyhow::Error,
    },
    #[error(
        "capability discovery failed for server '{server_name}' ({server_id}) during {operation}; catalog={catalog_error:?}; existing={existing:?}; fresh={fresh:?}"
    )]
    DiscoveryFailed {
        server_id: String,
        server_name: String,
        operation: &'static str,
        kind: CapabilityType,
        catalog_error: Option<CatalogError>,
        existing: Option<Box<DiscoveryAttemptFailure>>,
        fresh: Option<Box<DiscoveryAttemptFailure>>,
    },
    #[error(
        "capability owner cleanup failed for server '{server_name}' ({server_id}) during {operation}, instance '{instance_id}': {error}"
    )]
    CleanupFailed {
        server_id: String,
        server_name: String,
        operation: &'static str,
        instance_id: String,
        connection_generation: Option<u64>,
        owner_source: OwnerSource,
        #[source]
        error: Box<CapabilityOwnerError>,
    },
    #[error(
        "capability projection failed for server '{server_name}' ({server_id}) during {operation}, instance '{instance_id}': {source}"
    )]
    ProjectionFailed {
        server_id: String,
        server_name: String,
        operation: &'static str,
        kind: CapabilityType,
        instance_id: String,
        connection_generation: Option<u64>,
        owner_source: OwnerSource,
        #[source]
        source: CapabilityProjectionFailure,
    },
}

impl CapabilityReadError {
    fn discovery_attempt_ms(
        existing: &Option<Box<DiscoveryAttemptFailure>>,
        fresh: &Option<Box<DiscoveryAttemptFailure>>,
        extractor: impl Fn(&DiscoveryAttemptFailure) -> Option<u128>,
    ) -> Option<u128> {
        fresh
            .as_deref()
            .and_then(&extractor)
            .or_else(|| existing.as_deref().and_then(&extractor))
    }

    pub(crate) fn connection_timeout_ms(&self) -> Option<u128> {
        if let Self::CleanupFailed { error, .. } = self {
            if let CapabilityOwnerError::Timeout { timeout_ms } = error.as_ref() {
                return Some(*timeout_ms);
            }
        }
        let Self::DiscoveryFailed { existing, fresh, .. } = self else {
            return None;
        };
        Self::discovery_attempt_ms(existing, fresh, DiscoveryAttemptFailure::connection_timeout_ms)
    }

    pub(crate) fn operation_timeout_ms(&self) -> Option<u128> {
        let Self::DiscoveryFailed { existing, fresh, .. } = self else {
            return None;
        };
        Self::discovery_attempt_ms(existing, fresh, DiscoveryAttemptFailure::operation_timeout_ms)
    }

    /// Surfaces an upstream authentication failure reason, when the discovery attempt or
    /// owner cleanup failed because the upstream server rejected our credentials.
    pub(crate) fn authentication_failure(&self) -> Option<(CapabilityAuthenticationFailureCode, &str)> {
        if let Self::CleanupFailed { error, .. } = self {
            if let CapabilityOwnerError::Authentication { code, reason } = error.as_ref() {
                return Some((*code, reason.as_str()));
            }
        }
        let Self::DiscoveryFailed { existing, fresh, .. } = self else {
            return None;
        };
        fresh
            .as_deref()
            .and_then(DiscoveryAttemptFailure::authentication_failure)
            .or_else(|| {
                existing
                    .as_deref()
                    .and_then(DiscoveryAttemptFailure::authentication_failure)
            })
    }

    pub(crate) fn authentication_reason(&self) -> Option<&str> {
        self.authentication_failure().map(|(_, reason)| reason)
    }
}

enum OwnerReadError {
    Attempt {
        failure: Box<DiscoveryAttemptFailure>,
        disposition: DiscoveryRetryDisposition,
    },
    Cleanup(Box<CapabilityReadError>),
    Projection(Box<CapabilityReadError>),
}

#[derive(Debug)]
pub(crate) struct DiscoveryAttemptFailure {
    pub instance_id: Option<String>,
    pub connection_generation: Option<u64>,
    pub source: crate::core::capability::connection_provider::OwnerSource,
    pub error: CapabilityAttemptError,
}

impl DiscoveryAttemptFailure {
    fn owner(
        source: crate::core::capability::connection_provider::OwnerSource,
        error: CapabilityOwnerError,
    ) -> Self {
        Self {
            instance_id: None,
            connection_generation: None,
            source,
            error: CapabilityAttemptError::Owner(error),
        }
    }

    fn from_owner(
        owner: &CapabilityOwner,
        error: CapabilityAttemptError,
    ) -> Self {
        Self {
            instance_id: Some(owner.instance_id.clone()),
            connection_generation: owner.connection_generation,
            source: owner.source,
            error,
        }
    }

    fn runtime(
        owner: &CapabilityOwner,
        error: RuntimeFailure,
    ) -> Self {
        Self::from_owner(owner, CapabilityAttemptError::Runtime(error))
    }

    fn commit(
        owner: &CapabilityOwner,
        error: CapabilityCommitFailure,
    ) -> Self {
        Self::from_owner(owner, CapabilityAttemptError::Commit(error))
    }

    fn connection_timeout_ms(&self) -> Option<u128> {
        match self.error {
            CapabilityAttemptError::Owner(CapabilityOwnerError::Timeout { timeout_ms }) => Some(timeout_ms),
            _ => None,
        }
    }

    fn operation_timeout_ms(&self) -> Option<u128> {
        match &self.error {
            CapabilityAttemptError::Runtime(failure) => failure.timeout_ms,
            _ => None,
        }
    }

    fn authentication_failure(&self) -> Option<(CapabilityAuthenticationFailureCode, &str)> {
        match &self.error {
            CapabilityAttemptError::Owner(CapabilityOwnerError::Authentication { code, reason }) => {
                Some((*code, reason.as_str()))
            }
            CapabilityAttemptError::Runtime(failure) => {
                failure.kind.authentication_code().zip(failure.message.as_deref())
            }
            _ => None,
        }
    }

    fn summary(&self) -> String {
        format!(
            "source={:?}, instance={:?}, generation={:?}, error={}",
            self.source, self.instance_id, self.connection_generation, self.error
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CapabilityAttemptError {
    #[error(transparent)]
    Owner(#[from] CapabilityOwnerError),
    #[error(transparent)]
    Runtime(#[from] RuntimeFailure),
    #[error(transparent)]
    Commit(#[from] CapabilityCommitFailure),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CapabilityCommitFailure {
    #[error(transparent)]
    ConfigurationChanged(#[from] crate::config::server::capabilities::CapabilityConfigurationChanged),
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error("capability catalog database commit failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("capability commit failed: {0}")]
    Operation(#[source] anyhow::Error),
}

impl CapabilityCommitFailure {
    fn from_anyhow(error: anyhow::Error) -> Self {
        match error.downcast::<crate::config::server::capabilities::CapabilityConfigurationChanged>() {
            Ok(error) => Self::ConfigurationChanged(error),
            Err(error) => match error.downcast::<CatalogError>() {
                Ok(error) => Self::Catalog(error),
                Err(error) => match error.downcast::<sqlx::Error>() {
                    Ok(error) => Self::Database(error),
                    Err(error) => Self::Operation(error),
                },
            },
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("capability projection failed: {0}")]
pub(crate) struct CapabilityProjectionFailure(#[source] anyhow::Error);

#[async_trait]
pub(crate) trait CapabilityReadBackend: Send + Sync {
    async fn coordination_fingerprint(
        &self,
        ctx: &ListCtx,
    ) -> Result<String, CapabilityReadError>;
    async fn try_cache_first(
        &self,
        ctx: &ListCtx,
    ) -> Result<Option<ListResult>, CapabilityReadError>;
    async fn persisted_kind_failure(
        &self,
        _ctx: &ListCtx,
    ) -> Result<Option<RuntimeFailure>, CapabilityReadError> {
        Ok(None)
    }
    async fn discover(
        &self,
        ctx: &ListCtx,
        owner: &CapabilityOwner,
    ) -> Result<CapabilityDiscoveryObservation, RuntimeFailure>;
    async fn canonical_server_name(
        &self,
        ctx: &ListCtx,
    ) -> Result<String, CapabilityReadError>;
    async fn commit_observation(
        &self,
        owner: &CapabilityOwner,
        observation: &CapabilityDiscoveryObservation,
    ) -> Result<i64, CapabilityCommitFailure>;
    async fn project_observation(
        &self,
        ctx: &ListCtx,
        owner: &CapabilityOwner,
        items: crate::core::capability::runtime::CapabilityItems,
        committed_revision: i64,
    ) -> Result<ListResult, CapabilityProjectionFailure>;
    async fn record_failure(
        &self,
        ctx: &ListCtx,
        server_name: &str,
        instance_id: Option<&str>,
        connection_generation: Option<u64>,
        reason: &str,
        failure: Option<&RuntimeFailure>,
    ) -> Result<(), CatalogError>;
    async fn record_failures(
        &self,
        ctx: &ListCtx,
        kinds: &[CapabilityType],
        server_name: &str,
        instance_id: Option<&str>,
        connection_generation: Option<u64>,
        reason: &str,
        failure: Option<&RuntimeFailure>,
    ) -> Result<(), CatalogError> {
        for kind in kinds {
            let mut failure_ctx = ctx.clone();
            failure_ctx.capability = *kind;
            self.record_failure(
                &failure_ctx,
                server_name,
                instance_id,
                connection_generation,
                reason,
                failure,
            )
            .await?;
        }
        Ok(())
    }
    async fn discover_all_kinds(
        &self,
        ctx: &ListCtx,
        owner: &CapabilityOwner,
    ) -> Result<runtime::CapabilityFullDiscoveryObservation, RuntimeFailure>;
    async fn commit_full_discovery(
        &self,
        owner: &CapabilityOwner,
        observation: &runtime::CapabilityFullDiscoveryObservation,
    ) -> Result<i64, CapabilityCommitFailure>;
}

pub(crate) struct CapabilityReadService {
    backend: Arc<dyn CapabilityReadBackend>,
    connection_provider: Arc<dyn CapabilityConnectionProvider>,
    coordination_scope: usize,
}

#[derive(Debug)]
pub(crate) struct CapabilityListsResult {
    pub tools: Result<ListResult, CapabilityReadError>,
    pub resources: Result<ListResult, CapabilityReadError>,
    pub prompts: Result<ListResult, CapabilityReadError>,
    pub resource_templates: Result<ListResult, CapabilityReadError>,
}

const ALL_CAPABILITY_TYPES: [CapabilityType; 4] = [
    CapabilityType::Tools,
    CapabilityType::Resources,
    CapabilityType::Prompts,
    CapabilityType::ResourceTemplates,
];

impl CapabilityListsResult {
    pub(crate) fn into_first_error(self) -> Option<CapabilityReadError> {
        self.tools
            .err()
            .or_else(|| self.resources.err())
            .or_else(|| self.prompts.err())
            .or_else(|| self.resource_templates.err())
    }

    pub(crate) fn has_failures(&self) -> bool {
        self.tools.is_err() || self.resources.is_err() || self.prompts.is_err() || self.resource_templates.is_err()
    }
}

struct ManagementDiscoveryCoordinator {
    gate: Mutex<()>,
    completed_generation: AtomicU64,
    last_outcome: StdMutex<Option<(u64, SharedManagementOutcome)>>,
}

impl ManagementDiscoveryCoordinator {
    fn new() -> Self {
        Self {
            gate: Mutex::new(()),
            completed_generation: AtomicU64::new(0),
            last_outcome: StdMutex::new(None),
        }
    }
}

static MANAGEMENT_DISCOVERY_LOCKS: OnceLock<DashMap<(usize, String, String), Arc<ManagementDiscoveryCoordinator>>> =
    OnceLock::new();

fn apply_batch_warm_meta(result: &mut ListResult) {
    if result.meta.source == "live" {
        return;
    }
    result.meta.cache_hit = false;
    result.meta.source = "live".to_string();
    result.meta.had_peer = true;
}

fn management_discovery_coordinator(
    coordination_scope: usize,
    server_id: &str,
    config_fingerprint: &str,
) -> Arc<ManagementDiscoveryCoordinator> {
    MANAGEMENT_DISCOVERY_LOCKS
        .get_or_init(DashMap::new)
        .entry((
            coordination_scope,
            server_id.to_string(),
            config_fingerprint.to_string(),
        ))
        .or_insert_with(|| Arc::new(ManagementDiscoveryCoordinator::new()))
        .clone()
}

fn complete_management_discovery(
    coordinator: &ManagementDiscoveryCoordinator,
    result: &Result<ManagementWarmOutcome, CapabilityReadError>,
) {
    let outcome = SharedManagementOutcome::from_result(result);
    let generation = coordinator.completed_generation.fetch_add(1, Ordering::AcqRel) + 1;
    *coordinator
        .last_outcome
        .lock()
        .expect("management discovery outcome mutex is not poisoned") = Some((generation, outcome));
}

#[derive(Debug, Clone)]
struct ScopedKindFailure {
    kind: CapabilityType,
    instance_id: String,
    connection_generation: Option<u64>,
    owner_source: OwnerSource,
    failure: RuntimeFailure,
}

#[derive(Debug, Clone)]
struct ManagementWarmOutcome {
    warmed: bool,
    failures: Vec<ScopedKindFailure>,
}

#[derive(Debug, Clone)]
enum SharedManagementOutcome {
    Success(ManagementWarmOutcome),
    Failure(Box<SharedCapabilityReadError>),
}

#[derive(Debug, Clone)]
enum SharedCapabilityReadError {
    CatalogUntrusted {
        server_id: String,
        source: SharedCatalogError,
    },
    CatalogOperation {
        server_id: String,
        message: String,
    },
    DiscoveryFailed {
        server_id: String,
        server_name: String,
        operation: &'static str,
        kind: CapabilityType,
        catalog_error: Option<SharedCatalogError>,
        existing: Option<SharedDiscoveryAttemptFailure>,
        fresh: Option<Box<SharedDiscoveryAttemptFailure>>,
    },
    CleanupFailed {
        server_id: String,
        server_name: String,
        operation: &'static str,
        instance_id: String,
        connection_generation: Option<u64>,
        owner_source: OwnerSource,
        error: CapabilityOwnerError,
    },
    ProjectionFailed {
        server_id: String,
        server_name: String,
        operation: &'static str,
        kind: CapabilityType,
        instance_id: String,
        connection_generation: Option<u64>,
        owner_source: OwnerSource,
        message: String,
    },
}

#[derive(Debug, Clone)]
struct SharedDiscoveryAttemptFailure {
    instance_id: Option<String>,
    connection_generation: Option<u64>,
    source: OwnerSource,
    error: SharedCapabilityAttemptError,
}

#[derive(Debug, Clone)]
enum SharedCapabilityAttemptError {
    Owner(CapabilityOwnerError),
    Runtime(RuntimeFailure),
    Commit(SharedCapabilityCommitFailure),
}

#[derive(Debug, Clone)]
enum SharedCapabilityCommitFailure {
    ConfigurationChanged(crate::config::server::capabilities::CapabilityConfigurationChanged),
    Catalog(SharedCatalogError),
    Database(String),
    Operation(String),
}

#[derive(Debug, Clone)]
enum SharedCatalogError {
    UnsupportedRecordVersion {
        actual: i64,
        expected: i64,
    },
    IncompatibleSchema {
        details: String,
    },
    InvalidValue {
        field: &'static str,
        value: String,
    },
    InvalidTimestamp {
        field: &'static str,
        value: String,
    },
    SnapshotNotFound {
        server_id: String,
    },
    ServerNotFound {
        server_id: String,
    },
    InvalidIdentity {
        identity_kind: &'static str,
        value: String,
    },
    CapabilityKindMismatch {
        source_kind: mcpmate_capability_store::CapabilityKind,
        payload_kind: mcpmate_capability_store::CapabilityKind,
    },
    UnsupportedEffectiveCapabilityFormat {
        actual: String,
        expected: &'static str,
    },
    IntegrityMismatch {
        identity: String,
    },
    DuplicateOrigin {
        server_id: String,
        kind: mcpmate_capability_store::CapabilityKind,
        origin_key: String,
    },
    InvalidSurfaceValue {
        field: &'static str,
        value: String,
    },
    DuplicateManifestRef {
        ref_id: String,
    },
    ConcurrencyConflict {
        entity: &'static str,
        id: String,
    },
    SurfaceNotFound {
        entity: &'static str,
        id: String,
    },
    Database(String),
    Json(String),
}

enum SharedManagementResolution {
    Success(ManagementWarmOutcome),
    Failure(Box<CapabilityReadError>),
}

impl SharedManagementOutcome {
    fn configuration_changed(&self) -> bool {
        let Self::Failure(error) = self else {
            return false;
        };
        let SharedCapabilityReadError::DiscoveryFailed { existing, fresh, .. } = error.as_ref() else {
            return false;
        };
        existing.as_ref().is_some_and(|failure| {
            matches!(
                failure.error,
                SharedCapabilityAttemptError::Commit(SharedCapabilityCommitFailure::ConfigurationChanged(_))
            )
        }) || fresh.as_deref().is_some_and(|failure| {
            matches!(
                failure.error,
                SharedCapabilityAttemptError::Commit(SharedCapabilityCommitFailure::ConfigurationChanged(_))
            )
        })
    }
}

impl From<&CapabilityReadError> for SharedCapabilityReadError {
    fn from(error: &CapabilityReadError) -> Self {
        match error {
            CapabilityReadError::CatalogUntrusted { server_id, source } => Self::CatalogUntrusted {
                server_id: server_id.clone(),
                source: SharedCatalogError::from(source),
            },
            CapabilityReadError::CatalogOperation { server_id, source } => Self::CatalogOperation {
                server_id: server_id.clone(),
                message: source.to_string(),
            },
            CapabilityReadError::DiscoveryFailed {
                server_id,
                server_name,
                operation,
                kind,
                catalog_error,
                existing,
                fresh,
            } => Self::DiscoveryFailed {
                server_id: server_id.clone(),
                server_name: server_name.clone(),
                operation,
                kind: *kind,
                catalog_error: catalog_error.as_ref().map(SharedCatalogError::from),
                existing: existing.as_deref().map(SharedDiscoveryAttemptFailure::from),
                fresh: fresh.as_deref().map(SharedDiscoveryAttemptFailure::from).map(Box::new),
            },
            CapabilityReadError::CleanupFailed {
                server_id,
                server_name,
                operation,
                instance_id,
                connection_generation,
                owner_source,
                error,
            } => Self::CleanupFailed {
                server_id: server_id.clone(),
                server_name: server_name.clone(),
                operation,
                instance_id: instance_id.clone(),
                connection_generation: *connection_generation,
                owner_source: *owner_source,
                error: error.as_ref().clone(),
            },
            CapabilityReadError::ProjectionFailed {
                server_id,
                server_name,
                operation,
                kind,
                instance_id,
                connection_generation,
                owner_source,
                source,
            } => Self::ProjectionFailed {
                server_id: server_id.clone(),
                server_name: server_name.clone(),
                operation,
                kind: *kind,
                instance_id: instance_id.clone(),
                connection_generation: *connection_generation,
                owner_source: *owner_source,
                message: source.to_string(),
            },
        }
    }
}

impl SharedCapabilityReadError {
    fn into_error(self) -> CapabilityReadError {
        match self {
            Self::CatalogUntrusted { server_id, source } => CapabilityReadError::CatalogUntrusted {
                server_id,
                source: source.into_error(),
            },
            Self::CatalogOperation { server_id, message } => CapabilityReadError::CatalogOperation {
                server_id,
                source: anyhow::anyhow!(message),
            },
            Self::DiscoveryFailed {
                server_id,
                server_name,
                operation,
                kind,
                catalog_error,
                existing,
                fresh,
            } => CapabilityReadError::DiscoveryFailed {
                server_id,
                server_name,
                operation,
                kind,
                catalog_error: catalog_error.map(SharedCatalogError::into_error),
                existing: existing.map(|failure| Box::new(failure.into_failure())),
                fresh: fresh.map(|failure| failure.into_failure()).map(Box::new),
            },
            Self::CleanupFailed {
                server_id,
                server_name,
                operation,
                instance_id,
                connection_generation,
                owner_source,
                error,
            } => CapabilityReadError::CleanupFailed {
                server_id,
                server_name,
                operation,
                instance_id,
                connection_generation,
                owner_source,
                error: Box::new(error),
            },
            Self::ProjectionFailed {
                server_id,
                server_name,
                operation,
                kind,
                instance_id,
                connection_generation,
                owner_source,
                message,
            } => CapabilityReadError::ProjectionFailed {
                server_id,
                server_name,
                operation,
                kind,
                instance_id,
                connection_generation,
                owner_source,
                source: CapabilityProjectionFailure(anyhow::anyhow!(message)),
            },
        }
    }
}

impl From<&DiscoveryAttemptFailure> for SharedDiscoveryAttemptFailure {
    fn from(failure: &DiscoveryAttemptFailure) -> Self {
        Self {
            instance_id: failure.instance_id.clone(),
            connection_generation: failure.connection_generation,
            source: failure.source,
            error: SharedCapabilityAttemptError::from(&failure.error),
        }
    }
}

impl SharedDiscoveryAttemptFailure {
    fn into_failure(self) -> DiscoveryAttemptFailure {
        DiscoveryAttemptFailure {
            instance_id: self.instance_id,
            connection_generation: self.connection_generation,
            source: self.source,
            error: self.error.into_error(),
        }
    }
}

impl From<&CapabilityAttemptError> for SharedCapabilityAttemptError {
    fn from(error: &CapabilityAttemptError) -> Self {
        match error {
            CapabilityAttemptError::Owner(error) => Self::Owner(error.clone()),
            CapabilityAttemptError::Runtime(error) => Self::Runtime(error.clone()),
            CapabilityAttemptError::Commit(error) => Self::Commit(SharedCapabilityCommitFailure::from(error)),
        }
    }
}

impl SharedCapabilityAttemptError {
    fn into_error(self) -> CapabilityAttemptError {
        match self {
            Self::Owner(error) => CapabilityAttemptError::Owner(error),
            Self::Runtime(error) => CapabilityAttemptError::Runtime(error),
            Self::Commit(error) => CapabilityAttemptError::Commit(error.into_error()),
        }
    }
}

impl From<&CapabilityCommitFailure> for SharedCapabilityCommitFailure {
    fn from(error: &CapabilityCommitFailure) -> Self {
        match error {
            CapabilityCommitFailure::ConfigurationChanged(error) => Self::ConfigurationChanged(error.clone()),
            CapabilityCommitFailure::Catalog(error) => Self::Catalog(SharedCatalogError::from(error)),
            CapabilityCommitFailure::Database(error) => Self::Database(error.to_string()),
            CapabilityCommitFailure::Operation(error) => Self::Operation(error.to_string()),
        }
    }
}

impl SharedCapabilityCommitFailure {
    fn into_error(self) -> CapabilityCommitFailure {
        match self {
            Self::ConfigurationChanged(error) => CapabilityCommitFailure::ConfigurationChanged(error),
            Self::Catalog(error) => CapabilityCommitFailure::Catalog(error.into_error()),
            Self::Database(message) => CapabilityCommitFailure::Database(sqlx::Error::Protocol(message)),
            Self::Operation(message) => CapabilityCommitFailure::Operation(anyhow::anyhow!(message)),
        }
    }
}

impl From<&CatalogError> for SharedCatalogError {
    fn from(error: &CatalogError) -> Self {
        match error {
            CatalogError::Database(error) => Self::Database(error.to_string()),
            CatalogError::Json(error) => Self::Json(error.to_string()),
            CatalogError::UnsupportedRecordVersion { actual, expected } => Self::UnsupportedRecordVersion {
                actual: *actual,
                expected: *expected,
            },
            CatalogError::IncompatibleSchema { details } => Self::IncompatibleSchema {
                details: details.clone(),
            },
            CatalogError::InvalidValue { field, value } => Self::InvalidValue {
                field,
                value: value.clone(),
            },
            CatalogError::InvalidTimestamp { field, value } => Self::InvalidTimestamp {
                field,
                value: value.clone(),
            },
            CatalogError::SnapshotNotFound { server_id } => Self::SnapshotNotFound {
                server_id: server_id.clone(),
            },
            CatalogError::ServerNotFound { server_id } => Self::ServerNotFound {
                server_id: server_id.clone(),
            },
            CatalogError::InvalidIdentity { identity_kind, value } => Self::InvalidIdentity {
                identity_kind,
                value: value.clone(),
            },
            CatalogError::CapabilityKindMismatch {
                source_kind,
                payload_kind,
            } => Self::CapabilityKindMismatch {
                source_kind: *source_kind,
                payload_kind: *payload_kind,
            },
            CatalogError::UnsupportedEffectiveCapabilityFormat { actual, expected } => {
                Self::UnsupportedEffectiveCapabilityFormat {
                    actual: actual.clone(),
                    expected,
                }
            }
            CatalogError::IntegrityMismatch { identity } => Self::IntegrityMismatch {
                identity: identity.clone(),
            },
            CatalogError::DuplicateOrigin {
                server_id,
                kind,
                origin_key,
            } => Self::DuplicateOrigin {
                server_id: server_id.clone(),
                kind: *kind,
                origin_key: origin_key.clone(),
            },
            CatalogError::InvalidSurfaceValue { field, value } => Self::InvalidSurfaceValue {
                field,
                value: value.clone(),
            },
            CatalogError::DuplicateManifestRef { ref_id } => Self::DuplicateManifestRef { ref_id: ref_id.clone() },
            CatalogError::ConcurrencyConflict { entity, id } => Self::ConcurrencyConflict { entity, id: id.clone() },
            CatalogError::SurfaceNotFound { entity, id } => Self::SurfaceNotFound { entity, id: id.clone() },
        }
    }
}

impl SharedCatalogError {
    fn into_error(self) -> CatalogError {
        match self {
            Self::Database(message) => CatalogError::Database(sqlx::Error::Protocol(message)),
            Self::Json(message) => CatalogError::Json(serde_json::Error::io(std::io::Error::other(message))),
            Self::UnsupportedRecordVersion { actual, expected } => {
                CatalogError::UnsupportedRecordVersion { actual, expected }
            }
            Self::IncompatibleSchema { details } => CatalogError::IncompatibleSchema { details },
            Self::InvalidValue { field, value } => CatalogError::InvalidValue { field, value },
            Self::InvalidTimestamp { field, value } => CatalogError::InvalidTimestamp { field, value },
            Self::SnapshotNotFound { server_id } => CatalogError::SnapshotNotFound { server_id },
            Self::ServerNotFound { server_id } => CatalogError::ServerNotFound { server_id },
            Self::InvalidIdentity { identity_kind, value } => CatalogError::InvalidIdentity { identity_kind, value },
            Self::CapabilityKindMismatch {
                source_kind,
                payload_kind,
            } => CatalogError::CapabilityKindMismatch {
                source_kind,
                payload_kind,
            },
            Self::UnsupportedEffectiveCapabilityFormat { actual, expected } => {
                CatalogError::UnsupportedEffectiveCapabilityFormat { actual, expected }
            }
            Self::IntegrityMismatch { identity } => CatalogError::IntegrityMismatch { identity },
            Self::DuplicateOrigin {
                server_id,
                kind,
                origin_key,
            } => CatalogError::DuplicateOrigin {
                server_id,
                kind,
                origin_key,
            },
            Self::InvalidSurfaceValue { field, value } => CatalogError::InvalidSurfaceValue { field, value },
            Self::DuplicateManifestRef { ref_id } => CatalogError::DuplicateManifestRef { ref_id },
            Self::ConcurrencyConflict { entity, id } => CatalogError::ConcurrencyConflict { entity, id },
            Self::SurfaceNotFound { entity, id } => CatalogError::SurfaceNotFound { entity, id },
        }
    }
}

impl SharedManagementOutcome {
    fn from_result(result: &Result<ManagementWarmOutcome, CapabilityReadError>) -> Self {
        match result {
            Ok(outcome) => Self::Success(outcome.clone()),
            Err(error) => Self::Failure(Box::new(SharedCapabilityReadError::from(error))),
        }
    }

    fn resolve(self) -> SharedManagementResolution {
        match self {
            Self::Success(mut outcome) => {
                outcome.warmed = false;
                SharedManagementResolution::Success(outcome)
            }
            Self::Failure(error) => SharedManagementResolution::Failure(Box::new(error.into_error())),
        }
    }
}

struct RuntimeCapabilityReadBackend {
    database: Arc<Database>,
    pool: Option<Arc<Mutex<UpstreamConnectionPool>>>,
}

impl RuntimeCapabilityReadBackend {
    async fn preserve_collision_side_effects(
        &self,
        error: &anyhow::Error,
    ) -> anyhow::Result<()> {
        let Some(collision) =
            crate::config::server::namespace_repair::record_capability_collision_from_error(&self.database.pool, error)
                .await?
        else {
            return Ok(());
        };
        let Some(pool) = self.pool.as_ref() else {
            return Ok(());
        };
        let mut pool = pool.lock().await;
        pool.block_server_after_capability_collision(&collision.server_id).await;
        pool.sync_servers_from_active_profile().await.with_context(|| {
            format!(
                "Failed to block server '{}' after external capability collision",
                collision.server_id
            )
        })?;
        Ok(())
    }
}

async fn apply_owner_runtime_failure(
    pool: Option<&Arc<Mutex<UpstreamConnectionPool>>>,
    owner: &CapabilityOwner,
    failure: &RuntimeFailure,
) {
    if owner.source != OwnerSource::Existing {
        return;
    }
    if let Some(pool) = pool {
        runtime::handle_runtime_failure(pool, &owner.server_id, &owner.instance_id, failure.clone()).await;
    }
}

#[async_trait]
impl CapabilityReadBackend for RuntimeCapabilityReadBackend {
    async fn coordination_fingerprint(
        &self,
        ctx: &ListCtx,
    ) -> Result<String, CapabilityReadError> {
        crate::config::server::capabilities::current_config_fingerprint(&self.database.pool, &ctx.server_id)
            .await
            .map_err(|source| CapabilityReadError::CatalogOperation {
                server_id: ctx.server_id.clone(),
                source,
            })
    }

    async fn try_cache_first(
        &self,
        ctx: &ListCtx,
    ) -> Result<Option<ListResult>, CapabilityReadError> {
        runtime::try_catalog_read(ctx, &self.database)
            .await
            .map_err(|error| match error {
                CatalogReadFailure::Catalog(source) => CapabilityReadError::CatalogUntrusted {
                    server_id: ctx.server_id.clone(),
                    source,
                },
                CatalogReadFailure::Operation(source) => CapabilityReadError::CatalogOperation {
                    server_id: ctx.server_id.clone(),
                    source,
                },
            })
    }

    async fn persisted_kind_failure(
        &self,
        ctx: &ListCtx,
    ) -> Result<Option<RuntimeFailure>, CapabilityReadError> {
        runtime::persisted_kind_failure(ctx, &self.database)
            .await
            .map_err(|error| match error {
                CatalogReadFailure::Catalog(source) => CapabilityReadError::CatalogUntrusted {
                    server_id: ctx.server_id.clone(),
                    source,
                },
                CatalogReadFailure::Operation(source) => CapabilityReadError::CatalogOperation {
                    server_id: ctx.server_id.clone(),
                    source,
                },
            })
    }

    async fn discover(
        &self,
        ctx: &ListCtx,
        owner: &CapabilityOwner,
    ) -> Result<CapabilityDiscoveryObservation, RuntimeFailure> {
        let result = runtime::discover_owner(ctx, owner).await;
        if let Err(failure) = &result {
            apply_owner_runtime_failure(self.pool.as_ref(), owner, failure).await;
        }
        result
    }

    async fn canonical_server_name(
        &self,
        ctx: &ListCtx,
    ) -> Result<String, CapabilityReadError> {
        let server = crate::config::server::get_server_by_id(&self.database.pool, &ctx.server_id)
            .await
            .map_err(|source| CapabilityReadError::CatalogOperation {
                server_id: ctx.server_id.clone(),
                source,
            })?
            .ok_or_else(|| CapabilityReadError::CatalogOperation {
                server_id: ctx.server_id.clone(),
                source: anyhow::anyhow!("server '{}' is missing from the canonical database", ctx.server_id),
            })?;
        crate::config::server::validate_server_namespace(&server.name).map_err(|source| {
            CapabilityReadError::CatalogOperation {
                server_id: ctx.server_id.clone(),
                source: source.into(),
            }
        })?;
        Ok(server.name)
    }

    async fn commit_observation(
        &self,
        owner: &CapabilityOwner,
        observation: &CapabilityDiscoveryObservation,
    ) -> Result<i64, CapabilityCommitFailure> {
        match runtime::commit_discovery_observation(owner, observation, &self.database).await {
            Ok(revision) => Ok(revision),
            Err(error) => {
                self.preserve_collision_side_effects(&error)
                    .await
                    .map_err(CapabilityCommitFailure::from_anyhow)?;
                Err(CapabilityCommitFailure::from_anyhow(error))
            }
        }
    }

    async fn project_observation(
        &self,
        ctx: &ListCtx,
        owner: &CapabilityOwner,
        items: crate::core::capability::runtime::CapabilityItems,
        committed_revision: i64,
    ) -> Result<ListResult, CapabilityProjectionFailure> {
        runtime::project_discovery_observation(ctx, owner, items, committed_revision, &self.database)
            .await
            .map_err(CapabilityProjectionFailure)
    }

    async fn record_failure(
        &self,
        ctx: &ListCtx,
        server_name: &str,
        instance_id: Option<&str>,
        connection_generation: Option<u64>,
        reason: &str,
        failure: Option<&RuntimeFailure>,
    ) -> Result<(), CatalogError> {
        runtime::record_discovery_failure(
            ctx,
            server_name,
            instance_id,
            connection_generation,
            reason,
            failure,
            &self.database,
        )
        .await
    }

    async fn record_failures(
        &self,
        ctx: &ListCtx,
        kinds: &[CapabilityType],
        server_name: &str,
        instance_id: Option<&str>,
        connection_generation: Option<u64>,
        reason: &str,
        failure: Option<&RuntimeFailure>,
    ) -> Result<(), CatalogError> {
        runtime::record_discovery_failures(
            ctx,
            server_name,
            kinds,
            instance_id,
            connection_generation,
            reason,
            failure,
            &self.database,
        )
        .await
    }

    async fn discover_all_kinds(
        &self,
        ctx: &ListCtx,
        owner: &CapabilityOwner,
    ) -> Result<runtime::CapabilityFullDiscoveryObservation, RuntimeFailure> {
        let result = runtime::discover_all_kinds_owner(ctx, owner).await;
        if let Err(failure) = &result {
            apply_owner_runtime_failure(self.pool.as_ref(), owner, failure).await;
        }
        result
    }

    async fn commit_full_discovery(
        &self,
        owner: &CapabilityOwner,
        observation: &runtime::CapabilityFullDiscoveryObservation,
    ) -> Result<i64, CapabilityCommitFailure> {
        match runtime::commit_full_discovery_observation(owner, observation, &self.database).await {
            Ok(revision) => Ok(revision),
            Err(error) => {
                self.preserve_collision_side_effects(&error)
                    .await
                    .map_err(CapabilityCommitFailure::from_anyhow)?;
                Err(CapabilityCommitFailure::from_anyhow(error))
            }
        }
    }
}

impl CapabilityReadService {
    pub(crate) fn from_runtime(
        database: Arc<Database>,
        pool: Arc<Mutex<UpstreamConnectionPool>>,
    ) -> Self {
        let connection_provider = Arc::new(PoolCapabilityConnectionProvider::new(pool.clone(), database.clone()));
        let coordination_scope = Arc::as_ptr(&database) as usize;
        Self::with_backend(
            Arc::new(RuntimeCapabilityReadBackend {
                database,
                pool: Some(pool),
            }),
            connection_provider,
            coordination_scope,
        )
    }

    fn with_backend(
        backend: Arc<dyn CapabilityReadBackend>,
        connection_provider: Arc<dyn CapabilityConnectionProvider>,
        coordination_scope: usize,
    ) -> Self {
        Self {
            backend,
            connection_provider,
            coordination_scope,
        }
    }

    pub(crate) async fn catalog_only(
        &self,
        ctx: &ListCtx,
    ) -> Result<Option<ListResult>, CapabilityReadError> {
        self.backend.try_cache_first(ctx).await
    }

    pub(crate) async fn list(
        &self,
        ctx: &ListCtx,
    ) -> Result<ListResult, CapabilityReadError> {
        let started = Instant::now();
        let mut catalog_error = None;
        if !matches!(ctx.refresh, Some(RefreshStrategy::Force)) {
            match self.backend.try_cache_first(ctx).await {
                Ok(Some(mut result)) => {
                    result.meta.duration_ms = started.elapsed().as_millis() as u64;
                    return Ok(result);
                }
                Ok(None) => {
                    if let Some(failure) = self.backend.persisted_kind_failure(ctx).await? {
                        let server_name = self.backend.canonical_server_name(ctx).await?;
                        return Err(persisted_kind_discovery_error(ctx, &server_name, failure));
                    }
                }
                Err(CapabilityReadError::CatalogUntrusted { server_id, source }) => {
                    if is_replaceable_catalog_error(&source) {
                        catalog_error = Some(source);
                    } else {
                        return Err(CapabilityReadError::CatalogUntrusted { server_id, source });
                    }
                }
                Err(error) => return Err(error),
            }
        }
        let server_name = self.backend.canonical_server_name(ctx).await?;
        let mut result = if ctx.validation_session.is_none() {
            let warm = self
                .ensure_management_catalog(ctx, &server_name, &mut catalog_error)
                .await?;
            if let Some(failure) = warm.failures.iter().find(|failure| failure.kind == ctx.capability) {
                return Err(scoped_kind_discovery_error(ctx, &server_name, failure));
            }
            let mut result = match self.backend.try_cache_first(ctx).await {
                Ok(Some(result)) => result,
                Ok(None) => {
                    return Err(discovery_error(
                        ctx,
                        &server_name,
                        catalog_error,
                        Some(DiscoveryAttemptFailure::owner(
                            OwnerSource::Fresh,
                            CapabilityOwnerError::Missing {
                                reason: format!(
                                    "Management catalog warm completed without a readable '{}' snapshot",
                                    capability_operation(ctx.capability)
                                ),
                            },
                        )),
                        None,
                    ));
                }
                Err(CapabilityReadError::CatalogUntrusted { server_id, source }) => {
                    return Err(CapabilityReadError::CatalogUntrusted { server_id, source });
                }
                Err(error) => return Err(error),
            };
            if warm.warmed {
                apply_batch_warm_meta(&mut result);
            }
            result
        } else {
            self.discover_existing_then_fresh(ctx, &server_name, catalog_error)
                .await?
        };
        result.meta.duration_ms = started.elapsed().as_millis() as u64;
        Ok(result)
    }

    pub(crate) async fn list_all_kinds(
        &self,
        server_id: &str,
        refresh: Option<RefreshStrategy>,
    ) -> Result<CapabilityListsResult, CapabilityReadError> {
        let contexts = ALL_CAPABILITY_TYPES.map(|kind| ListCtx {
            capability: kind,
            server_id: server_id.to_string(),
            refresh,
            operation_timeout: crate::core::transport::timeout_policy::DEFAULT_CAPABILITY_OPERATION_TIMEOUT,
            validation_session: None,
            runtime_identity: None,
            connection_selection: None,
            visibility_snapshot: None,
            name_domain: NameDomain::External,
        });
        let mut results = HashMap::new();
        let mut requires_warm = matches!(refresh, Some(RefreshStrategy::Force));
        let mut catalog_error = None;

        if !requires_warm {
            for ctx in &contexts {
                match self.backend.try_cache_first(ctx).await {
                    Ok(Some(result)) => {
                        results.insert(ctx.capability, Ok(result));
                    }
                    Ok(None) => {
                        if let Some(failure) = self.backend.persisted_kind_failure(ctx).await? {
                            let server_name = self.backend.canonical_server_name(ctx).await?;
                            results.insert(
                                ctx.capability,
                                Err(persisted_kind_discovery_error(ctx, &server_name, failure)),
                            );
                        } else {
                            requires_warm = true;
                        }
                    }
                    Err(CapabilityReadError::CatalogUntrusted { source, .. })
                        if is_replaceable_catalog_error(&source) =>
                    {
                        catalog_error = Some(source);
                        requires_warm = true;
                    }
                    Err(error) => return Err(error),
                }
            }
        }

        let warm = if requires_warm {
            let ctx = contexts
                .iter()
                .find(|ctx| !results.contains_key(&ctx.capability))
                .unwrap_or(&contexts[0]);
            let server_name = self.backend.canonical_server_name(ctx).await?;
            match self
                .ensure_management_catalog(ctx, &server_name, &mut catalog_error)
                .await
            {
                Ok(warm) => Some(warm),
                Err(error) => {
                    let Some((code, reason)) = error.authentication_failure() else {
                        return Err(error);
                    };
                    return Ok(authentication_failure_lists(&contexts, &server_name, code, reason));
                }
            }
        } else {
            None
        };

        if let Some(warm) = warm.as_ref() {
            let server_name = self.backend.canonical_server_name(&contexts[0]).await?;
            for ctx in &contexts {
                let result = if let Some(failure) = warm.failures.iter().find(|failure| failure.kind == ctx.capability)
                {
                    Err(scoped_kind_discovery_error(ctx, &server_name, failure))
                } else {
                    match self.backend.try_cache_first(ctx).await? {
                        Some(mut result) => {
                            if warm.warmed {
                                apply_batch_warm_meta(&mut result);
                            }
                            Ok(result)
                        }
                        None => {
                            if let Some(failure) = self.backend.persisted_kind_failure(ctx).await? {
                                Err(persisted_kind_discovery_error(ctx, &server_name, failure))
                            } else {
                                Err(discovery_error(
                                    ctx,
                                    &server_name,
                                    None,
                                    None,
                                    Some(DiscoveryAttemptFailure::owner(
                                        OwnerSource::Fresh,
                                        CapabilityOwnerError::Missing {
                                            reason: format!(
                                                "The shared management discovery completed without a readable '{}' snapshot",
                                                capability_operation(ctx.capability)
                                            ),
                                        },
                                    )),
                                ))
                            }
                        }
                    }
                };
                results.insert(ctx.capability, result);
            }
        }

        Ok(CapabilityListsResult {
            tools: results.remove(&CapabilityType::Tools).expect("tools list result"),
            resources: results
                .remove(&CapabilityType::Resources)
                .expect("resources list result"),
            prompts: results.remove(&CapabilityType::Prompts).expect("prompts list result"),
            resource_templates: results
                .remove(&CapabilityType::ResourceTemplates)
                .expect("resource templates list result"),
        })
    }

    async fn ensure_management_catalog(
        &self,
        ctx: &ListCtx,
        server_name: &str,
        catalog_error: &mut Option<CatalogError>,
    ) -> Result<ManagementWarmOutcome, CapabilityReadError> {
        let config_fingerprint = self.backend.coordination_fingerprint(ctx).await?;
        let coordinator =
            management_discovery_coordinator(self.coordination_scope, &ctx.server_id, &config_fingerprint);
        let observed_generation = coordinator.completed_generation.load(Ordering::Acquire);
        let _guard = coordinator.gate.lock().await;
        let current_generation = coordinator.completed_generation.load(Ordering::Acquire);
        if current_generation > observed_generation {
            let shared = coordinator
                .last_outcome
                .lock()
                .expect("management discovery outcome mutex is not poisoned")
                .as_ref()
                .filter(|(generation, _)| *generation == current_generation)
                .map(|(_, outcome)| outcome.clone())
                .expect("a completed management generation retains its outcome");
            if shared.configuration_changed() && !matches!(ctx.refresh, Some(RefreshStrategy::Force)) {
                let result = self.run_management_catalog(ctx, server_name, catalog_error).await;
                complete_management_discovery(&coordinator, &result);
                return result;
            }
            return match shared.resolve() {
                SharedManagementResolution::Success(outcome) => Ok(outcome),
                SharedManagementResolution::Failure(error) => Err(*error),
            };
        }

        let result = self.run_management_catalog(ctx, server_name, catalog_error).await;
        complete_management_discovery(&coordinator, &result);
        result
    }

    async fn release_management_owner(
        &self,
        ctx: &ListCtx,
        server_name: &str,
        owner: CapabilityOwner,
    ) -> Result<(), CapabilityReadError> {
        let instance_id = owner.instance_id.clone();
        let connection_generation = owner.connection_generation;
        let owner_source = owner.source;
        self.connection_provider
            .release_owner(owner)
            .await
            .map_err(|error| CapabilityReadError::CleanupFailed {
                server_id: ctx.server_id.clone(),
                server_name: server_name.to_string(),
                operation: "management catalog warm",
                instance_id,
                connection_generation,
                owner_source,
                error: Box::new(error),
            })
    }

    async fn run_management_catalog(
        &self,
        ctx: &ListCtx,
        server_name: &str,
        catalog_error: &mut Option<CatalogError>,
    ) -> Result<ManagementWarmOutcome, CapabilityReadError> {
        if !matches!(ctx.refresh, Some(RefreshStrategy::Force)) && self.backend.try_cache_first(ctx).await?.is_some() {
            return Ok(ManagementWarmOutcome {
                warmed: false,
                failures: Vec::new(),
            });
        }
        let fresh_owner = match self.connection_provider.fresh_owner(ctx).await {
            Ok(owner) => owner,
            Err(error) => {
                if matches!(error, CapabilityOwnerError::Backoff { .. })
                    && let Some(failure) = self.backend.persisted_kind_failure(ctx).await?
                    && failure.kind.authentication_code().is_some()
                {
                    return Err(persisted_kind_discovery_error(ctx, server_name, failure));
                }
                let reason = error.to_string();
                if let CapabilityOwnerError::Authentication { code, reason: detail } = &error {
                    let failure = RuntimeFailure {
                        kind: RuntimeFailureKind::from_authentication_code(*code),
                        message: Some(detail.clone()),
                        timeout_ms: None,
                    };
                    self.record_failures(
                        ctx,
                        &ALL_CAPABILITY_TYPES,
                        server_name,
                        None,
                        None,
                        &reason,
                        Some(&failure),
                        catalog_error,
                    )
                    .await;
                } else if !matches!(error, CapabilityOwnerError::Backoff { .. }) {
                    self.record_failures(
                        ctx,
                        &ALL_CAPABILITY_TYPES,
                        server_name,
                        None,
                        None,
                        &reason,
                        None,
                        catalog_error,
                    )
                    .await;
                } else {
                    tracing::debug!(
                        server_id = %ctx.server_id,
                        reason = %reason,
                        "Capability owner acquisition deferred without replacing persisted failure evidence"
                    );
                }
                return Err(discovery_error(
                    ctx,
                    server_name,
                    catalog_error.take(),
                    None,
                    Some(DiscoveryAttemptFailure::owner(OwnerSource::Fresh, error)),
                ));
            }
        };

        let observation = match self.backend.discover_all_kinds(ctx, &fresh_owner).await {
            Ok(observation) => observation,
            Err(failure) => {
                let reason = owner_attempt_reason(&fresh_owner, &failure);
                self.record_failure(
                    ctx,
                    server_name,
                    Some(&fresh_owner.instance_id),
                    fresh_owner.connection_generation,
                    &reason,
                    Some(&failure),
                    catalog_error,
                )
                .await;
                let discovery_failure = DiscoveryAttemptFailure::runtime(&fresh_owner, failure);
                self.release_management_owner(ctx, server_name, fresh_owner).await?;
                return Err(discovery_error(
                    ctx,
                    server_name,
                    catalog_error.take(),
                    None,
                    Some(discovery_failure),
                ));
            }
        };

        let scoped_failures = observation
            .failures
            .iter()
            .map(|failure| ScopedKindFailure {
                kind: failure.kind,
                instance_id: fresh_owner.instance_id.clone(),
                connection_generation: fresh_owner.connection_generation,
                owner_source: fresh_owner.source,
                failure: failure.failure.clone(),
            })
            .collect();

        if let Err(commit_failure) = self.backend.commit_full_discovery(&fresh_owner, &observation).await {
            let discovery_failure = DiscoveryAttemptFailure::commit(&fresh_owner, commit_failure);
            let reason = format!(
                "owner '{}' generation {:?}: {}",
                fresh_owner.instance_id, fresh_owner.connection_generation, discovery_failure.error
            );
            self.record_failure(
                ctx,
                server_name,
                Some(&fresh_owner.instance_id),
                fresh_owner.connection_generation,
                &reason,
                None,
                catalog_error,
            )
            .await;
            self.release_management_owner(ctx, server_name, fresh_owner).await?;
            return Err(discovery_error(
                ctx,
                server_name,
                catalog_error.take(),
                None,
                Some(discovery_failure),
            ));
        }

        self.release_management_owner(ctx, server_name, fresh_owner).await?;

        Ok(ManagementWarmOutcome {
            warmed: true,
            failures: scoped_failures,
        })
    }

    async fn discover_existing_then_fresh(
        &self,
        ctx: &ListCtx,
        server_name: &str,
        mut catalog_error: Option<CatalogError>,
    ) -> Result<ListResult, CapabilityReadError> {
        let (existing_error, disposition) = match self.connection_provider.existing_owner(ctx).await {
            Ok(Some(owner)) => match self
                .discover_with_owner(ctx, server_name, owner, &mut catalog_error)
                .await
            {
                Ok(result) => return Ok(result),
                Err(OwnerReadError::Attempt { failure, disposition }) => (Some(*failure), disposition),
                Err(OwnerReadError::Cleanup(error) | OwnerReadError::Projection(error)) => {
                    return Err(*error);
                }
            },
            Ok(None) => {
                let error = CapabilityOwnerError::Missing {
                    reason: match ctx.validation_session.as_ref() {
                        Some(session_id) => format!(
                            "Validation session '{}' has no connected peer for server '{}'",
                            session_id, ctx.server_id
                        ),
                        None => format!("No connected capability peer for server '{}'", ctx.server_id),
                    },
                };
                (
                    Some(DiscoveryAttemptFailure::owner(OwnerSource::Existing, error)),
                    DiscoveryRetryDisposition::FreshOnce,
                )
            }
            Err(error) => {
                let disposition = error.retry_disposition();
                let reason = error.to_string();
                self.record_failure(ctx, server_name, None, None, &reason, None, &mut catalog_error)
                    .await;
                (
                    Some(DiscoveryAttemptFailure::owner(OwnerSource::Existing, error)),
                    disposition,
                )
            }
        };

        if disposition == DiscoveryRetryDisposition::DoNotRetry {
            return Err(discovery_error(ctx, server_name, catalog_error, existing_error, None));
        }

        let fresh_owner = match self.connection_provider.fresh_owner(ctx).await {
            Ok(owner) => owner,
            Err(error) => {
                let reason = error.to_string();
                self.record_failure(ctx, server_name, None, None, &reason, None, &mut catalog_error)
                    .await;
                return Err(discovery_error(
                    ctx,
                    server_name,
                    catalog_error,
                    existing_error,
                    Some(DiscoveryAttemptFailure::owner(OwnerSource::Fresh, error)),
                ));
            }
        };

        match self
            .discover_with_owner(ctx, server_name, fresh_owner, &mut catalog_error)
            .await
        {
            Ok(result) => Ok(result),
            Err(OwnerReadError::Attempt { failure, .. }) => Err(discovery_error(
                ctx,
                server_name,
                catalog_error,
                existing_error,
                Some(*failure),
            )),
            Err(OwnerReadError::Cleanup(error) | OwnerReadError::Projection(error)) => Err(*error),
        }
    }

    async fn discover_with_owner(
        &self,
        ctx: &ListCtx,
        server_name: &str,
        owner: CapabilityOwner,
        catalog_error: &mut Option<CatalogError>,
    ) -> Result<ListResult, OwnerReadError> {
        let attempt = self.backend.discover(ctx, &owner).await;
        match attempt {
            Ok(observation) => {
                let committed_revision = match self.backend.commit_observation(&owner, &observation).await {
                    Ok(revision) => revision,
                    Err(failure) => {
                        let reason = owner_attempt_reason(&owner, &failure);
                        self.record_failure(
                            ctx,
                            server_name,
                            Some(&owner.instance_id),
                            owner.connection_generation,
                            &reason,
                            None,
                            catalog_error,
                        )
                        .await;
                        let attempt = DiscoveryAttemptFailure::commit(&owner, failure);
                        self.release_after_failed_attempt(ctx, owner).await;
                        return Err(OwnerReadError::Attempt {
                            failure: Box::new(attempt),
                            disposition: DiscoveryRetryDisposition::DoNotRetry,
                        });
                    }
                };

                let result = self
                    .backend
                    .project_observation(ctx, &owner, observation.items, committed_revision)
                    .await;
                let source = owner.source;
                let instance_id = owner.instance_id.clone();
                let connection_generation = owner.connection_generation;
                match result {
                    Ok(result) => match self.connection_provider.release_owner(owner).await {
                        Ok(()) => Ok(result),
                        Err(error) => Err(OwnerReadError::Cleanup(Box::new(CapabilityReadError::CleanupFailed {
                            server_id: ctx.server_id.clone(),
                            server_name: server_name.to_string(),
                            operation: capability_operation(ctx.capability),
                            instance_id,
                            connection_generation,
                            owner_source: source,
                            error: Box::new(error),
                        }))),
                    },
                    Err(projection_failure) => {
                        if let Err(error) = self.connection_provider.release_owner(owner).await {
                            tracing::warn!(
                                server_id = %ctx.server_id,
                                capability = ?ctx.capability,
                                error = %error,
                                "Capability owner cleanup failed after projection failure"
                            );
                        }
                        Err(OwnerReadError::Projection(Box::new(
                            CapabilityReadError::ProjectionFailed {
                                server_id: ctx.server_id.clone(),
                                server_name: server_name.to_string(),
                                operation: capability_operation(ctx.capability),
                                kind: ctx.capability,
                                instance_id,
                                connection_generation,
                                owner_source: source,
                                source: projection_failure,
                            },
                        )))
                    }
                }
            }
            Err(failure) => {
                let disposition = failure.kind.retry_disposition();
                let reason = owner_attempt_reason(&owner, &failure);
                self.record_owner_failure(ctx, server_name, &owner, &reason, &failure, catalog_error)
                    .await;
                let attempt = DiscoveryAttemptFailure::runtime(&owner, failure);
                self.release_after_failed_attempt(ctx, owner).await;
                Err(OwnerReadError::Attempt {
                    failure: Box::new(attempt),
                    disposition,
                })
            }
        }
    }

    async fn release_after_failed_attempt(
        &self,
        ctx: &ListCtx,
        owner: CapabilityOwner,
    ) {
        if let Err(error) = self.connection_provider.release_owner(owner).await {
            tracing::warn!(
                server_id = %ctx.server_id,
                capability = ?ctx.capability,
                error = %error,
                "Capability owner cleanup failed after discovery"
            );
        }
    }

    async fn record_owner_failure(
        &self,
        ctx: &ListCtx,
        server_name: &str,
        owner: &CapabilityOwner,
        reason: &str,
        failure: &RuntimeFailure,
        catalog_error: &mut Option<CatalogError>,
    ) {
        self.record_failure(
            ctx,
            server_name,
            Some(&owner.instance_id),
            owner.connection_generation,
            reason,
            Some(failure),
            catalog_error,
        )
        .await;
    }

    async fn record_failure(
        &self,
        ctx: &ListCtx,
        server_name: &str,
        instance_id: Option<&str>,
        connection_generation: Option<u64>,
        reason: &str,
        failure: Option<&RuntimeFailure>,
        catalog_error: &mut Option<CatalogError>,
    ) {
        if let Err(error) = self
            .backend
            .record_failure(ctx, server_name, instance_id, connection_generation, reason, failure)
            .await
        {
            tracing::warn!(
                server_id = %ctx.server_id,
                capability = ?ctx.capability,
                error = %error,
                "Capability failure evidence could not be persisted"
            );
            if catalog_error.is_none() {
                *catalog_error = Some(error);
            }
        }
    }

    async fn record_failures(
        &self,
        ctx: &ListCtx,
        kinds: &[CapabilityType],
        server_name: &str,
        instance_id: Option<&str>,
        connection_generation: Option<u64>,
        reason: &str,
        failure: Option<&RuntimeFailure>,
        catalog_error: &mut Option<CatalogError>,
    ) {
        if let Err(error) = self
            .backend
            .record_failures(
                ctx,
                kinds,
                server_name,
                instance_id,
                connection_generation,
                reason,
                failure,
            )
            .await
        {
            tracing::warn!(
                server_id = %ctx.server_id,
                kinds = ?kinds,
                error = %error,
                "Capability failure evidence could not be persisted"
            );
            if catalog_error.is_none() {
                *catalog_error = Some(error);
            }
        }
    }
}

fn is_replaceable_catalog_error(error: &CatalogError) -> bool {
    matches!(
        error,
        CatalogError::Json(_)
            | CatalogError::UnsupportedRecordVersion { .. }
            | CatalogError::InvalidValue { .. }
            | CatalogError::InvalidTimestamp { .. }
    )
}

fn owner_attempt_reason(
    owner: &CapabilityOwner,
    detail: &impl std::fmt::Display,
) -> String {
    format!(
        "owner '{}' generation {:?}: {}",
        owner.instance_id, owner.connection_generation, detail
    )
}

fn discovery_error(
    ctx: &ListCtx,
    server_name: &str,
    catalog_error: Option<CatalogError>,
    existing: Option<DiscoveryAttemptFailure>,
    fresh: Option<DiscoveryAttemptFailure>,
) -> CapabilityReadError {
    tracing::debug!(
        server_id = %ctx.server_id,
        capability = ?ctx.capability,
        existing = ?existing.as_ref().map(DiscoveryAttemptFailure::summary),
        fresh = ?fresh.as_ref().map(DiscoveryAttemptFailure::summary),
        "Capability discovery exhausted its permitted owners"
    );
    CapabilityReadError::DiscoveryFailed {
        server_id: ctx.server_id.clone(),
        server_name: server_name.to_string(),
        operation: capability_operation(ctx.capability),
        kind: ctx.capability,
        catalog_error,
        existing: existing.map(Box::new),
        fresh: fresh.map(Box::new),
    }
}

fn authentication_failure_lists(
    contexts: &[ListCtx; 4],
    server_name: &str,
    code: CapabilityAuthenticationFailureCode,
    reason: &str,
) -> CapabilityListsResult {
    let error = |ctx: &ListCtx| {
        discovery_error(
            ctx,
            server_name,
            None,
            None,
            Some(DiscoveryAttemptFailure::owner(
                OwnerSource::Fresh,
                CapabilityOwnerError::Authentication {
                    code,
                    reason: reason.to_string(),
                },
            )),
        )
    };
    CapabilityListsResult {
        tools: Err(error(&contexts[0])),
        resources: Err(error(&contexts[1])),
        prompts: Err(error(&contexts[2])),
        resource_templates: Err(error(&contexts[3])),
    }
}

fn scoped_kind_discovery_error(
    ctx: &ListCtx,
    server_name: &str,
    failure: &ScopedKindFailure,
) -> CapabilityReadError {
    discovery_error(
        ctx,
        server_name,
        None,
        None,
        Some(DiscoveryAttemptFailure {
            instance_id: Some(failure.instance_id.clone()),
            connection_generation: failure.connection_generation,
            source: failure.owner_source,
            error: CapabilityAttemptError::Runtime(failure.failure.clone()),
        }),
    )
}

fn persisted_kind_discovery_error(
    ctx: &ListCtx,
    server_name: &str,
    failure: RuntimeFailure,
) -> CapabilityReadError {
    discovery_error(
        ctx,
        server_name,
        None,
        None,
        Some(DiscoveryAttemptFailure {
            instance_id: None,
            connection_generation: None,
            source: OwnerSource::Fresh,
            error: CapabilityAttemptError::Runtime(failure),
        }),
    )
}

const fn capability_operation(capability: CapabilityType) -> &'static str {
    match capability {
        CapabilityType::Tools => "tools/list",
        CapabilityType::Prompts => "prompts/list",
        CapabilityType::Resources => "resources/list",
        CapabilityType::ResourceTemplates => "resources/templates/list",
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, VecDeque},
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use async_trait::async_trait;
    use mcpmate_capability_store::{
        CapabilityCatalog, CapabilityKind as CatalogKind, CapabilityPayload, CatalogError, DeclarationState,
        InventoryState, KindFailureKind, KindObservation, SnapshotState, SqliteCapabilityCatalog,
    };
    use rmcp::{
        ServerHandler, ServiceExt,
        model::Tool,
        service::{Peer, RoleClient, RunningService},
    };
    use tokio::sync::Mutex;

    use super::{
        CapabilityAttemptError, CapabilityCommitFailure, CapabilityProjectionFailure, CapabilityReadBackend,
        CapabilityReadError, CapabilityReadService, DiscoveryAttemptFailure, RuntimeCapabilityReadBackend,
        apply_owner_runtime_failure,
    };
    use crate::config::{
        database::Database,
        models::{Server, ServerTransportDraft},
        server::upsert_server_definition,
    };
    use crate::core::capability::{
        CapabilityType,
        connection_provider::{
            CapabilityAuthenticationFailureCode, CapabilityConnectionProvider, CapabilityOwner, CapabilityOwnerError,
            OwnerSource,
        },
        runtime::{
            self, CapabilityDiscoveryObservation, CapabilityItems, ListCtx, ListResult, Meta, NameDomain,
            RefreshStrategy, RuntimeFailure, RuntimeFailureKind,
        },
    };
    use crate::core::{
        events::{Event, EventBus},
        models::Config,
        pool::{CapSyncFlags, FailureKind, UpstreamConnection, UpstreamConnectionPool},
        transport::client::UpstreamClientHandler,
    };

    #[derive(Clone, Default)]
    struct TestServer;

    impl ServerHandler for TestServer {}

    struct TestPeerFixture {
        peer: Peer<RoleClient>,
        client: Option<RunningService<RoleClient, ()>>,
        server_task: tokio::task::JoinHandle<()>,
    }

    impl TestPeerFixture {
        async fn shutdown(mut self) {
            drop(self.peer);
            let mut client = self.client.take().expect("test client owner should exist");
            client.close().await.expect("test client should close");
            self.server_task.await.expect("test server task should join");
        }
    }

    #[derive(Clone, Debug)]
    struct EvidenceRecord {
        server_id: String,
        kind: CapabilityType,
        instance_id: Option<String>,
        connection_generation: Option<u64>,
        reason: String,
    }

    struct FakeBackend {
        coordination_fingerprint: std::sync::Mutex<String>,
        cache_result: Mutex<Option<Result<Option<ListResult>, CapabilityReadError>>>,
        cache_calls: AtomicUsize,
        persisted_kind_calls: AtomicUsize,
        canonical_name_calls: AtomicUsize,
        discovery_calls: AtomicUsize,
        projection_calls: AtomicUsize,
        discoveries: Mutex<VecDeque<Result<CapabilityDiscoveryObservation, RuntimeFailure>>>,
        warmed_cache: Mutex<HashMap<CapabilityType, ListResult>>,
        evidence: Mutex<Vec<EvidenceRecord>>,
        evidence_error: Mutex<Option<CatalogError>>,
        projection_error: Mutex<Option<CapabilityProjectionFailure>>,
        commit_error: Mutex<Option<CapabilityCommitFailure>>,
        commits: AtomicUsize,
        discovery_started: tokio::sync::Semaphore,
        discovery_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    }

    struct CommitFailureBackend {
        runtime: RuntimeCapabilityReadBackend,
        observation: Mutex<Option<CapabilityDiscoveryObservation>>,
        discovery_failure: Mutex<Option<RuntimeFailure>>,
        projection_error: Mutex<Option<CapabilityProjectionFailure>>,
    }

    #[async_trait]
    impl CapabilityReadBackend for CommitFailureBackend {
        async fn coordination_fingerprint(
            &self,
            ctx: &ListCtx,
        ) -> Result<String, CapabilityReadError> {
            self.runtime.coordination_fingerprint(ctx).await
        }

        async fn try_cache_first(
            &self,
            _ctx: &ListCtx,
        ) -> Result<Option<ListResult>, CapabilityReadError> {
            Ok(None)
        }

        async fn discover(
            &self,
            _ctx: &ListCtx,
            _owner: &CapabilityOwner,
        ) -> Result<CapabilityDiscoveryObservation, RuntimeFailure> {
            if let Some(failure) = self.discovery_failure.lock().await.take() {
                return Err(failure);
            }
            Ok(self
                .observation
                .lock()
                .await
                .take()
                .expect("commit fixture observation"))
        }

        async fn canonical_server_name(
            &self,
            ctx: &ListCtx,
        ) -> Result<String, CapabilityReadError> {
            self.runtime.canonical_server_name(ctx).await
        }

        async fn commit_observation(
            &self,
            owner: &CapabilityOwner,
            observation: &CapabilityDiscoveryObservation,
        ) -> Result<i64, CapabilityCommitFailure> {
            self.runtime.commit_observation(owner, observation).await
        }

        async fn project_observation(
            &self,
            ctx: &ListCtx,
            owner: &CapabilityOwner,
            items: crate::core::capability::runtime::CapabilityItems,
            committed_revision: i64,
        ) -> Result<ListResult, CapabilityProjectionFailure> {
            if let Some(error) = self.projection_error.lock().await.take() {
                return Err(error);
            }
            self.runtime
                .project_observation(ctx, owner, items, committed_revision)
                .await
        }

        async fn record_failure(
            &self,
            ctx: &ListCtx,
            server_name: &str,
            instance_id: Option<&str>,
            connection_generation: Option<u64>,
            reason: &str,
            failure: Option<&RuntimeFailure>,
        ) -> Result<(), CatalogError> {
            self.runtime
                .record_failure(ctx, server_name, instance_id, connection_generation, reason, failure)
                .await
        }

        async fn discover_all_kinds(
            &self,
            _ctx: &ListCtx,
            _owner: &CapabilityOwner,
        ) -> Result<runtime::CapabilityFullDiscoveryObservation, RuntimeFailure> {
            Ok(observation_to_full(
                self.observation
                    .lock()
                    .await
                    .take()
                    .expect("commit fixture observation"),
            ))
        }

        async fn commit_full_discovery(
            &self,
            owner: &CapabilityOwner,
            observation: &runtime::CapabilityFullDiscoveryObservation,
        ) -> Result<i64, CapabilityCommitFailure> {
            self.runtime.commit_full_discovery(owner, observation).await
        }
    }

    impl FakeBackend {
        fn new(cache_result: Result<Option<ListResult>, CapabilityReadError>) -> Self {
            Self {
                coordination_fingerprint: std::sync::Mutex::new("test-config".to_string()),
                cache_result: Mutex::new(Some(cache_result)),
                cache_calls: AtomicUsize::new(0),
                persisted_kind_calls: AtomicUsize::new(0),
                canonical_name_calls: AtomicUsize::new(0),
                discovery_calls: AtomicUsize::new(0),
                projection_calls: AtomicUsize::new(0),
                discoveries: Mutex::new(VecDeque::new()),
                warmed_cache: Mutex::new(HashMap::new()),
                evidence: Mutex::new(Vec::new()),
                evidence_error: Mutex::new(None),
                projection_error: Mutex::new(None),
                commit_error: Mutex::new(None),
                commits: AtomicUsize::new(0),
                discovery_started: tokio::sync::Semaphore::new(0),
                discovery_gate: Mutex::new(None),
            }
        }

        async fn push_discovery(
            &self,
            result: Result<CapabilityDiscoveryObservation, RuntimeFailure>,
        ) {
            self.discoveries.lock().await.push_back(result);
        }

        async fn pause_discovery_with(
            &self,
            gate: Arc<tokio::sync::Semaphore>,
        ) {
            *self.discovery_gate.lock().await = Some(gate);
        }

        fn set_coordination_fingerprint(
            &self,
            fingerprint: impl Into<String>,
        ) {
            *self
                .coordination_fingerprint
                .lock()
                .expect("coordination fingerprint mutex is not poisoned") = fingerprint.into();
        }

        async fn fail_next_commit(&self) {
            self.fail_next_commit_with(CapabilityCommitFailure::Operation(anyhow::anyhow!(
                "forced management commit failure"
            )))
            .await;
        }

        async fn fail_next_commit_with(
            &self,
            error: CapabilityCommitFailure,
        ) {
            *self.commit_error.lock().await = Some(error);
        }
    }

    fn observation_to_full(observation: CapabilityDiscoveryObservation) -> runtime::CapabilityFullDiscoveryObservation {
        let CapabilityDiscoveryObservation {
            items,
            flags,
            kind_states,
        } = observation;
        let mut full = runtime::CapabilityFullDiscoveryObservation {
            tools: Vec::new(),
            resources: Vec::new(),
            prompts: Vec::new(),
            templates: Vec::new(),
            flags,
            kind_states,
            failures: Vec::new(),
        };
        match items {
            CapabilityItems::Tools(items) => full.tools = items,
            CapabilityItems::Resources(items) => full.resources = items,
            CapabilityItems::Prompts(items) => full.prompts = items,
            CapabilityItems::ResourceTemplates(items) => full.templates = items,
        }
        full
    }

    fn warmed_result(
        owner_source: OwnerSource,
        capability: CapabilityType,
    ) -> ListResult {
        let source = match owner_source {
            OwnerSource::Existing => "live_existing",
            OwnerSource::Fresh => "live_fresh",
            OwnerSource::Validation => "live_validation",
        }
        .to_string();
        let items = match capability {
            CapabilityType::Tools => CapabilityItems::Tools(Vec::new()),
            CapabilityType::Resources => CapabilityItems::Resources(Vec::new()),
            CapabilityType::Prompts => CapabilityItems::Prompts(Vec::new()),
            CapabilityType::ResourceTemplates => CapabilityItems::ResourceTemplates(Vec::new()),
        };
        ListResult {
            items,
            meta: Meta {
                cache_hit: false,
                source,
                duration_ms: 0,
                had_peer: true,
            },
        }
    }

    #[async_trait]
    impl CapabilityReadBackend for FakeBackend {
        async fn coordination_fingerprint(
            &self,
            _ctx: &ListCtx,
        ) -> Result<String, CapabilityReadError> {
            Ok(self
                .coordination_fingerprint
                .lock()
                .expect("coordination fingerprint mutex is not poisoned")
                .clone())
        }

        async fn try_cache_first(
            &self,
            ctx: &ListCtx,
        ) -> Result<Option<ListResult>, CapabilityReadError> {
            self.cache_calls.fetch_add(1, Ordering::Relaxed);
            if let Some(result) = self.warmed_cache.lock().await.get(&ctx.capability).cloned() {
                return Ok(Some(result));
            }
            self.cache_result.lock().await.take().unwrap_or(Ok(None))
        }

        async fn persisted_kind_failure(
            &self,
            _ctx: &ListCtx,
        ) -> Result<Option<RuntimeFailure>, CapabilityReadError> {
            self.persisted_kind_calls.fetch_add(1, Ordering::Relaxed);
            Ok(None)
        }

        async fn discover(
            &self,
            _ctx: &ListCtx,
            _owner: &CapabilityOwner,
        ) -> Result<CapabilityDiscoveryObservation, RuntimeFailure> {
            self.discovery_calls.fetch_add(1, Ordering::Relaxed);
            self.discoveries
                .lock()
                .await
                .pop_front()
                .expect("a discovery result must be configured")
        }

        async fn canonical_server_name(
            &self,
            _ctx: &ListCtx,
        ) -> Result<String, CapabilityReadError> {
            self.canonical_name_calls.fetch_add(1, Ordering::Relaxed);
            Ok("docs".to_string())
        }

        async fn commit_observation(
            &self,
            _owner: &CapabilityOwner,
            _observation: &CapabilityDiscoveryObservation,
        ) -> Result<i64, CapabilityCommitFailure> {
            self.commits.fetch_add(1, Ordering::Relaxed);
            Ok(1)
        }

        async fn project_observation(
            &self,
            _ctx: &ListCtx,
            owner: &CapabilityOwner,
            items: crate::core::capability::runtime::CapabilityItems,
            _committed_revision: i64,
        ) -> Result<ListResult, CapabilityProjectionFailure> {
            self.projection_calls.fetch_add(1, Ordering::Relaxed);
            if let Some(error) = self.projection_error.lock().await.take() {
                return Err(error);
            }
            Ok(ListResult {
                items,
                meta: Meta {
                    cache_hit: false,
                    source: match owner.source {
                        OwnerSource::Existing => "live_existing",
                        OwnerSource::Fresh => "live_fresh",
                        OwnerSource::Validation => "live_validation",
                    }
                    .to_string(),
                    duration_ms: 0,
                    had_peer: true,
                },
            })
        }

        async fn record_failure(
            &self,
            ctx: &ListCtx,
            _server_name: &str,
            instance_id: Option<&str>,
            connection_generation: Option<u64>,
            reason: &str,
            _failure: Option<&RuntimeFailure>,
        ) -> Result<(), CatalogError> {
            self.evidence.lock().await.push(EvidenceRecord {
                server_id: ctx.server_id.clone(),
                kind: ctx.capability,
                instance_id: instance_id.map(ToOwned::to_owned),
                connection_generation,
                reason: reason.to_string(),
            });
            match self.evidence_error.lock().await.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }

        async fn discover_all_kinds(
            &self,
            _ctx: &ListCtx,
            _owner: &CapabilityOwner,
        ) -> Result<runtime::CapabilityFullDiscoveryObservation, RuntimeFailure> {
            self.discovery_calls.fetch_add(1, Ordering::Relaxed);
            self.discovery_started.add_permits(1);
            if let Some(gate) = self.discovery_gate.lock().await.clone() {
                gate.acquire().await.expect("discovery test gate remains open").forget();
            }
            self.discoveries
                .lock()
                .await
                .pop_front()
                .expect("a discovery result must be configured")
                .map(observation_to_full)
        }

        async fn commit_full_discovery(
            &self,
            owner: &CapabilityOwner,
            _observation: &runtime::CapabilityFullDiscoveryObservation,
        ) -> Result<i64, CapabilityCommitFailure> {
            self.commits.fetch_add(1, Ordering::Relaxed);
            if let Some(error) = self.commit_error.lock().await.take() {
                return Err(error);
            }
            let mut cache = self.warmed_cache.lock().await;
            for capability in [
                CapabilityType::Tools,
                CapabilityType::Prompts,
                CapabilityType::Resources,
                CapabilityType::ResourceTemplates,
            ] {
                cache.insert(capability, warmed_result(owner.source, capability));
            }
            Ok(1)
        }
    }

    struct FakeProvider {
        peer: rmcp::service::Peer<rmcp::service::RoleClient>,
        existing_result: Mutex<Option<Result<bool, CapabilityOwnerError>>>,
        fresh_result: Mutex<Option<Result<(), CapabilityOwnerError>>>,
        existing_calls: AtomicUsize,
        fresh_calls: AtomicUsize,
        acquisition_order: Mutex<Vec<&'static str>>,
        released: Mutex<Vec<OwnerSource>>,
        release_error: Mutex<Option<CapabilityOwnerError>>,
        fresh_started: tokio::sync::Semaphore,
        fresh_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
        config_fingerprint: std::sync::Mutex<String>,
    }

    impl FakeProvider {
        fn new(peer: rmcp::service::Peer<rmcp::service::RoleClient>) -> Self {
            Self {
                peer,
                existing_result: Mutex::new(Some(Ok(false))),
                fresh_result: Mutex::new(Some(Ok(()))),
                existing_calls: AtomicUsize::new(0),
                fresh_calls: AtomicUsize::new(0),
                acquisition_order: Mutex::new(Vec::new()),
                released: Mutex::new(Vec::new()),
                release_error: Mutex::new(None),
                fresh_started: tokio::sync::Semaphore::new(0),
                fresh_gate: Mutex::new(None),
                config_fingerprint: std::sync::Mutex::new("test-config".to_string()),
            }
        }

        async fn set_existing(
            &self,
            result: Result<bool, CapabilityOwnerError>,
        ) {
            *self.existing_result.lock().await = Some(result);
        }

        async fn set_fresh(
            &self,
            result: Result<(), CapabilityOwnerError>,
        ) {
            *self.fresh_result.lock().await = Some(result);
        }

        fn set_config_fingerprint(
            &self,
            fingerprint: String,
        ) {
            *self
                .config_fingerprint
                .lock()
                .expect("test config fingerprint mutex is not poisoned") = fingerprint;
        }

        async fn pause_fresh_owner_with(
            &self,
            gate: Arc<tokio::sync::Semaphore>,
        ) {
            *self.fresh_gate.lock().await = Some(gate);
        }

        fn owner(
            &self,
            source: OwnerSource,
            sequence: usize,
        ) -> CapabilityOwner {
            CapabilityOwner {
                server_id: "server-1".to_string(),
                server_name: "docs".to_string(),
                config_fingerprint: self
                    .config_fingerprint
                    .lock()
                    .expect("test config fingerprint mutex is not poisoned")
                    .clone(),
                instance_id: format!("{source:?}-{sequence}"),
                connection_generation: None,
                peer: self.peer.clone(),
                startup_tools: None,
                source,
                cleanup: None,
            }
        }
    }

    #[async_trait]
    impl CapabilityConnectionProvider for FakeProvider {
        async fn existing_owner(
            &self,
            _ctx: &ListCtx,
        ) -> Result<Option<CapabilityOwner>, CapabilityOwnerError> {
            let sequence = self.existing_calls.fetch_add(1, Ordering::Relaxed) + 1;
            self.acquisition_order.lock().await.push("existing");
            match self.existing_result.lock().await.take().unwrap_or(Ok(false)) {
                Ok(true) => Ok(Some(self.owner(OwnerSource::Existing, sequence))),
                Ok(false) => Ok(None),
                Err(error) => Err(error),
            }
        }

        async fn fresh_owner(
            &self,
            _ctx: &ListCtx,
        ) -> Result<CapabilityOwner, CapabilityOwnerError> {
            let sequence = self.fresh_calls.fetch_add(1, Ordering::Relaxed) + 1;
            self.acquisition_order.lock().await.push("fresh");
            self.fresh_started.add_permits(1);
            if let Some(gate) = self.fresh_gate.lock().await.clone() {
                gate.acquire()
                    .await
                    .expect("fresh owner test gate remains open")
                    .forget();
            }
            match self.fresh_result.lock().await.take().unwrap_or(Ok(())) {
                Ok(()) => Ok(self.owner(OwnerSource::Fresh, sequence)),
                Err(error) => Err(error),
            }
        }

        async fn release_owner(
            &self,
            owner: CapabilityOwner,
        ) -> Result<(), CapabilityOwnerError> {
            self.released.lock().await.push(owner.source);
            match self.release_error.lock().await.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }

    fn list_ctx(refresh: Option<RefreshStrategy>) -> ListCtx {
        ListCtx {
            capability: CapabilityType::Tools,
            server_id: "server-1".to_string(),
            refresh,
            operation_timeout: Duration::from_secs(1),
            validation_session: None,
            runtime_identity: None,
            connection_selection: None,
            visibility_snapshot: None,
            name_domain: NameDomain::Upstream,
        }
    }

    fn inspector_list_ctx(refresh: Option<RefreshStrategy>) -> ListCtx {
        ListCtx {
            validation_session: Some("inspector-test".to_string()),
            ..list_ctx(refresh)
        }
    }

    fn result(source: &str) -> ListResult {
        ListResult {
            items: CapabilityItems::Tools(Vec::new()),
            meta: Meta {
                cache_hit: true,
                source: source.to_string(),
                duration_ms: 0,
                had_peer: false,
            },
        }
    }

    fn observation() -> CapabilityDiscoveryObservation {
        CapabilityDiscoveryObservation {
            items: CapabilityItems::Tools(Vec::new()),
            flags: CapSyncFlags::TOOLS,
            kind_states: vec![KindObservation::new(
                CatalogKind::Tools,
                DeclarationState::Supported,
                InventoryState::Complete,
            )],
        }
    }

    fn failure(
        kind: RuntimeFailureKind,
        message: &str,
    ) -> RuntimeFailure {
        RuntimeFailure {
            kind,
            message: Some(message.to_string()),
            timeout_ms: None,
        }
    }

    async fn test_peer() -> TestPeerFixture {
        let (server_transport, client_transport) = tokio::io::duplex(4096);
        let server_task = tokio::spawn(async move {
            let server = TestServer
                .serve(server_transport)
                .await
                .expect("test server should initialize");
            server.waiting().await.expect("test server should stop");
        });
        let client = ().serve(client_transport).await.expect("test client should initialize");
        let peer = client.peer().clone();
        TestPeerFixture {
            peer,
            client: Some(client),
            server_task,
        }
    }

    async fn runtime_database() -> Arc<Database> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory database");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::server::init::initialize_server_tables(&pool)
            .await
            .expect("initialize server tables");
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("initialize client tables");
        crate::config::profile::init::initialize_profile_tables(&pool)
            .await
            .expect("initialize profile tables");
        crate::config::database::initialize_capability_catalog(&pool)
            .await
            .expect("initialize capability catalog");
        let mut server = Server::new_stdio("docs".to_string(), Some("fixture-command".to_string()));
        server.id = Some("server-1".to_string());
        upsert_server_definition(
            &pool,
            &server,
            &ServerTransportDraft::Stdio {
                command: Some("fixture-command".to_string()),
                args: Vec::new(),
                env: Default::default(),
            },
        )
        .await
        .expect("insert typed server fixture");
        Arc::new(Database {
            pool,
            path: PathBuf::new(),
            capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
        })
    }

    async fn commit_failure_database() -> Arc<Database> {
        let database = runtime_database().await;
        sqlx::query(
            "CREATE TRIGGER fail_capability_commit BEFORE INSERT ON server_tools BEGIN SELECT RAISE(ABORT, 'typed commit fixture'); END",
        )
        .execute(&database.pool)
        .await
        .expect("install commit failure trigger");
        database
    }

    async fn pooled_owner(
        source: OwnerSource
    ) -> (
        Arc<Mutex<UpstreamConnectionPool>>,
        CapabilityOwner,
        tokio::task::JoinHandle<()>,
    ) {
        let (server_transport, client_transport) = tokio::io::duplex(4096);
        let server_task = tokio::spawn(async move {
            let server = TestServer
                .serve(server_transport)
                .await
                .expect("test server should initialize");
            server.waiting().await.expect("test server should stop");
        });
        let service = UpstreamClientHandler::new("docs".to_string())
            .serve(client_transport)
            .await
            .expect("test client should initialize");
        let peer = service.peer().clone();
        let mut connection = UpstreamConnection::new("docs".to_string());
        connection.id = "owner-1".to_string();
        connection.update_connected(service, Vec::new(), Some(rmcp::model::ServerCapabilities::default()));
        let mut pool = UpstreamConnectionPool::new(Arc::new(Config::default()), None);
        pool.connections
            .entry("server-1".to_string())
            .or_default()
            .insert("owner-1".to_string(), connection);
        let pool = Arc::new(Mutex::new(pool));
        let owner = CapabilityOwner {
            server_id: "server-1".to_string(),
            server_name: "docs".to_string(),
            config_fingerprint: "test-config".to_string(),
            instance_id: "owner-1".to_string(),
            connection_generation: None,
            peer,
            startup_tools: None,
            source,
            cleanup: None,
        };
        (pool, owner, server_task)
    }

    #[tokio::test]
    async fn lru_or_sqlite_hit_never_acquires_an_owner() {
        for source in ["memory_cache", "sqlite_catalog"] {
            let backend = Arc::new(FakeBackend::new(Ok(Some(result(source)))));
            let fixture = test_peer().await;
            let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
            let service = CapabilityReadService::with_backend(backend, provider.clone(), 0);

            let listed = service
                .list(&list_ctx(None))
                .await
                .expect("cache hit should be returned");

            assert_eq!(listed.meta.source, source);
            assert_eq!(provider.existing_calls.load(Ordering::Relaxed), 0);
            assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 0);
            drop(service);
            drop(provider);
            fixture.shutdown().await;
        }
    }

    #[tokio::test]
    async fn catalog_only_preserves_catalog_projection_without_warming() {
        let literal_tool: Tool = serde_json::from_value(serde_json::json!({
            "name": "literal-catalog-tool",
            "description": "Literal catalog tool",
            "inputSchema": {
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"]
            }
        }))
        .expect("build literal catalog tool");
        let expected = ListResult {
            items: CapabilityItems::Tools(vec![literal_tool]),
            meta: Meta {
                cache_hit: true,
                source: "sqlite_catalog".to_string(),
                duration_ms: 41,
                had_peer: false,
            },
        };
        let backend = Arc::new(FakeBackend::new(Ok(Some(expected))));
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        let service = CapabilityReadService::with_backend(backend.clone(), provider.clone(), 0);
        let listed = service
            .catalog_only(&list_ctx(Some(RefreshStrategy::Force)))
            .await
            .expect("catalog projection should be returned")
            .expect("catalog projection should remain available");

        assert_eq!(listed.meta.source, "sqlite_catalog");
        assert_eq!(listed.meta.duration_ms, 41);
        assert!(listed.meta.cache_hit);
        assert!(!listed.meta.had_peer);
        let tools = listed
            .items
            .into_tools()
            .expect("catalog projection should retain tool items");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name.as_ref(), "literal-catalog-tool");
        let literal_payload = serde_json::to_value(&tools[0]).expect("serialize literal catalog tool");
        assert_eq!(literal_payload["description"], "Literal catalog tool");
        assert_eq!(literal_payload["inputSchema"]["type"], "object");
        assert_eq!(literal_payload["inputSchema"]["properties"]["query"]["type"], "string");
        assert_eq!(backend.cache_calls.load(Ordering::Relaxed), 1);
        assert_eq!(backend.persisted_kind_calls.load(Ordering::Relaxed), 0);
        assert_eq!(backend.canonical_name_calls.load(Ordering::Relaxed), 0);
        assert_eq!(backend.discovery_calls.load(Ordering::Relaxed), 0);
        assert_eq!(backend.projection_calls.load(Ordering::Relaxed), 0);
        assert_eq!(backend.commits.load(Ordering::Relaxed), 0);
        assert!(backend.warmed_cache.lock().await.is_empty());
        assert_eq!(provider.existing_calls.load(Ordering::Relaxed), 0);
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 0);
        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn catalog_only_returns_missing_catalog_without_warming() {
        let backend = Arc::new(FakeBackend::new(Ok(None)));
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        let service = CapabilityReadService::with_backend(backend.clone(), provider.clone(), 0);

        let listed = service
            .catalog_only(&list_ctx(None))
            .await
            .expect("missing catalog is not an error");

        assert!(listed.is_none());
        assert_eq!(backend.cache_calls.load(Ordering::Relaxed), 1);
        assert_eq!(backend.persisted_kind_calls.load(Ordering::Relaxed), 0);
        assert_eq!(backend.canonical_name_calls.load(Ordering::Relaxed), 0);
        assert_eq!(backend.discovery_calls.load(Ordering::Relaxed), 0);
        assert_eq!(backend.projection_calls.load(Ordering::Relaxed), 0);
        assert_eq!(backend.commits.load(Ordering::Relaxed), 0);
        assert!(backend.warmed_cache.lock().await.is_empty());
        assert_eq!(provider.existing_calls.load(Ordering::Relaxed), 0);
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 0);
        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn catalog_only_propagates_catalog_error_without_warming() {
        let backend = Arc::new(FakeBackend::new(Err(CapabilityReadError::CatalogOperation {
            server_id: "server-1".to_string(),
            source: anyhow::anyhow!("catalog query failed"),
        })));
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        let service = CapabilityReadService::with_backend(backend.clone(), provider.clone(), 0);

        let error = service
            .catalog_only(&list_ctx(None))
            .await
            .expect_err("catalog read error should propagate");

        match error {
            CapabilityReadError::CatalogOperation { server_id, source } => {
                assert_eq!(server_id, "server-1");
                assert_eq!(source.to_string(), "catalog query failed");
            }
            other => panic!("catalog error must be forwarded unchanged: {other}"),
        }
        assert_eq!(backend.cache_calls.load(Ordering::Relaxed), 1);
        assert_eq!(backend.persisted_kind_calls.load(Ordering::Relaxed), 0);
        assert_eq!(backend.canonical_name_calls.load(Ordering::Relaxed), 0);
        assert_eq!(backend.discovery_calls.load(Ordering::Relaxed), 0);
        assert_eq!(backend.projection_calls.load(Ordering::Relaxed), 0);
        assert_eq!(backend.commits.load(Ordering::Relaxed), 0);
        assert!(backend.warmed_cache.lock().await.is_empty());
        assert_eq!(provider.existing_calls.load(Ordering::Relaxed), 0);
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 0);
        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn list_all_kinds_force_refresh_reuses_single_discovery_warm() {
        let backend = Arc::new(FakeBackend::new(Ok(None)));
        backend.push_discovery(Ok(observation())).await;
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        let service = CapabilityReadService::with_backend(backend.clone(), provider.clone(), 0);

        let lists = service
            .list_all_kinds("server-1", Some(RefreshStrategy::Force))
            .await
            .expect("forced list all kinds should succeed");

        assert_eq!(backend.commits.load(Ordering::Relaxed), 1);
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 1);
        assert_eq!(lists.tools.unwrap().meta.source, "live");
        assert_eq!(lists.resources.unwrap().meta.source, "live");
        assert_eq!(lists.prompts.unwrap().meta.source, "live");
        assert_eq!(lists.resource_templates.unwrap().meta.source, "live");
        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn list_all_kinds_returns_one_owner_auth_failure_for_every_kind() {
        let backend = Arc::new(FakeBackend::new(Ok(None)));
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        provider
            .set_fresh(Err(CapabilityOwnerError::Authentication {
                code: CapabilityAuthenticationFailureCode::Forbidden,
                reason: "upstream rejected the token".to_string(),
            }))
            .await;
        let service = CapabilityReadService::with_backend(backend.clone(), provider.clone(), 0);

        let lists = service
            .list_all_kinds("server-1", Some(RefreshStrategy::Force))
            .await
            .expect("batch reads retain authentication as per-kind evidence");
        let failures = [
            lists.tools.expect_err("tools auth failure"),
            lists.resources.expect_err("resources auth failure"),
            lists.prompts.expect_err("prompts auth failure"),
            lists.resource_templates.expect_err("resource templates auth failure"),
        ];

        for failure in failures {
            assert_eq!(
                failure.authentication_failure(),
                Some((
                    CapabilityAuthenticationFailureCode::Forbidden,
                    "upstream rejected the token",
                ))
            );
        }
        let evidence = backend.evidence.lock().await;
        assert_eq!(evidence.len(), 4);
        assert_eq!(
            evidence.iter().map(|record| record.kind).collect::<Vec<_>>(),
            vec![
                CapabilityType::Tools,
                CapabilityType::Resources,
                CapabilityType::Prompts,
                CapabilityType::ResourceTemplates,
            ]
        );
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 1);

        drop(evidence);
        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn persisted_owner_auth_failure_prevents_a_second_management_discovery() {
        let database = runtime_database().await;
        let backend = Arc::new(RuntimeCapabilityReadBackend { database, pool: None });
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        provider
            .set_fresh(Err(CapabilityOwnerError::Authentication {
                code: CapabilityAuthenticationFailureCode::InsufficientScope,
                reason: "token lacks the required scope".to_string(),
            }))
            .await;
        let service = CapabilityReadService::with_backend(backend, provider.clone(), 11);

        let refreshed = service
            .list_all_kinds("server-1", Some(RefreshStrategy::Force))
            .await
            .expect("refresh retains authentication evidence");
        assert!(refreshed.has_failures());

        let cached = service
            .list_all_kinds("server-1", None)
            .await
            .expect("cache-first returns persisted authentication evidence");
        for failure in [
            cached.tools.expect_err("tools auth failure"),
            cached.resources.expect_err("resources auth failure"),
            cached.prompts.expect_err("prompts auth failure"),
            cached.resource_templates.expect_err("resource templates auth failure"),
        ] {
            let (code, reason) = failure
                .authentication_failure()
                .expect("persisted authentication remains typed");
            assert_eq!(code, CapabilityAuthenticationFailureCode::InsufficientScope);
            assert!(reason.contains("token lacks the required scope"));
        }
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 1);

        provider
            .set_fresh(Err(CapabilityOwnerError::Backoff { remaining_ms: 10_000 }))
            .await;
        let repeated_refresh = service
            .list_all_kinds("server-1", Some(RefreshStrategy::Force))
            .await
            .expect("backoff retains the persisted authentication evidence");
        for failure in [
            repeated_refresh.tools.expect_err("tools auth failure"),
            repeated_refresh.resources.expect_err("resources auth failure"),
            repeated_refresh.prompts.expect_err("prompts auth failure"),
            repeated_refresh
                .resource_templates
                .expect_err("resource templates auth failure"),
        ] {
            let (code, reason) = failure
                .authentication_failure()
                .expect("backoff must not replace the authentication failure");
            assert_eq!(code, CapabilityAuthenticationFailureCode::InsufficientScope);
            assert!(reason.contains("token lacks the required scope"));
        }
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 2);

        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn newer_connection_failure_is_not_replaced_by_older_authentication_evidence() {
        let database = runtime_database().await;
        let backend = Arc::new(RuntimeCapabilityReadBackend { database, pool: None });
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        provider
            .set_fresh(Err(CapabilityOwnerError::Authentication {
                code: CapabilityAuthenticationFailureCode::AuthRequired,
                reason: "authorization required".to_string(),
            }))
            .await;
        let service = CapabilityReadService::with_backend(backend, provider.clone(), 12);

        let authenticated_failure = service
            .list_all_kinds("server-1", Some(RefreshStrategy::Force))
            .await
            .expect("authentication failure is persisted");
        assert!(authenticated_failure.has_failures());

        provider
            .set_fresh(Err(CapabilityOwnerError::Other {
                reason: "TLS handshake failed".to_string(),
            }))
            .await;
        let newer_failure = service
            .list_all_kinds("server-1", Some(RefreshStrategy::Force))
            .await
            .expect_err("new connection-wide failure remains non-authentication");
        assert!(newer_failure.authentication_failure().is_none());

        provider
            .set_fresh(Err(CapabilityOwnerError::Backoff { remaining_ms: 10_000 }))
            .await;
        let backoff = service
            .list_all_kinds("server-1", Some(RefreshStrategy::Force))
            .await
            .expect_err("backoff must not resurrect older authentication evidence");
        assert!(backoff.authentication_failure().is_none());
        assert!(matches!(
            backoff,
            CapabilityReadError::DiscoveryFailed {
                fresh: Some(failure),
                ..
            } if matches!(
                failure.error,
                CapabilityAttemptError::Owner(CapabilityOwnerError::Backoff {
                    remaining_ms: 10_000
                })
            )
        ));
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 3);

        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn overlapping_force_refreshes_join_one_management_flight() {
        let backend = Arc::new(FakeBackend::new(Ok(None)));
        backend.push_discovery(Ok(observation())).await;
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        backend.pause_discovery_with(gate.clone()).await;
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        let service = Arc::new(CapabilityReadService::with_backend(
            backend.clone(),
            provider.clone(),
            1,
        ));

        let first = {
            let service = service.clone();
            tokio::spawn(async move { service.list_all_kinds("server-1", Some(RefreshStrategy::Force)).await })
        };
        backend
            .discovery_started
            .acquire()
            .await
            .expect("first discovery starts")
            .forget();
        let second = {
            let service = service.clone();
            tokio::spawn(async move { service.list_all_kinds("server-1", Some(RefreshStrategy::Force)).await })
        };
        tokio::task::yield_now().await;
        gate.add_permits(1);

        assert!(
            !first
                .await
                .expect("first task joins")
                .expect("first refresh")
                .has_failures()
        );
        assert!(
            !second
                .await
                .expect("second task joins")
                .expect("second refresh")
                .has_failures()
        );
        assert_eq!(backend.commits.load(Ordering::Relaxed), 1);
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 1);

        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn concurrent_cache_misses_share_one_failing_management_discovery() {
        let backend = Arc::new(FakeBackend::new(Ok(None)));
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        provider
            .set_fresh(Err(CapabilityOwnerError::Other {
                reason: "fixture upstream unavailable".to_string(),
            }))
            .await;
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        provider.pause_fresh_owner_with(gate.clone()).await;
        let service = Arc::new(CapabilityReadService::with_backend(
            backend.clone(),
            provider.clone(),
            16,
        ));
        let first = {
            let service = service.clone();
            tokio::spawn(async move { service.list_all_kinds("server-1", None).await })
        };
        provider
            .fresh_started
            .acquire()
            .await
            .expect("one discovery owner starts")
            .forget();
        let second = {
            let service = service.clone();
            tokio::spawn(async move { service.list_all_kinds("server-1", None).await })
        };
        tokio::task::yield_now().await;
        assert_eq!(
            provider.fresh_calls.load(Ordering::Relaxed),
            1,
            "both cache misses must overlap the sole fresh discovery"
        );
        gate.add_permits(1);

        for task in [first, second] {
            let error = task
                .await
                .expect("concurrent read joins")
                .expect_err("shared failed discovery remains visible");
            assert!(matches!(
                error,
                CapabilityReadError::DiscoveryFailed {
                    fresh: Some(failure),
                    ..
                } if matches!(
                    &failure.error,
                    CapabilityAttemptError::Owner(CapabilityOwnerError::Other { reason })
                        if reason == "fixture upstream unavailable"
                )
            ));
        }
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 1);

        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn overlapping_force_refreshes_share_owner_timeout() {
        let backend = Arc::new(FakeBackend::new(Ok(None)));
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        provider
            .set_fresh(Err(CapabilityOwnerError::Timeout { timeout_ms: 321 }))
            .await;
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        provider.pause_fresh_owner_with(gate.clone()).await;
        let service = Arc::new(CapabilityReadService::with_backend(backend, provider.clone(), 2));

        let first = {
            let service = service.clone();
            tokio::spawn(async move { service.list_all_kinds("server-1", Some(RefreshStrategy::Force)).await })
        };
        provider
            .fresh_started
            .acquire()
            .await
            .expect("first owner acquisition starts")
            .forget();
        let second = {
            let service = service.clone();
            tokio::spawn(async move { service.list_all_kinds("server-1", Some(RefreshStrategy::Force)).await })
        };
        tokio::task::yield_now().await;
        gate.add_permits(1);

        for task in [first, second] {
            let error = task
                .await
                .expect("force task joins")
                .expect_err("owner timeout remains visible");
            assert_eq!(error.connection_timeout_ms(), Some(321));
        }
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 1);

        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn overlapping_force_refreshes_share_runtime_timeout() {
        let backend = Arc::new(FakeBackend::new(Ok(None)));
        backend
            .push_discovery(Err(RuntimeFailure {
                kind: RuntimeFailureKind::Timeout,
                message: Some("tools/list timed out".to_string()),
                timeout_ms: Some(456),
            }))
            .await;
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        backend.pause_discovery_with(gate.clone()).await;
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        let service = Arc::new(CapabilityReadService::with_backend(
            backend.clone(),
            provider.clone(),
            3,
        ));

        let first = {
            let service = service.clone();
            tokio::spawn(async move { service.list_all_kinds("server-1", Some(RefreshStrategy::Force)).await })
        };
        backend
            .discovery_started
            .acquire()
            .await
            .expect("first discovery starts")
            .forget();
        let second = {
            let service = service.clone();
            tokio::spawn(async move { service.list_all_kinds("server-1", Some(RefreshStrategy::Force)).await })
        };
        tokio::task::yield_now().await;
        gate.add_permits(1);

        for task in [first, second] {
            let error = task
                .await
                .expect("force task joins")
                .expect_err("runtime timeout remains visible");
            assert_eq!(error.operation_timeout_ms(), Some(456));
        }
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 1);

        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn overlapping_force_refreshes_share_commit_failure() {
        let backend = Arc::new(FakeBackend::new(Ok(None)));
        backend.push_discovery(Ok(observation())).await;
        backend.fail_next_commit().await;
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        backend.pause_discovery_with(gate.clone()).await;
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        let service = Arc::new(CapabilityReadService::with_backend(
            backend.clone(),
            provider.clone(),
            4,
        ));

        let first = {
            let service = service.clone();
            tokio::spawn(async move { service.list_all_kinds("server-1", Some(RefreshStrategy::Force)).await })
        };
        backend
            .discovery_started
            .acquire()
            .await
            .expect("first discovery starts")
            .forget();
        let second = {
            let service = service.clone();
            tokio::spawn(async move { service.list_all_kinds("server-1", Some(RefreshStrategy::Force)).await })
        };
        tokio::task::yield_now().await;
        gate.add_permits(1);

        for task in [first, second] {
            let error = task
                .await
                .expect("force task joins")
                .expect_err("commit failure remains visible");
            let CapabilityReadError::DiscoveryFailed {
                fresh: Some(failure), ..
            } = error
            else {
                panic!("commit failure must retain the discovery error variant");
            };
            assert_eq!(failure.instance_id.as_deref(), Some("Fresh-1"));
            assert_eq!(failure.source, OwnerSource::Fresh);
            assert!(matches!(
                failure.error,
                CapabilityAttemptError::Commit(CapabilityCommitFailure::Operation(_))
            ));
        }
        assert_eq!(backend.commits.load(Ordering::Relaxed), 1);
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 1);

        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn overlapping_force_refreshes_share_typed_cleanup_failure() {
        let backend = Arc::new(FakeBackend::new(Ok(None)));
        backend.push_discovery(Ok(observation())).await;
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        backend.pause_discovery_with(gate.clone()).await;
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        *provider.release_error.lock().await = Some(CapabilityOwnerError::Other {
            reason: "cleanup transport closed".to_string(),
        });
        let service = Arc::new(CapabilityReadService::with_backend(
            backend.clone(),
            provider.clone(),
            15,
        ));

        let first = {
            let service = service.clone();
            tokio::spawn(async move { service.list_all_kinds("server-1", Some(RefreshStrategy::Force)).await })
        };
        backend
            .discovery_started
            .acquire()
            .await
            .expect("first discovery starts")
            .forget();
        let second = {
            let service = service.clone();
            tokio::spawn(async move { service.list_all_kinds("server-1", Some(RefreshStrategy::Force)).await })
        };
        tokio::task::yield_now().await;
        gate.add_permits(1);

        for task in [first, second] {
            let error = task
                .await
                .expect("force task joins")
                .expect_err("cleanup failure remains visible");
            let CapabilityReadError::CleanupFailed {
                instance_id,
                connection_generation,
                owner_source,
                error,
                ..
            } = error
            else {
                panic!("cleanup failure must retain its typed variant");
            };
            let CapabilityOwnerError::Other { reason } = error.as_ref() else {
                panic!("cleanup failure must retain its typed variant");
            };
            assert_eq!(instance_id, "Fresh-1");
            assert_eq!(connection_generation, None);
            assert_eq!(owner_source, OwnerSource::Fresh);
            assert_eq!(reason, "cleanup transport closed");
        }
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 1);

        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn force_refreshes_with_different_config_fingerprints_do_not_share_a_flight() {
        let backend = Arc::new(FakeBackend::new(Ok(None)));
        backend.push_discovery(Ok(observation())).await;
        backend.push_discovery(Ok(observation())).await;
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        backend.pause_discovery_with(gate.clone()).await;
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        let service = Arc::new(CapabilityReadService::with_backend(
            backend.clone(),
            provider.clone(),
            14,
        ));

        let first = {
            let service = service.clone();
            tokio::spawn(async move { service.list_all_kinds("server-1", Some(RefreshStrategy::Force)).await })
        };
        backend
            .discovery_started
            .acquire()
            .await
            .expect("old configuration discovery starts")
            .forget();

        backend.set_coordination_fingerprint("test-config-v2");
        provider.set_config_fingerprint("test-config-v2".to_string());
        let second = {
            let service = service.clone();
            tokio::spawn(async move { service.list_all_kinds("server-1", Some(RefreshStrategy::Force)).await })
        };
        backend
            .discovery_started
            .acquire()
            .await
            .expect("new configuration discovery starts independently")
            .forget();
        gate.add_permits(2);

        assert!(
            !first
                .await
                .expect("old task joins")
                .expect("old refresh")
                .has_failures()
        );
        assert!(
            !second
                .await
                .expect("new task joins")
                .expect("new refresh")
                .has_failures()
        );
        assert_eq!(backend.commits.load(Ordering::Relaxed), 2);
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 2);

        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn persisted_kind_failure_preserves_runtime_type() {
        let database = runtime_database().await;
        let config_fingerprint =
            crate::config::server::capabilities::current_config_fingerprint(&database.pool, "server-1")
                .await
                .expect("load config fingerprint");
        let fixture = test_peer().await;
        let provider = FakeProvider::new(fixture.peer.clone());
        let mut owner = provider.owner(OwnerSource::Fresh, 1);
        owner.config_fingerprint = config_fingerprint;

        for (kind, timeout_ms) in [
            (RuntimeFailureKind::Authentication, None),
            (RuntimeFailureKind::AuthRequired, None),
            (RuntimeFailureKind::Unauthorized, None),
            (RuntimeFailureKind::Forbidden, None),
            (RuntimeFailureKind::InsufficientScope, None),
            (RuntimeFailureKind::Timeout, Some(789)),
        ] {
            let failure = RuntimeFailure {
                kind,
                message: Some(format!("{kind:?} discovery failure")),
                timeout_ms,
            };
            let observation = runtime::CapabilityFullDiscoveryObservation {
                tools: Vec::new(),
                resources: Vec::new(),
                prompts: Vec::new(),
                templates: Vec::new(),
                flags: CapSyncFlags::TOOLS,
                kind_states: vec![
                    KindObservation::new(CatalogKind::Tools, DeclarationState::Supported, InventoryState::Failed)
                        .with_failure(
                            failure.kind.persisted(),
                            failure.message.clone().expect("failure message"),
                            failure.timeout_ms.and_then(|timeout| u64::try_from(timeout).ok()),
                        ),
                ],
                failures: vec![runtime::CapabilityKindFailure {
                    kind: CapabilityType::Tools,
                    failure: failure.clone(),
                }],
            };
            runtime::commit_full_discovery_observation(&owner, &observation, &database)
                .await
                .expect("persist failed kind observation");

            let restored = runtime::persisted_kind_failure(&list_ctx(None), &database)
                .await
                .unwrap_or_else(|_| panic!("load persisted failure"))
                .expect("failed kind remains persisted");
            assert_eq!(restored.kind, failure.kind);
            assert_eq!(restored.timeout_ms, timeout_ms);
        }

        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn cache_first_waiter_restarts_after_stale_configuration_commit() {
        let backend = Arc::new(FakeBackend::new(Ok(None)));
        backend.push_discovery(Ok(observation())).await;
        backend.push_discovery(Ok(observation())).await;
        backend
            .fail_next_commit_with(CapabilityCommitFailure::ConfigurationChanged(
                crate::config::server::capabilities::CapabilityConfigurationChanged {
                    server_id: "server-1".to_string(),
                    expected: "old-config".to_string(),
                    actual: "new-config".to_string(),
                },
            ))
            .await;
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        backend.pause_discovery_with(gate.clone()).await;
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        let service = Arc::new(CapabilityReadService::with_backend(
            backend.clone(),
            provider.clone(),
            5,
        ));

        let old_discovery = {
            let service = service.clone();
            tokio::spawn(async move { service.list_all_kinds("server-1", Some(RefreshStrategy::Force)).await })
        };
        backend
            .discovery_started
            .acquire()
            .await
            .expect("old discovery starts")
            .forget();
        let updated_read = {
            let service = service.clone();
            tokio::spawn(async move { service.list_all_kinds("server-1", None).await })
        };
        tokio::task::yield_now().await;
        gate.add_permits(2);

        old_discovery
            .await
            .expect("old discovery task joins")
            .expect_err("old configuration commit is rejected");
        assert!(
            !updated_read
                .await
                .expect("updated read task joins")
                .expect("updated read launches fresh discovery")
                .has_failures()
        );
        assert_eq!(backend.commits.load(Ordering::Relaxed), 2);
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 2);

        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn list_all_kinds_reuses_single_discovery_warm() {
        let backend = Arc::new(FakeBackend::new(Ok(None)));
        backend.push_discovery(Ok(observation())).await;
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        let service = CapabilityReadService::with_backend(backend.clone(), provider.clone(), 0);

        let lists = service
            .list_all_kinds("server-1", None)
            .await
            .expect("list all kinds should succeed");

        assert_eq!(backend.commits.load(Ordering::Relaxed), 1);
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 1);
        let tools = lists.tools.unwrap();
        let resources = lists.resources.unwrap();
        let prompts = lists.prompts.unwrap();
        let resource_templates = lists.resource_templates.unwrap();
        assert_eq!(tools.meta.source, "live");
        assert_eq!(resources.meta.source, "live");
        assert_eq!(prompts.meta.source, "live");
        assert_eq!(resource_templates.meta.source, "live");
        assert!(!tools.meta.cache_hit);
        assert!(!resources.meta.cache_hit);
        assert!(!prompts.meta.cache_hit);
        assert!(!resource_templates.meta.cache_hit);
        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn list_all_kinds_completes_a_partial_catalog_instead_of_trusting_one_cached_kind() {
        let backend = Arc::new(FakeBackend::new(Ok(None)));
        backend
            .warmed_cache
            .lock()
            .await
            .insert(CapabilityType::Tools, result("sqlite_catalog"));
        backend.push_discovery(Ok(observation())).await;
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        let service = CapabilityReadService::with_backend(backend.clone(), provider.clone(), 0);

        let lists = service
            .list_all_kinds("server-1", None)
            .await
            .expect("partial catalog should be completed by one management discovery");

        assert!(!lists.has_failures());
        assert_eq!(backend.commits.load(Ordering::Relaxed), 1);
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 1);
        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn missing_snapshot_uses_fresh_owner_for_management_catalog() {
        let backend = Arc::new(FakeBackend::new(Ok(None)));
        backend.push_discovery(Ok(observation())).await;
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        let service = CapabilityReadService::with_backend(backend, provider.clone(), 0);

        let listed = service
            .list(&list_ctx(None))
            .await
            .expect("management discovery should succeed");

        assert_eq!(listed.meta.source, "live");
        assert_eq!(provider.existing_calls.load(Ordering::Relaxed), 0);
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 1);
        assert_eq!(*provider.acquisition_order.lock().await, ["fresh"]);
        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn missing_snapshot_uses_existing_owner_before_fresh_owner() {
        let backend = Arc::new(FakeBackend::new(Ok(None)));
        backend.push_discovery(Ok(observation())).await;
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        provider.set_existing(Ok(true)).await;
        let service = CapabilityReadService::with_backend(backend, provider.clone(), 0);

        let listed = service
            .list(&inspector_list_ctx(None))
            .await
            .expect("existing discovery should succeed");

        assert_eq!(listed.meta.source, "live_existing");
        assert_eq!(provider.existing_calls.load(Ordering::Relaxed), 1);
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 0);
        assert_eq!(*provider.acquisition_order.lock().await, ["existing"]);
        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn stale_existing_owner_retries_exactly_one_fresh_owner() {
        let backend = Arc::new(FakeBackend::new(Ok(None)));
        backend
            .push_discovery(Err(failure(RuntimeFailureKind::StaleGeneration, "stale generation")))
            .await;
        backend.push_discovery(Ok(observation())).await;
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        provider.set_existing(Ok(true)).await;
        let service = CapabilityReadService::with_backend(backend.clone(), provider.clone(), 0);

        let listed = service
            .list(&inspector_list_ctx(None))
            .await
            .expect("fresh discovery should recover");

        assert_eq!(listed.meta.source, "live_fresh");
        assert_eq!(provider.existing_calls.load(Ordering::Relaxed), 1);
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 1);
        assert_eq!(*provider.acquisition_order.lock().await, ["existing", "fresh"]);
        assert_eq!(
            *provider.released.lock().await,
            [OwnerSource::Existing, OwnerSource::Fresh]
        );
        assert_eq!(backend.evidence.lock().await.len(), 1);
        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn protocol_error_does_not_retry_with_a_fresh_owner() {
        let backend = Arc::new(FakeBackend::new(Ok(None)));
        backend
            .push_discovery(Err(failure(RuntimeFailureKind::Protocol, "invalid response")))
            .await;
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        provider.set_existing(Ok(true)).await;
        let service = CapabilityReadService::with_backend(backend.clone(), provider.clone(), 0);

        let error = service
            .list(&inspector_list_ctx(None))
            .await
            .expect_err("protocol failure must be returned");

        assert!(matches!(error, CapabilityReadError::DiscoveryFailed { .. }));
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 0);
        let evidence = backend.evidence.lock().await;
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].server_id, "server-1");
        assert_eq!(evidence[0].kind, CapabilityType::Tools);
        assert_eq!(evidence[0].instance_id.as_deref(), Some("Existing-1"));
        assert_eq!(evidence[0].connection_generation, None);
        assert!(evidence[0].reason.contains("invalid response"));
        drop(evidence);
        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn dual_stage_failure_preserves_canonical_name_and_protocol_operation() {
        let backend = Arc::new(FakeBackend::new(Ok(None)));
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        *provider.fresh_result.lock().await = Some(Err(CapabilityOwnerError::Other {
            reason: "fresh owner unavailable".to_string(),
        }));
        let service = CapabilityReadService::with_backend(backend, provider, 0);

        let error = service
            .list(&list_ctx(None))
            .await
            .expect_err("both acquisition stages should fail");
        let display = error.to_string();

        match &error {
            CapabilityReadError::DiscoveryFailed {
                server_name, operation, ..
            } => {
                assert_eq!(server_name, "docs");
                assert_eq!(*operation, "tools/list");
            }
            other => panic!("unexpected error: {other:?}"),
        }

        assert!(display.contains("docs"), "missing canonical server name: {display}");
        assert!(display.contains("tools/list"), "missing protocol operation: {display}");
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn cleanup_failure_after_commit_does_not_record_inventory_failure() {
        let backend = Arc::new(FakeBackend::new(Ok(None)));
        backend.push_discovery(Ok(observation())).await;
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        *provider.release_error.lock().await = Some(CapabilityOwnerError::Other {
            reason: "shutdown join failed".to_string(),
        });
        let service = CapabilityReadService::with_backend(backend.clone(), provider, 0);

        let error = service
            .list(&list_ctx(None))
            .await
            .expect_err("cleanup failure must remain visible");

        match error {
            CapabilityReadError::CleanupFailed {
                server_name,
                operation,
                owner_source,
                error,
                ..
            } => {
                let CapabilityOwnerError::Other { reason } = error.as_ref() else {
                    panic!("unexpected cleanup error: {error:?}");
                };
                assert_eq!(server_name, "docs");
                assert_eq!(operation, "management catalog warm");
                assert_eq!(owner_source, OwnerSource::Fresh);
                assert_eq!(reason, "shutdown join failed");
            }
            other => panic!("unexpected cleanup error: {other:?}"),
        }
        assert_eq!(backend.commits.load(Ordering::Relaxed), 1);
        assert!(backend.evidence.lock().await.is_empty());
        fixture.shutdown().await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn sqlite_commit_failure_remains_typed_in_read_backend_error() {
        let database = commit_failure_database().await;
        let config_fingerprint =
            crate::config::server::capabilities::current_config_fingerprint(&database.pool, "server-1")
                .await
                .expect("load config fingerprint");
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        provider.set_config_fingerprint(config_fingerprint);
        let tool: rmcp::model::Tool = serde_json::from_value(serde_json::json!({
            "name": "failing-tool",
            "description": "Commit failure fixture",
            "inputSchema": {"type": "object"}
        }))
        .expect("build tool fixture");
        let backend = Arc::new(CommitFailureBackend {
            runtime: RuntimeCapabilityReadBackend { database, pool: None },
            observation: Mutex::new(Some(CapabilityDiscoveryObservation {
                items: CapabilityItems::Tools(vec![tool]),
                flags: CapSyncFlags::ALL,
                kind_states: vec![KindObservation::new(
                    CatalogKind::Tools,
                    DeclarationState::Supported,
                    InventoryState::Complete,
                )],
            })),
            discovery_failure: Mutex::new(None),
            projection_error: Mutex::new(None),
        });
        let service = CapabilityReadService::with_backend(backend, provider, 0);

        let error = service
            .list(&list_ctx(None))
            .await
            .expect_err("SQLite trigger should fail canonical commit through the read service");

        let CapabilityReadError::DiscoveryFailed { fresh: Some(fresh), .. } = error else {
            panic!("catalog error must remain typed in the final read error");
        };
        assert!(matches!(
            fresh.error,
            CapabilityAttemptError::Commit(CapabilityCommitFailure::Catalog(_) | CapabilityCommitFailure::Database(_))
        ));
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn live_failure_never_returns_the_previous_ready_snapshot() {
        let backend = Arc::new(FakeBackend::new(Ok(Some(result("sqlite_catalog")))));
        backend
            .push_discovery(Err(failure(RuntimeFailureKind::Application, "application failure")))
            .await;
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        provider.set_existing(Ok(true)).await;
        let service = CapabilityReadService::with_backend(backend.clone(), provider, 0);

        let error = service
            .list(&list_ctx(Some(RefreshStrategy::Force)))
            .await
            .expect_err("force discovery failure must not return the previous snapshot");

        assert!(matches!(error, CapabilityReadError::DiscoveryFailed { .. }));
        assert_eq!(backend.cache_calls.load(Ordering::Relaxed), 0);
        assert_eq!(backend.evidence.lock().await.len(), 1);
        assert_eq!(backend.commits.load(Ordering::Relaxed), 0);
        drop(service);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn single_kind_failure_persists_typed_evidence_while_the_snapshot_is_unavailable() {
        let database = runtime_database().await;
        SqliteCapabilityCatalog::new(database.pool.clone())
            .ensure_schema()
            .await
            .expect("initialize capability catalog schema");
        let config_fingerprint =
            crate::config::server::capabilities::current_config_fingerprint(&database.pool, "server-1")
                .await
                .expect("load config fingerprint");
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        provider.set_config_fingerprint(config_fingerprint);
        provider.set_existing(Ok(true)).await;
        let backend = Arc::new(CommitFailureBackend {
            runtime: RuntimeCapabilityReadBackend {
                database: database.clone(),
                pool: None,
            },
            observation: Mutex::new(None),
            discovery_failure: Mutex::new(Some(RuntimeFailure {
                kind: RuntimeFailureKind::Timeout,
                message: Some("tools/list timed out".to_string()),
                timeout_ms: Some(789),
            })),
            projection_error: Mutex::new(None),
        });
        let service = CapabilityReadService::with_backend(backend, provider, 0);

        let error = service
            .list(&inspector_list_ctx(None))
            .await
            .expect_err("single-kind timeout must remain visible");
        assert_eq!(error.operation_timeout_ms(), Some(789));

        let snapshot = SqliteCapabilityCatalog::new(database.pool.clone())
            .load_snapshot("server-1")
            .await
            .expect("load failed snapshot")
            .expect("failed snapshot exists");
        assert_eq!(snapshot.state, SnapshotState::Unavailable);
        let tools = snapshot
            .kind_states
            .iter()
            .find(|state| state.kind == CatalogKind::Tools)
            .expect("tools failure state exists");
        assert_eq!(tools.inventory, InventoryState::Failed);
        assert_eq!(tools.failure_kind, Some(KindFailureKind::Timeout));
        assert_eq!(tools.timeout_ms, Some(789));
        assert!(
            tools
                .error
                .as_deref()
                .is_some_and(|reason| reason.contains("tools/list timed out")),
            "unexpected tools failure: {tools:?}"
        );
        let restored = runtime::persisted_kind_failure(&inspector_list_ctx(None), &database)
            .await
            .unwrap_or_else(|_| panic!("load unavailable kind failure"))
            .expect("unavailable snapshots retain typed failure evidence");
        assert_eq!(restored.kind, RuntimeFailureKind::Timeout);
        assert_eq!(restored.timeout_ms, Some(789));

        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn post_commit_projection_failure_does_not_record_inventory_failure() {
        let backend = Arc::new(FakeBackend::new(Ok(None)));
        backend.push_discovery(Ok(observation())).await;
        *backend.projection_error.lock().await = Some(CapabilityProjectionFailure(anyhow::anyhow!(
            "external-name projection failed after durable commit"
        )));
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        provider.set_existing(Ok(true)).await;
        let service = CapabilityReadService::with_backend(backend.clone(), provider.clone(), 0);

        let error = service
            .list(&inspector_list_ctx(None))
            .await
            .expect_err("post-commit projection failure must be surfaced");

        assert!(
            error
                .to_string()
                .contains("external-name projection failed after durable commit"),
            "projection cause was lost: {error:?}"
        );
        assert!(matches!(error, CapabilityReadError::ProjectionFailed { .. }));
        assert_eq!(backend.commits.load(Ordering::Relaxed), 1);
        assert!(
            backend.evidence.lock().await.is_empty(),
            "a local projection failure must not overwrite the committed inventory"
        );
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 0);
        assert_eq!(provider.released.lock().await.as_slice(), &[OwnerSource::Existing]);
        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn durable_commit_survives_projection_failure_without_a_second_catalog_transition() {
        let database = runtime_database().await;
        let config_fingerprint =
            crate::config::server::capabilities::current_config_fingerprint(&database.pool, "server-1")
                .await
                .expect("load config fingerprint");
        let mut events = EventBus::global().subscribe_async();
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        provider.set_config_fingerprint(config_fingerprint);
        provider.set_existing(Ok(true)).await;
        let tool: rmcp::model::Tool = serde_json::from_value(serde_json::json!({
            "name": "durable-tool",
            "description": "Projection failure fixture",
            "inputSchema": {"type": "object"}
        }))
        .expect("build tool fixture");
        let backend = Arc::new(CommitFailureBackend {
            runtime: RuntimeCapabilityReadBackend {
                database: database.clone(),
                pool: None,
            },
            observation: Mutex::new(Some(CapabilityDiscoveryObservation {
                items: CapabilityItems::Tools(vec![tool]),
                flags: CapSyncFlags::TOOLS,
                kind_states: vec![KindObservation::new(
                    CatalogKind::Tools,
                    DeclarationState::Supported,
                    InventoryState::Complete,
                )],
            })),
            discovery_failure: Mutex::new(None),
            projection_error: Mutex::new(Some(CapabilityProjectionFailure(anyhow::anyhow!(
                "forced projection failure after commit"
            )))),
        });
        let service = CapabilityReadService::with_backend(backend, provider.clone(), 0);

        let error = service
            .list(&inspector_list_ctx(None))
            .await
            .expect_err("projection failure must remain visible after the durable commit");

        assert!(matches!(error, CapabilityReadError::ProjectionFailed { .. }));
        let snapshot = SqliteCapabilityCatalog::new(database.pool.clone())
            .load_snapshot("server-1")
            .await
            .expect("load committed snapshot")
            .expect("durable snapshot exists");
        assert_eq!(snapshot.state, SnapshotState::Ready);
        assert_eq!(snapshot.revision, 1);
        assert_eq!(snapshot.last_error, None);
        assert_eq!(snapshot.records.len(), 1);
        match &snapshot.records[0].source_payload {
            CapabilityPayload::Tool(tool) => assert_eq!(tool.name, "durable-tool"),
            other => panic!("unexpected committed payload: {other:?}"),
        }
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 0);
        assert_eq!(provider.released.lock().await.as_slice(), &[OwnerSource::Existing]);

        let mut committed = 0;
        let mut changed = 0;
        tokio::time::timeout(Duration::from_secs(1), async {
            while committed == 0 || changed == 0 {
                match events.recv().await.expect("receive catalog transition") {
                    Event::CapabilityCatalogCommitted { server_id, .. } if server_id == "server-1" => committed += 1,
                    Event::CapabilityCatalogChanged { server_id, .. } if server_id == "server-1" => changed += 1,
                    _ => {}
                }
            }
        })
        .await
        .expect("durable commit must publish its transition");
        assert_eq!((committed, changed), (1, 1));

        let extra_transition = tokio::time::timeout(Duration::from_millis(100), async {
            loop {
                match events.recv().await.expect("receive catalog event") {
                    Event::CapabilityCatalogCommitted { server_id, .. }
                    | Event::CapabilityCatalogChanged { server_id, .. }
                        if server_id == "server-1" =>
                    {
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await;
        assert!(
            extra_transition.is_err(),
            "projection failure published a second catalog transition"
        );
        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn owner_timeout_remains_typed_in_discovery_error() {
        let backend = Arc::new(FakeBackend::new(Ok(None)));
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        provider
            .set_existing(Err(CapabilityOwnerError::Timeout { timeout_ms: 125 }))
            .await;
        let service = CapabilityReadService::with_backend(backend, provider.clone(), 0);

        let error = service
            .list(&inspector_list_ctx(None))
            .await
            .expect_err("owner timeout must be returned without a fresh retry");

        assert_eq!(error.connection_timeout_ms(), Some(125));
        assert_eq!(error.operation_timeout_ms(), None);
        match error {
            CapabilityReadError::DiscoveryFailed {
                existing: Some(failure),
                fresh: None,
                ..
            } => {
                assert_eq!(failure.instance_id, None);
                assert_eq!(failure.connection_generation, None);
                assert_eq!(failure.source, OwnerSource::Existing);
                assert!(matches!(
                    failure.error,
                    CapabilityAttemptError::Owner(CapabilityOwnerError::Timeout { timeout_ms: 125 })
                ));
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 0);
        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn runtime_timeout_remains_typed_in_discovery_error() {
        let backend = Arc::new(FakeBackend::new(Ok(None)));
        backend
            .push_discovery(Err(RuntimeFailure {
                kind: RuntimeFailureKind::Timeout,
                message: Some("request timeout".to_string()),
                timeout_ms: Some(1_000),
            }))
            .await;
        let fixture = test_peer().await;
        let provider = Arc::new(FakeProvider::new(fixture.peer.clone()));
        provider.set_existing(Ok(true)).await;
        let service = CapabilityReadService::with_backend(backend, provider.clone(), 0);

        let error = service
            .list(&inspector_list_ctx(None))
            .await
            .expect_err("runtime timeout must be returned without a fresh retry");

        assert_eq!(error.connection_timeout_ms(), None);
        assert_eq!(error.operation_timeout_ms(), Some(1_000));
        match error {
            CapabilityReadError::DiscoveryFailed {
                existing: Some(failure),
                fresh: None,
                ..
            } => {
                assert_eq!(failure.instance_id.as_deref(), Some("Existing-1"));
                assert!(matches!(
                    failure.error,
                    CapabilityAttemptError::Runtime(RuntimeFailure {
                        kind: RuntimeFailureKind::Timeout,
                        timeout_ms: Some(1_000),
                        ..
                    })
                ));
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(provider.fresh_calls.load(Ordering::Relaxed), 0);
        drop(service);
        drop(provider);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn only_existing_transport_failure_updates_pool_health_and_selection() {
        let selection = crate::core::capability::ConnectionSelection {
            server_id: "server-1".to_string(),
            affinity_key: crate::core::capability::AffinityKey::Default,
        };
        let failure = RuntimeFailure {
            kind: RuntimeFailureKind::TransportClosed,
            message: Some("transport closed".to_string()),
            timeout_ms: None,
        };

        let (existing_pool, existing_owner, existing_server) = pooled_owner(OwnerSource::Existing).await;
        apply_owner_runtime_failure(Some(&existing_pool), &existing_owner, &failure).await;
        {
            let guard = existing_pool.lock().await;
            assert_eq!(guard.select_ready_instance_id(&selection).expect("selection"), None);
            assert!(matches!(
                guard.failure_states.get("server-1").and_then(|state| state.last_kind),
                Some(FailureKind::RuntimeGone)
            ));
        }
        tokio::time::timeout(Duration::from_secs(1), existing_server)
            .await
            .expect("existing server should stop")
            .expect("existing server task should join");

        let (fresh_pool, fresh_owner, fresh_server) = pooled_owner(OwnerSource::Fresh).await;
        apply_owner_runtime_failure(Some(&fresh_pool), &fresh_owner, &failure).await;
        {
            let guard = fresh_pool.lock().await;
            assert_eq!(
                guard.select_ready_instance_id(&selection).expect("selection"),
                Some("owner-1".to_string())
            );
            assert!(!guard.failure_states.contains_key("server-1"));
        }
        fresh_pool
            .lock()
            .await
            .disconnect_non_blocking("server-1", "owner-1")
            .await
            .expect("fresh fixture should disconnect");
        tokio::time::timeout(Duration::from_secs(1), fresh_server)
            .await
            .expect("fresh server should stop")
            .expect("fresh server task should join");

        let (validation_pool, validation_owner, validation_server) = pooled_owner(OwnerSource::Validation).await;
        apply_owner_runtime_failure(Some(&validation_pool), &validation_owner, &failure).await;
        {
            let guard = validation_pool.lock().await;
            assert_eq!(
                guard.select_ready_instance_id(&selection).expect("selection"),
                Some("owner-1".to_string())
            );
            assert!(!guard.failure_states.contains_key("server-1"));
        }
        validation_pool
            .lock()
            .await
            .disconnect_non_blocking("server-1", "owner-1")
            .await
            .expect("validation fixture should disconnect");
        tokio::time::timeout(Duration::from_secs(1), validation_server)
            .await
            .expect("validation server should stop")
            .expect("validation server task should join");
    }

    mod rest_error_mapping {
        use super::{
            CapabilityAttemptError, CapabilityAuthenticationFailureCode, CapabilityOwnerError,
            CapabilityProjectionFailure, CapabilityReadError, CapabilityType, DiscoveryAttemptFailure, OwnerSource,
            RuntimeFailure, RuntimeFailureKind,
        };
        use crate::api::handlers::ApiError;
        use crate::core::capability::service::map_capability_read_error;
        use mcpmate_capability_store::CatalogError;

        fn discovery_failed(existing_error: CapabilityAttemptError) -> CapabilityReadError {
            CapabilityReadError::DiscoveryFailed {
                server_id: "server-1".to_string(),
                server_name: "docs".to_string(),
                operation: "tools/list",
                kind: CapabilityType::Tools,
                catalog_error: None,
                existing: Some(Box::new(DiscoveryAttemptFailure {
                    instance_id: Some("instance-1".to_string()),
                    connection_generation: None,
                    source: OwnerSource::Existing,
                    error: existing_error,
                })),
                fresh: None,
            }
        }

        #[test]
        fn connection_timeout_maps_to_gateway_timeout() {
            let error = discovery_failed(CapabilityAttemptError::Owner(CapabilityOwnerError::Timeout {
                timeout_ms: 750,
            }));

            assert!(matches!(map_capability_read_error(&error), ApiError::GatewayTimeout(_)));
        }

        #[test]
        fn operation_timeout_maps_to_request_timeout() {
            let error = discovery_failed(CapabilityAttemptError::Runtime(RuntimeFailure {
                kind: RuntimeFailureKind::Timeout,
                message: Some("request timeout".to_string()),
                timeout_ms: Some(500),
            }));

            assert!(matches!(map_capability_read_error(&error), ApiError::Timeout(_)));
        }

        #[test]
        fn authentication_failure_maps_to_unauthorized() {
            let error = discovery_failed(CapabilityAttemptError::Owner(CapabilityOwnerError::Authentication {
                code: CapabilityAuthenticationFailureCode::Unauthorized,
                reason: "401 from upstream".to_string(),
            }));

            assert!(matches!(map_capability_read_error(&error), ApiError::Unauthorized(_)));
        }

        #[test]
        fn runtime_authentication_failure_during_live_discovery_maps_to_unauthorized() {
            // Distinct from `authentication_failure_maps_to_unauthorized`: this failure comes
            // from the actual tools/list RPC call rejecting our credentials (a `Runtime`
            // failure), not from owner/session creation (an `Owner` failure). Both must map to
            // 401 so REST callers see a consistent, typed reason instead of a generic 502.
            let error = discovery_failed(CapabilityAttemptError::Runtime(RuntimeFailure {
                kind: RuntimeFailureKind::Authentication,
                message: Some("401 from upstream during tools/list".to_string()),
                timeout_ms: None,
            }));

            assert!(matches!(map_capability_read_error(&error), ApiError::Unauthorized(_)));
        }

        #[test]
        fn cleanup_authentication_failure_maps_to_unauthorized() {
            let error = CapabilityReadError::CleanupFailed {
                server_id: "server-1".to_string(),
                server_name: "docs".to_string(),
                operation: "tools/list",
                instance_id: "instance-1".to_string(),
                connection_generation: None,
                owner_source: OwnerSource::Existing,
                error: Box::new(CapabilityOwnerError::Authentication {
                    code: CapabilityAuthenticationFailureCode::Forbidden,
                    reason: "403 from upstream".to_string(),
                }),
            };

            assert!(matches!(map_capability_read_error(&error), ApiError::Unauthorized(_)));
        }

        #[test]
        fn catalog_failures_map_to_service_unavailable() {
            let untrusted = CapabilityReadError::CatalogUntrusted {
                server_id: "server-1".to_string(),
                source: CatalogError::InvalidValue {
                    field: "state",
                    value: "corrupted".to_string(),
                },
            };
            let operation = CapabilityReadError::CatalogOperation {
                server_id: "server-1".to_string(),
                source: anyhow::anyhow!("database unreachable"),
            };

            assert!(matches!(
                map_capability_read_error(&untrusted),
                ApiError::ServiceUnavailable(_)
            ));
            assert!(matches!(
                map_capability_read_error(&operation),
                ApiError::ServiceUnavailable(_)
            ));
        }

        #[test]
        fn exhausted_discovery_without_timeout_or_auth_maps_to_bad_gateway() {
            let error = discovery_failed(CapabilityAttemptError::Owner(CapabilityOwnerError::Missing {
                reason: "no owner available".to_string(),
            }));

            assert!(matches!(map_capability_read_error(&error), ApiError::BadGateway(_)));
        }

        #[test]
        fn projection_failure_maps_to_internal_error() {
            let error = CapabilityReadError::ProjectionFailed {
                server_id: "server-1".to_string(),
                server_name: "docs".to_string(),
                operation: "tools/list",
                kind: CapabilityType::Tools,
                instance_id: "instance-1".to_string(),
                connection_generation: None,
                owner_source: OwnerSource::Existing,
                source: CapabilityProjectionFailure(anyhow::anyhow!("projection decode failed")),
            };

            assert!(matches!(map_capability_read_error(&error), ApiError::InternalError(_)));
        }
    }
}
