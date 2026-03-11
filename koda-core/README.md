# koda-core

Engine library for the [Koda](https://github.com/lijunzh/koda) AI coding agent.

Pure logic with zero terminal dependencies — communicates exclusively through
`EngineEvent` (output) and `EngineCommand` (input) enums over async channels.

## What's inside

- **LLM providers** — 14 providers (Anthropic, OpenAI, Gemini, Groq, Ollama, LM Studio, etc.)
- **Tool system** — 20+ built-in tools (file ops, shell, search, memory, agents)
- **Per-tool approval** — three modes (Auto/Strict/Safe) with effect-based safety classification
- **Inference loop** — streaming tool-use loop with parallel execution
- **SQLite persistence** — sessions, messages, compaction
- **MCP client** — connects to external MCP servers for extensibility

**Rust edition:** 2024

## Usage

koda-core is a channel-driven engine. Create async channels, spawn the
inference loop, and drive the engine through `EngineCommand`/`EngineEvent` pairs:

```rust
use koda_core::engine::{EngineCommand, EngineEvent};
use tokio::sync::mpsc;

// The engine communicates exclusively through async channels.
// EngineEvents flow out (streaming text, tool calls, approvals).
// EngineCommands flow in (approval responses, cancellation).
let (cmd_tx, mut cmd_rx) = mpsc::channel::<EngineCommand>(64);
let (evt_tx, mut evt_rx) = mpsc::channel::<EngineEvent>(64);

// Spawn the inference loop, then select over evt_rx for streaming
// output and cmd_tx to send approval decisions back.
// See koda-cli for a complete implementation.
```

See [DESIGN.md](https://github.com/lijunzh/koda/blob/main/DESIGN.md) for
architectural decisions.

## License

MIT
