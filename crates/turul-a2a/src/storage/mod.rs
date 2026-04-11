pub mod error;
pub mod event_store;
pub mod filter;
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

pub use error::A2aStorageError;
pub use event_store::A2aEventStore;
pub use filter::{PushConfigListPage, TaskFilter, TaskListPage};
pub use traits::{A2aPushNotificationStorage, A2aTaskStorage};

#[cfg(feature = "in-memory")]
pub use in_memory::InMemoryA2aStorage;
