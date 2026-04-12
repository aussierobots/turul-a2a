//! Client prelude.
//!
//! ```rust
//! use turul_a2a_client::prelude::*;
//! ```

pub use crate::builders::MessageBuilder;
pub use crate::error::A2aClientError;
pub use crate::sse::{SseEvent, SseStream};
pub use crate::{A2aClient, ClientAuth, ListTasksParams};
