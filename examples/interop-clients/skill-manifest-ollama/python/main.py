"""Python A2A client for the `skill-manifest-ollama-agent`.

This script sends one greeting request and prints the streamed response.

Target agent
------------
- Crate    : `skill-manifest-ollama-agent`
- Path     : `examples/skill-manifest-ollama-agent/`
- Default  : `http://localhost:3010` (overridable via `A2A_BASE_URL`)
- Skill    : `greet` (declared in `skills/demo/SKILL.md`)

Wire path
---------
- AgentCard advertises `supportedInterfaces[0].protocolBinding=JSONRPC`.
- The SDK selects JSON-RPC and POSTs to `/jsonrpc`.
- Because the SDK sets `Accept: text/event-stream`, the wire `method` is
  `SendStreamingMessage` even though the Python call is `send_message`.
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

BASE_URL = os.environ.get("A2A_BASE_URL", "http://localhost:3010")

# The `greet` skill's inputSchema accepts a `user.name` + optional `style`.
# Encoded as a single JSON-text part; the agent parses it and validates
# against the manifest before dispatching to the offline stub or Ollama.
GREET_PAYLOAD = '{"user":{"name":"Ada"},"style":"formal"}'


async def main() -> int:
    async with httpx.AsyncClient(timeout=30.0) as http:
        resolver = A2ACardResolver(httpx_client=http, base_url=BASE_URL)
        card = await resolver.get_agent_card()

        print("=== AgentCard ===")
        print(f"  name       : {card.name}")
        print(f"  version    : {card.version}")
        if not card.supported_interfaces:
            print("  interface  : <none advertised>")
            return 2
        iface = card.supported_interfaces[0]
        print(
            "  interface  : "
            f"binding={iface.protocol_binding!r} "
            f"url={iface.url!r}"
        )

        # Verified `a2a-sdk==1.0.2` constructor path: factory inspects the
        # card and selects the JSON-RPC transport. Do NOT swap for a
        # `create_client(url)` shortcut without re-verifying against the SDK.
        factory = ClientFactory(ClientConfig(httpx_client=http))
        client = factory.create(card)

        message = Message(
            role=Role.ROLE_USER,
            parts=[Part(text=GREET_PAYLOAD)],
            message_id=uuid4().hex,
        )
        request = SendMessageRequest(
            message=message,
            configuration=SendMessageConfiguration(),
        )

        print()
        print("=== Sending `greet` request ===")
        print(f"  payload    : {GREET_PAYLOAD}")
        print(f"  message_id : {message.message_id}")
        print()

        print("=== Stream events ===")
        chunk_index = 0
        async for chunk in client.send_message(request):
            chunk_index += 1
            print(f"--- chunk #{chunk_index} ---")
            print(chunk)

        print()
        print(f"=== Done: {chunk_index} stream events received ===")
        return 0


if __name__ == "__main__":
    try:
        exit_code = asyncio.run(main())
    except Exception:
        traceback.print_exc()
        sys.exit(1)
    sys.exit(exit_code)
