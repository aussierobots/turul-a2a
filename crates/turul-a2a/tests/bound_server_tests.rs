//! `A2aServer::bind` binds the configured address and reports the address
//! it actually holds before serving, so a caller can bind port 0 and learn
//! the OS-assigned port without a reserve-then-rebind race.

use turul_a2a::A2aServer;
use turul_a2a::card_builder::AgentCardBuilder;
use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a_types::{Message, Task};

struct CardOnlyExecutor;

#[async_trait::async_trait]
impl AgentExecutor for CardOnlyExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        _msg: &Message,
        _ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        task.complete();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        AgentCardBuilder::new("Bound Port Agent", "1.0.0")
            .description("Reports its bound address")
            .url("http://localhost", "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .build()
            .unwrap()
    }
}

fn server_on(addr: ([u8; 4], u16)) -> A2aServer {
    A2aServer::builder()
        .executor(CardOnlyExecutor)
        .bind(addr)
        .build()
        .expect("build server")
}

#[tokio::test]
async fn bind_port_zero_reports_the_assigned_port_and_serves_on_it() {
    let bound = server_on(([127, 0, 0, 1], 0))
        .bind()
        .await
        .expect("bind port 0");
    let addr = bound.local_addr();
    assert_eq!(addr.ip().to_string(), "127.0.0.1");
    assert_ne!(addr.port(), 0, "the OS-assigned port is reported");

    let serving = tokio::spawn(bound.run());
    let card: serde_json::Value =
        reqwest::get(format!("http://{addr}/.well-known/agent-card.json"))
            .await
            .expect("agent card request")
            .json()
            .await
            .expect("agent card JSON");
    assert_eq!(card["name"], "Bound Port Agent");

    serving.abort();
    let _ = serving.await;
}

#[tokio::test]
async fn bind_holds_the_port_before_run() {
    let bound = server_on(([127, 0, 0, 1], 0))
        .bind()
        .await
        .expect("bind port 0");
    let addr = bound.local_addr();

    // The port is held from `bind`, not from `run`: nothing else can take it.
    assert!(
        std::net::TcpListener::bind(addr).is_err(),
        "{addr} must already be held by the bound server"
    );
    drop(bound);
}

#[tokio::test]
async fn bind_fails_when_the_address_is_taken() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve a port");
    let port = taken.local_addr().unwrap().port();

    let result = server_on(([127, 0, 0, 1], port)).bind().await;
    assert!(
        matches!(result, Err(A2aError::Internal(_))),
        "binding a held address fails"
    );
}
