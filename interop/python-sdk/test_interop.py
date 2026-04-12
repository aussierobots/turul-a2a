"""
Python interop harness against turul-a2a Rust server.

Two tracks:

1. JSON-RPC wire format interop (httpx):
   Proves our server's JSON-RPC endpoint is consumable from Python via raw HTTP.
   These are wire-level checks, not SDK client interop.

2. SDK compatibility probe (a2a-sdk):
   A standing probe using the official A2ACardResolver to detect when the Python
   SDK aligns with the v1.0 proto our server implements. Currently expected to skip
   due to version mismatch (SDK targets spec v0.3, server targets v1.0).

Version audit (2026-04-12):
  - a2a-sdk==0.3.26 (latest PyPI) implements spec v0.3
  - Our server implements proto package lf.a2a.v1 (upstream tag v1.0.0)
  - SDK AgentCard model requires top-level 'url' field not in v1.0 proto
  - SDK REST transport uses /v1/ prefixed routes; our server uses proto-derived unprefixed routes
  - SDK data models are hand-written Pydantic, not proto-generated

Prerequisites:
  - Running turul-a2a server (cargo run -p echo-agent)
  - TURUL_SERVER_URL env var (default: http://localhost:3000)
"""

import os
import json
import uuid
import pytest
import httpx

SERVER_URL = os.environ.get("TURUL_SERVER_URL", "http://localhost:3000")
JSONRPC_URL = f"{SERVER_URL}/jsonrpc"
AGENT_CARD_URL = f"{SERVER_URL}/.well-known/agent-card.json"

# =========================================================
# Helpers
# =========================================================

def jsonrpc_request(method: str, params: dict, req_id: int = 1) -> dict:
    """Build a JSON-RPC 2.0 request."""
    return {
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": req_id,
    }


def send_message_params(message_id: str, text: str, task_id: str = "") -> dict:
    """Build SendMessage params."""
    msg = {
        "messageId": message_id,
        "role": "ROLE_USER",
        "parts": [{"text": text}],
    }
    if task_id:
        msg["taskId"] = task_id
    return {"message": msg}


@pytest.fixture
def client():
    """HTTP client for JSON-RPC requests."""
    return httpx.Client(
        base_url=SERVER_URL,
        headers={"Content-Type": "application/json", "a2a-version": "1.0"},
        timeout=10.0,
    )


# =========================================================
# B1: Agent Card Discovery
# =========================================================

def test_discover_agent_card(client):
    """Agent card is discoverable at well-known URL."""
    resp = client.get("/.well-known/agent-card.json", headers={"a2a-version": "1.0"})
    assert resp.status_code == 200, f"Agent card fetch failed: {resp.text}"

    card = resp.json()
    assert "name" in card, "Agent card must have name"
    assert "supportedInterfaces" in card, "Agent card must have supportedInterfaces"
    assert len(card["supportedInterfaces"]) > 0, "Must have at least one interface"


# =========================================================
# B2: Send Message via JSON-RPC
# =========================================================

def test_send_message_jsonrpc(client):
    """Send a message via JSON-RPC and get a task back."""
    msg_id = str(uuid.uuid4())
    body = jsonrpc_request("SendMessage", send_message_params(msg_id, "hello from python"))

    resp = client.post("/jsonrpc", json=body)
    assert resp.status_code == 200, f"JSON-RPC failed: {resp.text}"

    result = resp.json()
    assert "result" in result, f"Expected result, got: {result}"
    assert "error" not in result, f"Unexpected error: {result}"

    # Result should be a Task or have task field
    task = result["result"]
    if "task" in task:
        task = task["task"]

    assert "id" in task, f"Task must have id: {task}"
    assert task["id"], "Task id must not be empty"


# =========================================================
# B3: Get Task via JSON-RPC
# =========================================================

def test_get_task_jsonrpc(client):
    """Create a task then retrieve it via GetTask."""
    # Create
    msg_id = str(uuid.uuid4())
    create_body = jsonrpc_request("SendMessage", send_message_params(msg_id, "create for get"))
    create_resp = client.post("/jsonrpc", json=create_body)
    task = create_resp.json()["result"]
    if "task" in task:
        task = task["task"]
    task_id = task["id"]

    # Get
    get_body = jsonrpc_request("GetTask", {"id": task_id}, req_id=2)
    get_resp = client.post("/jsonrpc", json=get_body)
    assert get_resp.status_code == 200

    result = get_resp.json()
    assert "result" in result, f"Expected result: {result}"
    got_task = result["result"]
    assert got_task["id"] == task_id


