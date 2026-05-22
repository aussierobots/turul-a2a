"""Python A2A client for the `agent-role-planner-router-agent`.

This script sends two messages sequentially:

1. `add 3 5`             → the agent's planner picks the `add` skill and
                           returns `{"result": 8}`.
2. `concat: foo bar baz` → the planner picks `concat` and returns
                           `{"joined": "foo bar baz"}`.

Target agent
------------
- Crate    : `agent-role-planner-router-agent`
- Path     : `examples/agent-role-planner-router-agent/`
- Default  : `http://localhost:3012` (overridable via `A2A_BASE_URL`)
- Skills   : `add`, `concat`

Wire path
---------
JSON-RPC over `/jsonrpc`; the SDK sets `Accept: text/event-stream` so the
wire method is `SendStreamingMessage`.
"""

from __future__ import annotations

import asyncio
import os
import sys
import traceback
from uuid import uuid4

import httpx
from a2a.client import A2ACardResolver, ClientConfig, ClientFactory
from a2a.types import (
    Message,
    Part,
    Role,
    SendMessageConfiguration,
    SendMessageRequest,
)

BASE_URL = os.environ.get("A2A_BASE_URL", "http://localhost:3012")

PROMPTS = [
    ("add 3 5", "expects {\"result\": 8}"),
    ("concat: foo bar baz", "expects {\"joined\": \"foo bar baz\"}"),
]


async def send_one(client, prompt: str, label: str) -> None:
    """Send a single prompt and print every stream event it produces."""
    message = Message(
        role=Role.ROLE_USER,
        parts=[Part(text=prompt)],
        message_id=uuid4().hex,
    )
    request = SendMessageRequest(
        message=message,
        configuration=SendMessageConfiguration(),
    )

    print()
    print(f"=== Sending: {prompt!r} ({label}) ===")
    print(f"  message_id : {message.message_id}")
    chunk_index = 0
    async for chunk in client.send_message(request):
        chunk_index += 1
        print(f"--- chunk #{chunk_index} ---")
        print(chunk)
    print(f"=== Done: {chunk_index} stream events ===")


async def main() -> int:
    async with httpx.AsyncClient(timeout=30.0) as http:
        resolver = A2ACardResolver(httpx_client=http, base_url=BASE_URL)
        card = await resolver.get_agent_card()

        print("=== AgentCard ===")
        print(f"  name      : {card.name}")
        print(f"  version   : {card.version}")
        if not card.supported_interfaces:
            print("  interface : <none advertised>")
            return 2
        iface = card.supported_interfaces[0]
        print(
            "  interface : "
            f"binding={iface.protocol_binding!r} url={iface.url!r}"
        )

        # Verified `a2a-sdk==1.0.2` constructor path: the factory picks the
        # JSON-RPC transport from `card.supported_interfaces`.
        factory = ClientFactory(ClientConfig(httpx_client=http))
        client = factory.create(card)

        for prompt, label in PROMPTS:
            await send_one(client, prompt, label)

        return 0


if __name__ == "__main__":
    try:
        exit_code = asyncio.run(main())
    except Exception:
        traceback.print_exc()
        sys.exit(1)
    sys.exit(exit_code)
