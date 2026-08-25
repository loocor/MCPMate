use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};

use crate::audit::{storage::AuditStore, types::AuditEventDto};

const AUDIT_QUEUE_CAPACITY: usize = 512;
const AUDIT_BROADCAST_CAPACITY: usize = 256;

#[derive(Debug, Clone)]
pub struct AuditBroadcaster {
    sender: broadcast::Sender<AuditEventDto>,
}

impl AuditBroadcaster {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(AUDIT_BROADCAST_CAPACITY);
        Self { sender }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AuditEventDto> {
        self.sender.subscribe()
    }

    pub fn publish(
        &self,
        event: AuditEventDto,
    ) {
        let _ = self.sender.send(event);
    }
}

impl Default for AuditBroadcaster {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct AuditService {
    store: Arc<AuditStore>,
    broadcaster: AuditBroadcaster,
    intents: mpsc::Sender<AuditEventDto>,
}

impl AuditService {
    pub async fn new(store: Arc<AuditStore>) -> anyhow::Result<Self> {
        store.initialize().await?;
        let broadcaster = AuditBroadcaster::new();
        let (intents, mut rx) = mpsc::channel(AUDIT_QUEUE_CAPACITY);
        let service = Self {
            store: store.clone(),
            broadcaster: broadcaster.clone(),
            intents,
        };

        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                match store.insert(&event).await {
                    Ok(stored) => broadcaster.publish(stored),
                    Err(error) => tracing::warn!(error = %error, action = ?event.action, "Audit persistence failed"),
                }
            }
        });

        Ok(service)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AuditEventDto> {
        self.broadcaster.subscribe()
    }

    pub async fn emit(
        &self,
        event: AuditEventDto,
    ) {
        if let Err(error) = self.intents.try_send(event) {
            tracing::warn!(error = %error, "Audit queue is full; dropping event");
        }
    }

    pub async fn list(
        &self,
        filter: &crate::audit::types::AuditFilter,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> anyhow::Result<crate::audit::types::AuditListPage> {
        self.store.list(filter, cursor, limit).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::types::{AuditAction, AuditEvent, AuditFilter, AuditStatus};
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
    use std::{str::FromStr, time::Duration};
    use tempfile::tempdir;

    async fn setup_service() -> AuditService {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("audit.db");
        let url = format!("sqlite:{}", path.display());
        let options = SqliteConnectOptions::from_str(&url)
            .expect("options")
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_millis(5_000))
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("connect");
        crate::helpers::prepare_audit_database(&pool).await;
        let store = Arc::new(AuditStore::new(pool));
        AuditService::new(store).await.expect("audit service")
    }

    #[tokio::test]
    async fn persists_and_broadcasts() {
        let service = setup_service().await;
        let mut subscription = service.subscribe();
        let event = AuditEvent::new(AuditAction::ToolsCall, AuditStatus::Success)
            .with_client_id("client-a")
            .build();

        service.emit(event).await;

        let received = tokio::time::timeout(Duration::from_secs(2), subscription.recv())
            .await
            .expect("receive timeout")
            .expect("receive event");
        assert_eq!(received.action, AuditAction::ToolsCall);

        let stored = service
            .list(&AuditFilter::default(), None, Some(10))
            .await
            .expect("list events");
        assert_eq!(stored.events.len(), 1);
    }
}
