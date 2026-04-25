//! Interrupting Agent library — executor + agent card lifted out of
//! `src/main.rs` so the smoke test in `tests/smoke.rs` can drive the
//! same code in-process.

use turul_a2a::card_builder::{AgentCardBuilder, AgentSkillBuilder};
use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a_types::{Artifact, Message, Part, Role, Task, TaskState, TaskStatus};

pub struct FlightBookingExecutor;

fn first_user_text(history: &[turul_a2a_proto::Message]) -> Option<String> {
    history
        .iter()
        .find(|m| m.role == turul_a2a_proto::Role::User as i32)
        .and_then(|m| {
            m.parts.iter().find_map(|p| match &p.content {
                Some(turul_a2a_proto::part::Content::Text(t)) => Some(t.clone()),
                _ => None,
            })
        })
}

#[async_trait::async_trait]
impl AgentExecutor for FlightBookingExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        message: &Message,
        _ctx: &turul_a2a::executor::ExecutionContext,
    ) -> Result<(), A2aError> {
        // Detect turn from the user-message count in history. Turn 1
        // has the originating message (count == 1); turn 2 has the
        // originating message + the answer (count >= 2). The
        // framework appends the inbound user message to history
        // before invoking the executor on both new and continuation
        // paths; agent-side mutations to history are NOT persisted
        // (only status + artifacts are committed), so don't rely on
        // counting agent messages.
        let user_message_count = task
            .history()
            .iter()
            .filter(|m| m.role == turul_a2a_proto::Role::User as i32)
            .count();

        let user_text = message.joined_text();

        if user_message_count <= 1 {
            // First turn: no destination yet. Pause for input.
            let question = Message::new(
                uuid::Uuid::now_v7().to_string(),
                Role::Agent,
                vec![Part::text(
                    "Where would you like to fly? Please reply with the destination.",
                )],
            );
            task.set_status(TaskStatus::new(TaskState::InputRequired).with_message(question));
            return Ok(());
        }

        // Continuation: user just answered the question.
        let booking = format!(
            "Booked flight to {dest}.\n\
             original request: {first}\n\
             confirmed answer: {dest}",
            dest = user_text.trim(),
            first = first_user_text(task.history()).unwrap_or_default(),
        );
        task.append_artifact(
            Artifact::new("booking-confirmation", vec![Part::text(booking)])
                .with_name("flight_booking.txt"),
        );
        task.complete();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        AgentCardBuilder::new("Interrupting Agent", "0.1.0")
            .description(
                "Demonstrates the INPUT_REQUIRED interrupted state. \
                 The first message starts a flight-booking task; the \
                 agent pauses the task with INPUT_REQUIRED while asking \
                 for the destination. The client sends a follow-up \
                 message carrying the same taskId with the answer; the \
                 task continues to COMPLETED.",
            )
            .url("http://localhost:3008/jsonrpc", "JSONRPC", "1.0")
            .provider("Example Org", "https://example.com")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .skill(
                AgentSkillBuilder::new(
                    "flight-booking",
                    "Flight Booking (with input-required interrupt)",
                    "Books a flight in two turns. Turn 1: state \
                     transitions to INPUT_REQUIRED with a question \
                     message asking for the destination. Turn 2: client \
                     replies with the destination, the task completes \
                     and emits a booking-confirmation artifact.",
                )
                .tags(vec!["interrupted-state", "input-required", "multi-turn"])
                .examples(vec![
                    "Turn 1: `{\"message\":{\"role\":\"ROLE_USER\",\"parts\":[{\"text\":\"book a flight\"}]}}` → task with status TASK_STATE_INPUT_REQUIRED and a question.",
                    "Turn 2: `{\"message\":{\"role\":\"ROLE_USER\",\"taskId\":\"<task.id>\",\"contextId\":\"<task.contextId>\",\"parts\":[{\"text\":\"Helsinki\"}]}}` → task TASK_STATE_COMPLETED with booking artifact.",
                ])
                .build(),
            )
            .build()
            .expect("Interrupting agent card should be valid")
    }
}
