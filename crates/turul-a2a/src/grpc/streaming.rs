//! gRPC streaming adapters for `SendStreamingMessage` and
//! `SubscribeToTask`.
//!
//! Both RPCs open the shared task stream (`crate::streaming::task_stream`)
//! that HTTP and JSON-RPC use; this module only encodes its items as
//! `pb::StreamResponse`. Stored events serialise through the
//! pbjson-generated `serde` impls, so `last_chunk` on
//! `TaskArtifactUpdateEvent` is read back from the persisted event and
//! never invented here.

use tokio_stream::StreamExt;
use tonic::Status;
use turul_a2a_proto as pb;

use crate::grpc::error::a2a_to_status;
use crate::grpc::service::BoxedStreamResponseStream;
use crate::router::{self, AppState};
use crate::streaming::task_stream::{StreamItem, TaskEventStream};

pub async fn handle_send_streaming_message(
    state: AppState,
    tenant: String,
    owner: String,
    body: String,
) -> Result<BoxedStreamResponseStream, Status> {
    // gRPC auth claims are not yet wired. Pass None for now.
    let stream = router::open_send_stream(state, &tenant, &owner, None, body)
        .await
        .map_err(a2a_to_status)?;
    Ok(grpc_stream(stream))
}

pub async fn handle_subscribe_to_task(
    state: AppState,
    tenant: String,
    owner: String,
    task_id: String,
) -> Result<BoxedStreamResponseStream, Status> {
    let stream = router::open_subscribe_stream(state, &tenant, &owner, &task_id)
        .await
        .map_err(a2a_to_status)?;
    Ok(grpc_stream(stream))
}

fn grpc_stream(stream: TaskEventStream) -> BoxedStreamResponseStream {
    Box::pin(stream.items.map(|item| match item {
        Ok(StreamItem::Task(task)) => Ok(pb::StreamResponse {
            payload: Some(pb::stream_response::Payload::Task((*task).into_proto())),
        }),
        Ok(StreamItem::Event(event)) => {
            let value = serde_json::to_value(&event).unwrap_or_default();
            serde_json::from_value::<pb::StreamResponse>(value).map_err(|err| {
                Status::internal(format!(
                    "grpc adapter: failed to encode stream event: {err}"
                ))
            })
        }
        Err(e) => Err(a2a_to_status(e)),
    }))
}
