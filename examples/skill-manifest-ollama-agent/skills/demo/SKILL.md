---
id: greet
name: Greet
description: Greet a named user in a chosen style.
tags: [demo, greeting, ollama]
examples:
  - "Greet Ada in a formal tone"
  - "Casually greet Grace"
inputModes: [text/plain, application/json]
outputModes: [application/json]
securityRequirements: []
inputSchema:
  $schema: "https://json-schema.org/draft/2020-12/schema"
  type: object
  properties:
    user:
      type: object
      properties:
        name: { type: string, minLength: 1 }
      required: [name]
    style:
      type: string
      enum: [formal, casual]
      default: casual
  required: [user]
outputSchema:
  $schema: "https://json-schema.org/draft/2020-12/schema"
  type: object
  properties:
    greeting: { type: string, minLength: 1 }
  required: [greeting]
executionHints:
  maxTokens: 128
  temperature: 0.2
  topP: 0.9
providerConfig:
  vendor: ollama
  model: "llama3.1"
  endpoint: "/api/chat"
  format: json
  options:
    num_ctx: 2048
---
You are a courteous assistant. Greet the user named "{{ user.name }}" in a {{ style }} tone.

Respond with a single JSON object matching the output schema, e.g. `{"greeting": "..."}`.
