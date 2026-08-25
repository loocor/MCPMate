//! Core - refactor core module
//!
//! this is the refactor version of the original core module, using a more clear hierarchical architecture:
//! - foundation: infrastructure layer
//! - models: data models (independent)
//! - events: event system (independent first-level module)
//! - transport: transport layer
//! - pool: connection pool management
//! - protocol: protocol handling
//! - proxy: proxy core

pub mod capability;
pub mod estimate;
pub mod events;
pub mod foundation;
pub mod models;
pub mod oauth;
pub mod pool;
pub mod profile;
pub mod proxy;
pub mod secrets;
pub mod transport;

pub use events::{
    Event, EventBus, EventHandlers, EventReceiver, init as init_events,
    init_with_handlers as init_events_with_handlers, needs_transport_ready_wait, wait_for_transport_ready,
};
pub use proxy::{Args as ProxyArgs, ProxyServer};
