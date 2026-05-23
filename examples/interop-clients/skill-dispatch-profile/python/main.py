"""Python A2A client for the `skill-dispatch-profile-agent`.

Demonstrates the skill-invocation dispatcher profile end-to-end:

1. The HTTP layer sends the `A2A-Extensions` request header advertising
   the profile URI. We attach it as a default header on the underlying
   `httpx.AsyncClient` so every outbound request (agent-card fetch,
   `SendStreamingMessage`, etc.) carries it.
2. The application layer sets two reserved keys on `Message.metadata`:
   `a2a.skillId` (which skill to dispatch to) and `a2a.skillParams`
   (structured inputs). `Message.metadata` is a `google.protobuf.Struct`
   in the SDK's protobuf-generated types, so we build it with
   `google.protobuf.json_format.ParseDict`.
3. The server echoes the honoured profile URIs in the response
   `A2A-Extensions` header. We capture it via an `httpx` response
   event hook.

Two calls are sent in sequence:
- `echo_loud` with `{"text": "hello"}`  -> artifact `{"shouted":"HELLO"}`
- `reverse`   with `{"text": "abc"}`   -> artifact `{"reversed":"cba"}`
"""

from __future__ import annotations

import asyncio
import os
import sys
import traceback
from typing import Any
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
from google.protobuf.json_format import ParseDict
from google.protobuf.struct_pb2 import Struct

BASE_URL = os.environ.get("A2A_BASE_URL", "http://localhost:3015")
PROFILE_URI = "https://turul.dev/a2a/extensions/skill-invocation/v1"


def build_metadata(skill_id: str, params: dict[str, Any]) -> Struct:
    """Pack the two reserved profile keys into a google.protobuf.Struct.

    The SDK's `Message.metadata` field is typed as
    `google.protobuf.Struct`; `ParseDict` recursively converts a plain
    Python dict (including the nested `skillParams` object) into the
    matching protobuf Value tree.
    """
    md = Struct()
    ParseDict(
        {"a2a.skillId": skill_id, "a2a.skillParams": params},
        md,
    )
    return md


def extract_artifact_text(chunks: list[Any]) -> str | None:
    """Walk the streamed lifecycle events and return the first artifact
    text part. The agent emits a single artifact with `last_chunk=true`.
    """
    for chunk in chunks:
        artifact_update = getattr(chunk, "artifact_update", None)
        if artifact_update is None:
            continue
        # protobuf wraps optionals; presence is checked via HasField.
        if not chunk.HasField("artifact_update"):
            continue
        for part in artifact_update.artifact.parts:
            if part.text:
                return part.text
    return None


async def dispatch_call(
    client: Any,
    skill_id: str,
    params: dict[str, Any],
    captured_headers: list[dict[str, str]],
) -> tuple[str | None, str | None]:
    """Send one skill-dispatch request. Returns (artifact_text, echoed_header)."""
    msg = Message(
        message_id=uuid4().hex,
        role=Role.ROLE_USER,
        parts=[Part(text="dispatch")],
        metadata=build_metadata(skill_id, params),
    )
    req = SendMessageRequest(
        message=msg,
        configuration=SendMessageConfiguration(),
    )

    before = len(captured_headers)
    chunks: list[Any] = []
    async for chunk in client.send_message(req):
        chunks.append(chunk)

    new_headers = captured_headers[before:]
    echoed = None
    for h in new_headers:
        if h.get("a2a-extensions"):
            echoed = h["a2a-extensions"]
            break

    return extract_artifact_text(chunks), echoed


async def main() -> int:
    captured_headers: list[dict[str, str]] = []

    async def capture_response(resp: httpx.Response) -> None:
        captured_headers.append({k.lower(): v for k, v in resp.headers.items()})

    # `A2A-Extensions` is set as a default request header on the shared
    # httpx client. The SDK's ClientFactory honours `httpx_client`, so
    # both the agent-card fetch and the JSON-RPC POSTs inherit it.
    async with httpx.AsyncClient(
        timeout=30.0,
        headers={"A2A-Extensions": PROFILE_URI},
        event_hooks={"response": [capture_response]},
    ) as http:
        resolver = A2ACardResolver(httpx_client=http, base_url=BASE_URL)
        card = await resolver.get_agent_card()

        print("=== AgentCard ===")
        print(f"  name        : {card.name}")
        print(f"  version     : {card.version}")
        advertised = [ext.uri for ext in (card.capabilities.extensions or [])]
        print(f"  extensions  : {advertised}")
        if PROFILE_URI not in advertised:
            print(f"ERROR: agent does not advertise {PROFILE_URI}", file=sys.stderr)
            return 2

        factory = ClientFactory(ClientConfig(httpx_client=http))
        client = factory.create(card)

        print()
        print("=== Call 1: echo_loud ===")
        print('  metadata    : a2a.skillId="echo_loud" a2a.skillParams={"text":"hello"}')
        text1, echoed1 = await dispatch_call(
            client, "echo_loud", {"text": "hello"}, captured_headers
        )
        print(f"  artifact    : {text1}")
        print(f"  echoed hdr  : {echoed1}")

        print()
        print("=== Call 2: reverse ===")
        print('  metadata    : a2a.skillId="reverse" a2a.skillParams={"text":"abc"}')
        text2, echoed2 = await dispatch_call(
            client, "reverse", {"text": "abc"}, captured_headers
        )
        print(f"  artifact    : {text2}")
        print(f"  echoed hdr  : {echoed2}")

        # Verification — every assertion is also useful as a non-zero
        # exit signal for the smoke run.
        expected1 = '{"shouted":"HELLO"}'
        expected2 = '{"reversed":"cba"}'
        ok = True
        if text1 != expected1:
            print(f"FAIL: echo_loud artifact mismatch (want {expected1!r})", file=sys.stderr)
            ok = False
        if text2 != expected2:
            print(f"FAIL: reverse artifact mismatch (want {expected2!r})", file=sys.stderr)
            ok = False
        if PROFILE_URI not in (echoed1 or "") and PROFILE_URI not in (echoed2 or ""):
            print("FAIL: server never echoed the A2A-Extensions header", file=sys.stderr)
            ok = False

        print()
        if ok:
            print("=== OK: both artifacts matched and A2A-Extensions echo observed ===")
            return 0
        return 1


if __name__ == "__main__":
    try:
        exit_code = asyncio.run(main())
    except Exception:
        traceback.print_exc()
        sys.exit(1)
    sys.exit(exit_code)
