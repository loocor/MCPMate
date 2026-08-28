// Service module index for client configuration management
// This file intentionally contains no business logic.

pub mod apply;
pub mod backup;
pub mod core;
pub mod list;
pub mod query;
pub mod reapply;
pub mod rules;
pub mod settings;
pub mod skill_mount;
pub mod state;
pub mod sync;

// Re-exports for external callers
pub use core::{
    ApplyOutcome, ClientBackupRecord, ClientConfigService, ClientDescriptor, ClientRenderOptions, ClientRenderResult,
    PreviewOutcome,
};
pub use reapply::HostedClientReapplySummary;
