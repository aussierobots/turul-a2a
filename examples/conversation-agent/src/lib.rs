//! Conversation Agent library — executor + agent card lifted out of
//! `src/main.rs` so the smoke test in `tests/smoke.rs` can drive the
//! same code in-process.

use turul_a2a::card_builder::{AgentCardBuilder, AgentSkillBuilder};
use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a_types::{Artifact, Message, Part, Task};

pub const ARTIFACT_NAME: &str = "sailboat_image.png";
pub const ARTIFACT_ID_V1: &str = "sailboat-v1";
pub const ARTIFACT_ID_V2: &str = "sailboat-v2";

pub struct ConversationExecutor;

#[async_trait::async_trait]
impl AgentExecutor for ConversationExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        message: &Message,
        _ctx: &turul_a2a::executor::ExecutionContext,
    ) -> Result<(), A2aError> {
        let prompt = message.joined_text();
        let reference_task_ids = &message.as_proto().reference_task_ids;
        let task_id = task.as_proto().id.clone();

        let body = if reference_task_ids.is_empty() {
            format!(
                "[v1] Generated image of {prompt}.\n\
                 task_id      = {task_id}\n\
                 artifact     = {ARTIFACT_NAME}\n\
                 artifactId   = {ARTIFACT_ID_V1}\n\
                 references   = (none — this is the originating task)"
            )
        } else {
            let refs = reference_task_ids.join(", ");
            format!(
                "[v2] Refined image of {prompt}.\n\
                 task_id      = {task_id}\n\
                 artifact     = {ARTIFACT_NAME}  (same name as prior version)\n\
                 artifactId   = {ARTIFACT_ID_V2}     (new id — refinement creates a new artifact)\n\
                 references   = [{refs}]"
            )
        };

        let artifact_id = if reference_task_ids.is_empty() {
            ARTIFACT_ID_V1
        } else {
            ARTIFACT_ID_V2
        };
        task.append_artifact(
            Artifact::new(artifact_id, vec![Part::text(body)]).with_name(ARTIFACT_NAME),
        );
        task.complete();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        AgentCardBuilder::new("Conversation Agent", "0.1.0")
            .description(
                "Demonstrates task refinement within a single contextId. \
                 First request creates an originating task and an artifact; \
                 a follow-up message in the same context with referenceTaskIds \
                 pointing at the prior task creates a new task whose artifact \
                 reuses the same name but carries a new artifactId.",
            )
            .url("http://localhost:3007/jsonrpc", "JSONRPC", "1.0")
            .provider("Example Org", "https://example.com")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .skill(
                AgentSkillBuilder::new(
                    "image-refinement",
                    "Image Refinement",
                    "Generates a placeholder image artifact on the first \
                     request, then accepts refinement requests via \
                     referenceTaskIds in subsequent messages within the \
                     same contextId. Refined artifacts share the same name \
                     and increment the artifactId.",
                )
                .tags(vec!["conversation", "refinement", "context"])
                .examples(vec![
                    "Step 1: send `{\"message\":{\"role\":\"ROLE_USER\",\"parts\":[{\"text\":\"sailboat\"}]}}` — get task1.",
                    "Step 2: send `{\"message\":{\"role\":\"ROLE_USER\",\"contextId\":\"<task1.contextId>\",\"referenceTaskIds\":[\"<task1.id>\"],\"parts\":[{\"text\":\"make it red\"}]}}` — get task2 with same contextId, new artifactId, same artifact name.",
                ])
                .build(),
            )
            .build()
            .expect("Conversation agent card should be valid")
    }
}
