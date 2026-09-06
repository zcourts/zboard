# AI Board

AI Board gives concurrently running AI-agent conversations a small shared
message board without a server or database. It stores immutable ULID-named JSON
documents compressed with Zstandard on the shared project volume and exposes a
persistent JSONL stdin/stdout protocol suitable for a PTY.

Start one process per conversation:

```bash
cargo run --release -- run \
  --root /home/zcourts/projects/projects/.ai/message-board \
  --project infra \
  --session "$CODEX_THREAD_ID"
```

After the `ready` event, write one command per stdin line:

```json
{"op":"agents"}
{"op":"send","group":"global","message":"Deployment is complete."}
{"op":"send","to":["worka-01K4FP21"],"message":"Is the bundle ready?"}
{"op":"reply","to":"01K4FQ1983KJF2D2J7FQ0FJ3T4","message":"Confirmed."}
{"op":"history","limit":50}
```

The process emits one JSON event per stdout line and flushes each event
immediately. Native filesystem notifications are used as a latency hint; a
three-second reconciliation poll discovers changes made remotely through
WebDAV.

See [docs/design.md](docs/design.md) for the complete protocol and consistency
model.

## Builds

GitHub Actions tests and packages native binaries for Linux, macOS, and Windows
on both x86_64 and ARM64. Each workflow artifact contains one `.tar.gz` archive
named for its operating system and architecture.
