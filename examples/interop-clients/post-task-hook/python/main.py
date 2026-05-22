"""Python A2A client for the `post-task-hook-agent`.

This script sends four messages sequentially:

1. `count 3`   → expects `{"squared": 9}`. Sent three times so the in-agent
                 outcome counter advances to `success=3`.
2. `metrics`   → expects `{"success": 3, "failure": 0, "last": "ok(count): ..."}`
                 (verifying the post-task terminal hook actually ran).

Target agent
------------
- Crate    : `post-task-hook-agent`
- Path     : `examples/post-task-hook-agent/`
- Default  : `http://localhost:3014` (overridable via `A2A_BASE_URL`)
- Skills   : `count`, `metrics`

Wire path is JSON-RPC over `/jsonrpc`. The SDK's `client.send_message(...)`
maps to wire method `SendStreamingMessage` because the SDK sets
`Accept: text/event-stream`.
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

BASE_URL = os.environ.get("A2A_BASE_URL", "http://localhost:3014")

# Sequence: three `count 3` calls (each yields {"squared": 9}) followed by
# one `metrics` call so the hook-recorded counter is visible on the wire.
PROMPTS = [
    ("count 3", "expects {\"squared\": 9} (1/3)"),
    ("count 3", "expects {\"squared\": 9} (2/3)"),
    ("count 3", "expects {\"squared\": 9} (3/3)"),
    ("metrics", "expects {\"success\": 3, \"failure\": 0, \"last\": \"ok(count): ...\"}"),
]


async def send_one(client, prompt: str, label: str) -> None:
    """Send a single prompt and print every stream event."""
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

        # Verified `a2a-sdk==1.0.2` constructor path.
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
