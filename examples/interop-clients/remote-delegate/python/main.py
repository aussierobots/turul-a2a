"""Python A2A client for the `remote-delegate-agent`.

This client is third-party from A2A's perspective — it does not import
any `turul-*` code. It talks to the delegate, which then talks to the
upstream `skill-manifest-ollama-agent`. **Two A2A hops** total:

    client (this) ──► remote-delegate-agent ──► skill-manifest-ollama-agent
        ^                                                       │
        └───────────── artifact body propagates back ◄──────────┘

The client only knows about the delegate; the upstream is invisible
from the wire. The test that this *is* a chain is the artifact body,
which carries the upstream's offline-stub marker.
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

BASE_URL = os.environ.get("A2A_BASE_URL", "http://localhost:3016")


def extract_artifact_text(chunks: list[Any]) -> str | None:
    """Walk the response chunks and return the first artifact text part.

    Buffered mode (the delegate advertises `streaming: false`) yields a
    single `StreamResponse` whose `task` payload carries the completed
    Task with all artifacts. Streaming mode would yield separate
    `artifact_update` chunks; we handle both shapes so the same client
    works if the delegate ever flips streaming on.
    """
    for chunk in chunks:
        if chunk.HasField("task"):
            for artifact in chunk.task.artifacts:
                for part in artifact.parts:
                    if part.text:
                        return part.text
        if chunk.HasField("artifact_update"):
            for part in chunk.artifact_update.artifact.parts:
                if part.text:
                    return part.text
    return None


async def main() -> int:
    payload = '{"user":{"name":"Ada"},"style":"formal"}'

    async with httpx.AsyncClient(timeout=30.0) as http:
        resolver = A2ACardResolver(httpx_client=http, base_url=BASE_URL)
        card = await resolver.get_agent_card()

        print("=== Delegate AgentCard (what THIS client sees) ===")
        print(f"  name        : {card.name}")
        print(f"  version     : {card.version}")
        print(f"  skills      : {[s.id for s in (card.skills or [])]}")
        print()

        factory = ClientFactory(ClientConfig(httpx_client=http))
        client = factory.create(card)

        msg = Message(
            message_id=uuid4().hex,
            role=Role.ROLE_USER,
            parts=[Part(text=payload)],
        )
        req = SendMessageRequest(
            message=msg,
            configuration=SendMessageConfiguration(),
        )

        print(f"=== Send (chain: client → delegate → upstream) ===")
        print(f"  payload     : {payload}")

        chunks: list[Any] = []
        async for chunk in client.send_message(req):
            chunks.append(chunk)

        artifact = extract_artifact_text(chunks)
        print(f"  artifact    : {artifact}")

        # The delegate re-emits the upstream's artifact body verbatim.
        # The upstream's offline stub marker proves the chain
        # round-tripped through both agents.
        ok = True
        if artifact is None:
            print("FAIL: delegate returned no artifact text", file=sys.stderr)
            ok = False
        else:
            if "offline stub" not in artifact:
                print(
                    "FAIL: artifact missing 'offline stub' marker — "
                    "chain didn't reach the upstream's offline-mode path",
                    file=sys.stderr,
                )
                ok = False
            if "Ada" not in artifact:
                print(
                    "FAIL: artifact missing 'Ada' — caller payload not forwarded",
                    file=sys.stderr,
                )
                ok = False

        print()
        if ok:
            print("=== OK: two-hop chain returned the upstream's artifact ===")
            return 0
        return 1


if __name__ == "__main__":
    try:
        exit_code = asyncio.run(main())
    except Exception:
        traceback.print_exc()
        sys.exit(1)
    sys.exit(exit_code)
