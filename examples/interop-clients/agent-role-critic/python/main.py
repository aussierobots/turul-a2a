"""Python A2A client for the `agent-role-critic-agent`.

This script sends two messages sequentially:

1. `validate_against_schema` — checks that `42` is a valid integer.
   Expected output:  `{"valid": true, "errors": []}`.
2. `check_invariants` — runs a `non_empty` + `min_length` table over the
   string `"hello"`.
   Expected output:  `{"verdict": "pass", "failures": []}`.

Target agent
------------
- Crate    : `agent-role-critic-agent`
- Path     : `examples/agent-role-critic-agent/`
- Default  : `http://localhost:3013` (overridable via `A2A_BASE_URL`)
- Skills   : `validate_against_schema`, `check_invariants`

The agent dispatches based on the inbound JSON's `kind` field. Both
requests are encoded as a single JSON-text part.
"""

from __future__ import annotations

import asyncio
import json
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

BASE_URL = os.environ.get("A2A_BASE_URL", "http://localhost:3013")

VALIDATE_PAYLOAD = json.dumps(
    {
        "kind": "validate_against_schema",
        "value": 42,
        "schema": {"type": "integer"},
    }
)

INVARIANTS_PAYLOAD = json.dumps(
    {
        "kind": "check_invariants",
        "value": "hello",
        "invariants": [
            {"name": "non_empty", "check": "non_empty"},
            {"name": "min_length_3", "check": "min_length", "args": {"min": 3}},
        ],
    }
)


async def send_one(client, payload: str, label: str) -> None:
    """Send a single payload and print each streamed event."""
    message = Message(
        role=Role.ROLE_USER,
        parts=[Part(text=payload)],
        message_id=uuid4().hex,
    )
    request = SendMessageRequest(
        message=message,
        configuration=SendMessageConfiguration(),
    )

    print()
    print(f"=== Sending: {label} ===")
    print(f"  payload    : {payload}")
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

        # Verified `a2a-sdk==1.0.2` constructor path: factory selects the
        # JSON-RPC transport advertised by the AgentCard.
        factory = ClientFactory(ClientConfig(httpx_client=http))
        client = factory.create(card)

        await send_one(client, VALIDATE_PAYLOAD, "validate_against_schema")
        await send_one(client, INVARIANTS_PAYLOAD, "check_invariants")
        return 0


if __name__ == "__main__":
    try:
        exit_code = asyncio.run(main())
    except Exception:
        traceback.print_exc()
        sys.exit(1)
    sys.exit(exit_code)
