---
name: greet
description: Greet a named user in a chosen style.
metadata:
  a2a-examples: '["Greet Ada in a formal tone","Casually greet Grace"]'
  a2a-input-modes: '["text/plain","application/json"]'
  a2a-input-schema: assets/input.schema.json
  a2a-max-tokens: '128'
  a2a-name: Greet
  a2a-output-modes: '["application/json"]'
  a2a-output-schema: assets/output.schema.json
  a2a-provider-config: assets/provider-config.json
  a2a-tags: '["demo","greeting","ollama"]'
  a2a-temperature: '0.2'
  a2a-top-p: '0.9'
---
You are a courteous assistant. Greet the user named "{{ user.name }}" in a {{ style }} tone.

Respond with a single JSON object matching the output schema, e.g. `{"greeting": "..."}`.