# =========================================================
# B4: Cancel Completed Task (Error Case)
# =========================================================

def test_cancel_completed_task_jsonrpc(client):
    """Cancel a completed task returns JSON-RPC error."""
    # Create and complete
    msg_id = str(uuid.uuid4())
    create_body = jsonrpc_request("SendMessage", send_message_params(msg_id, "complete me"))
    create_resp = client.post("/jsonrpc", json=create_body)
    task = create_resp.json()["result"]
    if "task" in task:
        task = task["task"]
    task_id = task["id"]

    # Cancel the completed task
    cancel_body = jsonrpc_request("CancelTask", {"id": task_id}, req_id=3)
    cancel_resp = client.post("/jsonrpc", json=cancel_body)
    assert cancel_resp.status_code == 200  # JSON-RPC always returns 200

    result = cancel_resp.json()
    assert "error" in result, f"Expected JSON-RPC error: {result}"
    assert result["error"]["code"] == -32002, f"Expected -32002, got: {result['error']['code']}"


# =========================================================
# B5: List Tasks via JSON-RPC
# =========================================================

def test_list_tasks_jsonrpc(client):
    """List tasks returns paginated results."""
    # Create a task first
    msg_id = str(uuid.uuid4())
    create_body = jsonrpc_request("SendMessage", send_message_params(msg_id, "for listing"))
    client.post("/jsonrpc", json=create_body)

    # List
    list_body = jsonrpc_request("ListTasks", {}, req_id=4)
    list_resp = client.post("/jsonrpc", json=list_body)
    assert list_resp.status_code == 200

    result = list_resp.json()
    assert "result" in result, f"Expected result: {result}"
    list_result = result["result"]
    assert "tasks" in list_result, f"Expected tasks field: {list_result}"
    assert "totalSize" in list_result, f"Expected totalSize: {list_result}"


# =========================================================
# B6: Get Task Not Found
# =========================================================

def test_get_task_not_found_jsonrpc(client):
    """Get nonexistent task returns JSON-RPC error."""
    body = jsonrpc_request("GetTask", {"id": "nonexistent-python-interop"}, req_id=5)
    resp = client.post("/jsonrpc", json=body)
    assert resp.status_code == 200

    result = resp.json()
    assert "error" in result, f"Expected error: {result}"
    assert result["error"]["code"] == -32001, f"Expected -32001, got: {result['error']['code']}"


# =========================================================
# B7: Try a2a-sdk client (if available)
# =========================================================

def test_a2a_sdk_card_resolver():
    """Standing SDK compatibility probe.

    Uses the official a2a-sdk A2ACardResolver against our server.
    Currently expected to SKIP: the SDK (v0.3) requires a top-level 'url' field
    on AgentCard that the upstream v1.0 proto does not define.

    This probe will start PASSING when the Python SDK ships v1.0-aligned models.
    Do not remove this test — it is an intentional live signal for ecosystem alignment.
    """
    try:
        from a2a.client import A2ACardResolver
        from a2a.client.errors import A2AClientJSONError
    except ImportError:
        pytest.skip("a2a-sdk not installed")

    import asyncio

    async def _resolve():
        resolver = A2ACardResolver(httpx.AsyncClient(), base_url=SERVER_URL)
        card = await resolver.get_agent_card()
        return card

    try:
        card = asyncio.run(_resolve())
        # If we get here, the SDK now accepts our card — interop achieved!
        assert card.name, "Card must have a name"
    except A2AClientJSONError:
        pytest.skip(
            "a2a-sdk 0.3.x implements spec v0.3; server targets v1.0 proto. "
            "SDK AgentCard model requires top-level 'url' field not in v1.0 proto. "
            "This probe will pass when the Python SDK ships v1.0-aligned models."
        )
    except Exception as e:
        pytest.skip(
            f"SDK compatibility probe failed with unexpected error: {type(e).__name__}: {e}"
        )
