//! Helpers shared by the integration-test crates.

use turul_a2a_types::wire::protocol;

/// Raw tonic client channel that stamps every call with the `a2a-version`
/// metadata a conforming client sends (spec Section 3.6.1).
pub type VersionedChannel = tonic::service::interceptor::InterceptedService<
    tonic::transport::Channel,
    fn(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status>,
>;

/// Interceptor that adds `a2a-version: <supported version>` to a call.
pub fn with_a2a_version(mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
    req.metadata_mut().insert(
        tonic::metadata::MetadataKey::from_bytes(protocol::VERSION_PARAM.as_bytes())
            .expect("valid metadata key"),
        tonic::metadata::MetadataValue::from_static(protocol::VERSION),
    );
    Ok(req)
}
