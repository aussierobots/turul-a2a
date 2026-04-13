//! Client prelude.
//!
//! ```rust
//! use turul_a2a_client::prelude::*;
//! ```

pub use crate::builders::MessageBuilder;
pub use crate::error::A2aClientError;
pub use crate::response::{SendResponse, ListResponse, artifact_text, first_data_artifact};
pub use crate::sse::{SseEvent, SseStream};
pub use crate::{A2aClient, ClientAuth, ListTasksParams};
