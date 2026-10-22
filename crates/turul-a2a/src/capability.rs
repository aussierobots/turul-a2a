//! Capability validation against the agent's own `AgentCard`.
//!
//! A2A v1.0.1 §3.3.4: an optional capability is supported only when
//! `AgentCard.capabilities` declares it `true`. A flag that is `false`
//! or not present means unsupported, and the operations that need it
//! are rejected:
//!
//! - push notification config Create / Get / List / Delete, and an
//!   inline `SendMessageConfiguration.taskPushNotificationConfig` →
//!   `PushNotificationNotSupportedError`;
//! - `SendStreamingMessage` / `SubscribeToTask` →
//!   `UnsupportedOperationError`;
//! - `GetExtendedAgentCard` → `UnsupportedOperationError`.
//!
//! Every transport (HTTP+JSON, JSON-RPC, gRPC, and the Lambda adapter,
//! which shares the HTTP router) reaches these operations through the
//! shared core handlers, and those call [`require`] before touching
//! storage.

use crate::error::A2aError;
use crate::executor::AgentExecutor;

/// An optional capability declared in `AgentCard.capabilities`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Capability {
    /// `AgentCapabilities.streaming`.
    Streaming,
    /// `AgentCapabilities.push_notifications`.
    PushNotifications,
    /// `AgentCapabilities.extended_agent_card`.
    ExtendedAgentCard,
}

impl Capability {
    fn flag(self, capabilities: &turul_a2a_proto::AgentCapabilities) -> Option<bool> {
        match self {
            Self::Streaming => capabilities.streaming,
            Self::PushNotifications => capabilities.push_notifications,
            Self::ExtendedAgentCard => capabilities.extended_agent_card,
        }
    }

    fn unsupported(self) -> A2aError {
        match self {
            Self::Streaming => A2aError::UnsupportedOperation {
                message: "streaming is not supported by this agent \
                          (AgentCard.capabilities.streaming is not true)"
                    .into(),
            },
            Self::PushNotifications => A2aError::PushNotificationNotSupported,
            Self::ExtendedAgentCard => A2aError::UnsupportedOperation {
                message: "the extended agent card is not supported by this agent \
                          (AgentCard.capabilities.extendedAgentCard is not true)"
                    .into(),
            },
        }
    }
}

/// `Ok` when the agent card served by `executor` declares `capability`
/// as `true`; otherwise the error A2A v1.0.1 §3.3.4 assigns to it.
pub(crate) fn require(
    executor: &dyn AgentExecutor,
    capability: Capability,
) -> Result<(), A2aError> {
    let declared = executor
        .agent_card()
        .capabilities
        .as_ref()
        .and_then(|c| capability.flag(c))
        .unwrap_or(false);
    if declared {
        Ok(())
    } else {
        Err(capability.unsupported())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::ExecutionContext;
    use turul_a2a_types::{Message, Task};

    struct CardOnly(Option<turul_a2a_proto::AgentCapabilities>);

    #[async_trait::async_trait]
    impl AgentExecutor for CardOnly {
        async fn execute(
            &self,
            _task: &mut Task,
            _msg: &Message,
            _ctx: &ExecutionContext,
        ) -> Result<(), A2aError> {
            Ok(())
        }

        fn agent_card(&self) -> turul_a2a_proto::AgentCard {
            turul_a2a_proto::AgentCard {
                capabilities: self.0.clone(),
                ..Default::default()
            }
        }
    }

    fn caps(value: Option<bool>) -> turul_a2a_proto::AgentCapabilities {
        turul_a2a_proto::AgentCapabilities {
            streaming: value,
            push_notifications: value,
            extended_agent_card: value,
            extensions: vec![],
        }
    }

    const ALL: [Capability; 3] = [
        Capability::Streaming,
        Capability::PushNotifications,
        Capability::ExtendedAgentCard,
    ];

    #[test]
    fn declared_true_is_supported() {
        let executor = CardOnly(Some(caps(Some(true))));
        for capability in ALL {
            assert!(require(&executor, capability).is_ok(), "{capability:?}");
        }
    }

    #[test]
    fn false_absent_or_missing_capabilities_is_unsupported() {
        for executor in [
            CardOnly(Some(caps(Some(false)))),
            CardOnly(Some(caps(None))),
            CardOnly(None),
        ] {
            assert!(matches!(
                require(&executor, Capability::PushNotifications),
                Err(A2aError::PushNotificationNotSupported)
            ));
            assert!(matches!(
                require(&executor, Capability::Streaming),
                Err(A2aError::UnsupportedOperation { .. })
            ));
            assert!(matches!(
                require(&executor, Capability::ExtendedAgentCard),
                Err(A2aError::UnsupportedOperation { .. })
            ));
        }
    }

    #[test]
    fn each_flag_is_read_independently() {
        let executor = CardOnly(Some(turul_a2a_proto::AgentCapabilities {
            streaming: Some(true),
            push_notifications: Some(false),
            extended_agent_card: None,
            extensions: vec![],
        }));
        assert!(require(&executor, Capability::Streaming).is_ok());
        assert!(require(&executor, Capability::PushNotifications).is_err());
        assert!(require(&executor, Capability::ExtendedAgentCard).is_err());
    }
}
