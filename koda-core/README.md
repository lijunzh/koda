# koda-core

Engine library for the [Koda](https://github.com/lijunzh/koda) AI coding agent.

Pure logic with zero terminal dependencies — communicates exclusively through
`EngineEvent` (output) and `EngineCommand` (input) enums over async channels.

## What's inside

- **LLM providers** — 14 providers (Anthropic, OpenAI, Gemini, Groq, Ollama, LM Studio, etc.)
- **Tool system** — 20+ built-in tools (file ops, shell, search, memory, agents)
- **Phase-gated approval** — six-phase state machine gates tool permissions
- **Inference loop** — streaming tool-use loop with parallel execution
- **SQLite persistence** — sessions, messages, compaction, phase flow log
- **MCP client** — connects to external MCP servers for extensibility

## Usage

```rust
use koda_core::{agent::KodaAgent, db::Database, inference::inference_loop};

// koda-core is a library — see koda-cli for the full CLI application.
// The engine communicates through EngineEvent/EngineCommand channels.
```

See [DESIGN.md](../DESIGN.md) for architectural decisions.

## License

MIT
