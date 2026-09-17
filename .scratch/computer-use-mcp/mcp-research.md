# Rust stdio MCP research for computer use

Date: 2026-09-17

## Recommendation

Use the official [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk) crate, pinned to version 3.4.0 in `Cargo.lock`, with only its server, macros/schema, base64, and stdio transport features. As checked on 2026-09-17, both the official repository's latest release and docs.rs identify 3.4.0; this is not inferred from the repository's main branch ([official releases](https://github.com/modelcontextprotocol/rust-sdk/releases); [published feature documentation](https://docs.rs/crate/rmcp/latest/features)). Avoid a hand-written JSON-RPC/MCP layer and avoid an HTTP stack: the official SDK already supplies lifecycle/version negotiation, tool routing, schemas, typed content blocks, and stdio. Its documented server path is `service.serve(stdio()).await?`, and stdio is the standard local subprocess transport ([official Rust SDK](https://github.com/modelcontextprotocol/rust-sdk); [rmcp transport docs](https://docs.rs/rmcp/latest/rmcp/transport/index.html)).

The smallest practical public surface is two tools:

1. `computer_displays`: enumerate displays and return their IDs, bounds, scaling/rotation metadata, and an opaque session-scoped `display_generation`.
2. `computer_action`: execute exactly one discriminated action and then capture exactly one screenshot of the selected display. This tool accepts the selected `display_id`, the `display_generation`, and action-specific arguments. Pointer actions additionally require a `frame_id` from a previous screenshot result, with coordinates expressed in that frame's pixels. A screenshot-only action needs no prior frame and bootstraps interaction.

Keep the action set in one tagged input enum rather than creating a tool for every gesture. It keeps discovery and policy handling small while preserving an explicit “one call, one action” rule. Do not support action arrays or scripts in v1. The server must validate the conditional fields after deserialization if the generated schema/client combination does not enforce the tagged union cleanly.

## Why this SDK and transport fit current Codex

Local inspection, without reading user configuration, found `codex-cli 0.153.3`. Its built-in help reports:

```text
codex mcp add <NAME> (--url <URL> | -- <COMMAND>...)
```

It also exposes `--env KEY=VALUE` for stdio servers. Therefore the binary can be registered directly as a child command; no wrapper or local HTTP listener is needed. This is a local CLI observation from `codex --version`, `codex mcp --help`, and `codex mcp add --help` on 2026-09-17.

At the protocol level, stdio means the client launches the server, JSON-RPC travels over stdin/stdout, messages are newline-delimited, logs may go to stderr, and stdout must contain no non-MCP output ([MCP transport specification](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)). Route tracing/logging to stderr.

Let `rmcp` perform ordinary initialization; do not override lifecycle negotiation for this tools-only server. MCP requires initialization to be the first interaction, followed by version and capability negotiation and the client's `notifications/initialized` notification ([MCP lifecycle specification](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle)). The official SDK documents automatic negotiation and a tools-only `#[tool_router(server_handler)]` server. Declare only the tools capability. Do not add resources, prompts, logging capability, tasks, elicitation, or server-to-client requests for v1.

Compatibility acceptance should be proved against the actual Codex client, not inferred from version numbers:

- start via the same command shape that `codex mcp add ... -- <binary>` uses;
- observe successful initialize/initialized and `tools/list`;
- call `computer_displays` and one harmless screenshot-only `computer_action`;
- confirm Codex receives both the image content block and structured/text result;
- confirm a deliberately stale `display_generation` is caller-visible and performs no input action.

## Result contract

MCP tool results can contain multiple content blocks, including an image whose `data` is base64 and whose `mimeType` is such as `image/png`. A result can also contain `structuredContent`; when an output schema is declared, servers must conform and clients should validate it. For backward compatibility, the specification says structured content should also be serialized into a text content block ([MCP tools specification](https://modelcontextprotocol.io/specification/2025-11-25/server/tools)). `rmcp::model::CallToolResult` exposes `content`, `structured_content`, and `is_error`, and the SDK supplies `ContentBlock::image(base64, "image/png")` ([CallToolResult docs](https://docs.rs/rmcp/latest/rmcp/model/struct.CallToolResult.html); [official Rust SDK example](https://github.com/modelcontextprotocol/rust-sdk#tool-result-content-types)).

For a normal action, return:

- `content[0]`: short text containing the same JSON as `structuredContent` for compatibility;
- `content[1]`: the PNG screenshot as an MCP image block;
- `structuredContent`: the machine-readable result below;
- `isError: false` (or omit it if the SDK's success constructor does so).

Recommended stable object shape:

```json
{
  "outcome": "ok",
  "effect": "completed",
  "action": { "kind": "click", "details": {} },
  "display": {
    "id": "stable-session-id",
    "generation": "opaque-session-generation",
    "logical_bounds": { "x": 0, "y": 0, "width": 1920, "height": 1080 },
    "scale": 1.0,
    "rotation_degrees": 0
  },
  "frame": {
    "id": "opaque-frame-id",
    "width_px": 1920,
    "height_px": 1080,
    "mime_type": "image/png",
    "coordinate_space": "frame_pixels"
  },
  "error": null
}
```

Clients should send `frame_id` plus frame-pixel coordinates back for pointer actions. The server stores the transform associated with that frame and performs all scale, origin, and rotation mapping internally after confirming its display generation is still current. Do not expose a transform matrix in v1: the client neither needs nor should be asked to calculate native desktop coordinates. Add transform metadata to the wire contract only if a real external consumer later needs it.

`display_generation` must be more than a topology fingerprint. A display can change and later return to the same fingerprint, so fingerprint equality alone cannot prove that an old frame is safe. At server start, create a fresh random session epoch. Maintain a monotonically increasing generation counter, invalidated by platform display-change events; pair it with a digest over all facts that affect targeting or mapping: active display set, stable platform identifiers, logical/native bounds, scale, rotation, and primary status. A generation token should bind the session epoch, counter, and digest. A server restart therefore invalidates every old frame even if the topology is identical.

Before every action, drain/process display-change events, re-enumerate, recompute the fingerprint, and compare the submitted generation and referenced frame to current session state before emitting any input. Either an event-driven counter change or a fingerprint mismatch rejects the call. If stale, return a tool execution error with `effect: "none"`, an error code such as `STALE_DISPLAY_CONFIGURATION`, and the newly observed display summary/generation. Keep the event subscription plus synchronous preflight fingerprint check: each catches gaps the other can miss. This is an application-level safety rule; MCP itself does not define display identity or coordinate semantics.

## Partial completion and errors

MCP distinguishes JSON-RPC/protocol errors from tool execution errors. Unknown tools, malformed request envelopes, and an unusable server belong in protocol errors. Validation, API, and business failures belong in a tool result with `isError: true`, so the model can see actionable feedback ([MCP tools error handling](https://modelcontextprotocol.io/specification/2025-11-25/server/tools#error-handling)). The `rmcp` guidance makes the same distinction: return `Ok(CallToolResult::error(...))` for caller-visible execution failures and `Err(McpError)` only when the request cannot be routed or processed as a protocol request ([official Rust SDK](https://github.com/modelcontextprotocol/rust-sdk#error-handling)).

Use these semantics:

| Situation | Result | `effect` | Retry guidance |
|---|---|---:|---|
| Stale generation, invalid target, or invalid coordinates rejected before injection | Tool result with `isError: true` | `none` | Correct/refresh, then retry |
| Injection API confirms no effect | Tool result with `isError: true` | `none` | Retry only if the backend guarantee is reliable |
| Injection reports a prefix/partial effect | Caller-visible result, `outcome: "partial"` | `partial` | Do **not** repeat blindly; inspect/recover |
| Injection fails after submission and effect cannot be determined | Caller-visible result, `outcome: "partial"` | `unknown` | Do **not** repeat blindly; inspect/recover |
| Action and screenshot succeed | Success result, `outcome: "ok"` | `completed` | Continue |
| Action completed but post-action screenshot failed | Success result, `outcome: "partial"`, no image block, structured `error.code: "SCREENSHOT_FAILED_AFTER_ACTION"` | `completed` | Do **not** repeat the action; call a screenshot-only action |
| Unknown tool or malformed MCP request | JSON-RPC/MCP error | absent | Fix protocol request |

Use an `effect` enum of `none | completed | partial | unknown`; a boolean cannot represent real input-injection outcomes. Multi-event actions such as typing may stop after a prefix, and even nominally atomic backend calls can fail after submission without proving whether the compositor/application consumed them. Never report `none` unless validation failed before injection or the backend explicitly guarantees no effect. Include any known progress, such as characters or events accepted, without claiming that acceptance proves application-level handling.

All `partial` and `unknown` effects, and the completed-action/screenshot-failure case, should be successful protocol responses with `outcome: "partial"` rather than generic retryable errors. A side effect may already have occurred, and a generic client/model may otherwise duplicate a click, keypress, or typed text. Return a prominent text block saying not to replay the action blindly, plus the structured result. This is a design inference from MCP's error categories rather than a protocol-mandated convention. A screenshot-only action gives the caller a safe recovery path.

No implementation can absolutely guarantee that every completed side effect has a screenshot or that a failed injection had no effect. The contract should therefore promise an attempted post-action screenshot and explicit effect state, while making stale-configuration rejection a precondition checked before side effects.

## Proposal review note

The initial proposal used `action_performed: bool`, a topology digest as the revision, and an exposed frame-to-desktop matrix. Review found all three too weak or unnecessary. The revised contract uses a four-state effect enum to prevent unsafe retries after ambiguous injection, binds frames to a server-session epoch plus event-invalidated generation and independently checked fingerprint, and keeps coordinate transforms inside the server. These changes add only state required for correctness; they do not expand the two-tool surface.

## Minimal dependency boundary

Use `rmcp` rather than a smaller unofficial facade because lifecycle, schema, and mixed image/structured results are correctness-sensitive interoperability code. Beyond `rmcp`, prefer dependencies already required by it or the selected platform backend. A practical core is `tokio` for the SDK runtime plus `serde`/`serde_json` and `schemars` for typed inputs and output schema; enable only the `rmcp` features required by server macros and stdio. The official crate's feature page identifies stdio under `transport-io` ([rmcp feature documentation](https://docs.rs/crate/rmcp/latest/features)).

Keep display capture and input injection behind concrete platform functions, not a speculative public trait/factory unless a second backend is actually in scope. Keep PNG as the sole screenshot format in v1. Do not add a resource store, HTTP transport, image URL indirection, task extension, action batching, or automatic retries. These omissions follow the ponytail dependency rule: use the official protocol implementation and native platform facilities, and own only the mapping and action logic specific to this product.

## Open verification points

- Confirm which protocol version Codex 0.153.3 negotiates in an end-to-end smoke test. The server should not hard-code a version merely from client release numbers.
- Verify that this Codex build surfaces `structuredContent` alongside an image. The duplicated compact JSON text block is the compatibility fallback required by the MCP recommendation.
- Decide the first platform backend before fixing the exact native monitor identifier and capture APIs. The wire contract above remains platform-neutral.
- Set screenshot size/encoding limits after measuring actual multi-monitor payloads; MCP image blocks carry inline base64, so payload size grows beyond raw PNG bytes.
