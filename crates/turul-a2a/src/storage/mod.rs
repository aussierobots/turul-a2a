// The artifact-separation helpers are consumed only by the persistent
// backends (SQLite, PostgreSQL, DynamoDB); the in-memory backend stores
// whole tasks and needs none of them. Gate the module so a default build
// (in-memory only) does not carry it as dead code.
#[cfg(any(feature = "sqlite", feature = "postgres", feature = "dynamodb"))]
pub(crate) mod artifacts;
pub mod atomic;
pub mod error;
pub mod event_store;
pub mod filter;
pub mod retention;
pub mod terminal_cas;
pub mod traits;

#[cfg(feature = "in-memory")]
pub mod in_memory;

#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "postgres")]
pub mod postgres;

#[cfg(feature = "dynamodb")]
pub mod dynamodb;

#[cfg(test)]
pub(crate) mod parity_tests;

pub use atomic::A2aAtomicStore;
pub use error::A2aStorageError;
pub use event_store::A2aEventStore;
pub use filter::{PushConfigListPage, TaskFilter, TaskListPage};
pub use retention::RetentionConfig;
pub use traits::{A2aCancellationSupervisor, A2aPushNotificationStorage, A2aTaskStorage};

#[cfg(feature = "in-memory")]
pub use in_memory::InMemoryA2aStorage;
