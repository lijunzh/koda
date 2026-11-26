# Local models

Koda runs well on a model on your own machine: no API key, no cost per
token, and the conversation never leaves the machine. This page covers
choosing a server and a model, connecting them, and what to expect next to
a hosted model such as Claude. [Providers](./providers.md) has the reference
for each provider.

## One command: `koda --oss`

```bash
brew install llama.cpp      # once
koda --oss
```

`--oss` (as in Codex) runs Koda on a local open-weight model with nothing to
set up. Koda starts llama.cpp's `llama-server` with the model, waits until it
answers, runs your session on it, and stops it when Koda exits. `koda exec
--oss`, `koda resume --oss` and `koda fork --oss` work the same way.

**Which model,** by your machine's memory:

| Machine | Model | Download | What it's for |
|---|---|---|---|
| A Mac with 128 GB or more | **Qwen3.8-Flash-Next** (`unsloth/Qwen3.8-Flash-Next-GGUF:UD-Q4_K_XL`), 262K context, images, at `xhigh` effort | about 105 GB | Real work: 30 of 31 tasks in Koda's evals |
| Anything else, Linux included | **LFM3-1.7B** (`LiquidAI/LFM3-1.7B-GGUF:Q4_K_M`), 64K context | about 1.1 GB | A quick try: it reads, runs and edits, but is too small for real coding, and Koda says so |

Only a Mac gets the big model by default: its GPU shares the machine's
memory, so 128 GB holds Flash-Next. On Linux the GPU's own memory decides
what fits, and a model in system RAM would run on the CPU far too slowly,
so `--oss` starts the quick try there; name a model sized for your GPU.

To run another model, name it in `~/.config/koda/config.toml` (or with `-c`
for one run) as a Hugging Face `<repo>:<quant>`, or as the path of a `.gguf`
file:

```toml
oss_model = "ornith-ai/Ornith-1.5-35B-A3B-GGUF:Q4_K_M"   # ~22 GB; 79 of 93 in Koda's evals
```

**The download** happens once, on the first run, into the Hugging Face cache
(`~/.cache/huggingface/hub/`, or `HF_HUB_CACHE`, or `HF_HOME/hub`), done by
llama.cpp; Koda shows progress while it waits. Before it starts, Koda says
how big the download is and where it goes, and warns when the disk there has
too little room for it (Flash-Next is about 105 GB). A model LM Studio has already downloaded (in `~/.lmstudio/models/`)
is used where it is, with the vision projector beside it, instead of being
downloaded again.

**A busy GPU stops the start (macOS):** before loading a model, Koda checks
that no other model server is running (llama-server, oMLX), that LM Studio
has no model loaded, and that the GPU isn't already busy, as two big models
side by side can run the Mac out of memory. It says what it found; stop that
and try again, or set `oss_check_gpu = false` to start anyway. Joining a
running `koda --oss` server skips the check.

**One server, shared:** a second `koda --oss` on the same model joins the
running server instead of loading another copy, and the last Koda to exit
stops it. A server left behind by a Koda that was killed is stopped at the
next start. Each server listens on `127.0.0.1` with an API key only Koda
knows; its log is `~/.config/koda/logs/llama-server-<model>.log`.

**LM Studio or Ollama instead:** they run as services, so `koda --oss
--local-provider lm-studio` (or `ollama`) connects to the one running, with
the model it has loaded; `oss_provider` in `config.toml` makes that the
default. To run your own `llama-server` and keep it running between
sessions, start Koda without `--oss` and point it at the server (see
[Connect Koda](#connect-koda) below).

With LM Studio, set up the model there, since Koda only connects:

- **Save the model's load settings as its default** in LM Studio: the
  context length (262,144 for Qwen3.8-Flash-Next) and GPU offload at max.
  A load from the app, or one a request starts, then gets them. Otherwise
  LM Studio picks the context length itself, which can be far smaller than
  an agent needs, and Koda, which reads the window from the loaded model,
  would compact all the time.
- **Load the model before starting Koda, or name it.** Koda takes the first
  model the server lists, and LM Studio lists every downloaded model, loaded
  or not, when it loads models on request. `--model qwen3.8-flash-next`
  (the id `lms ls` shows) picks one; LM Studio loads it if it isn't loaded.
- **Set the effort yourself.** `--oss` sets `xhigh` for Qwen3.8-Flash-Next
  only when it starts `llama-server`; with LM Studio, pass `--effort xhigh`.

## What you need

A machine that can hold the model in memory and run it at a usable speed: an
Apple silicon Mac with plenty of unified memory, or a Linux machine with a
GPU. The models below take roughly 20 GB (a 4-bit 27–35B model) and up.

## Choose a server

| Server | Use it when | Koda's provider |
|---|---|---|
| **llama.cpp's `llama-server`** | You want what Koda's evals use, or `koda --oss` to start and stop it for you | `llama-cpp` (`http://localhost:8080/v1`) |
| **LM Studio** | You want a desktop app that keeps the model running and lets you download, load and switch models | `lm-studio` (`http://localhost:1234/v1`) |
| **Ollama** | You already use it | `ollama` |
| **vLLM** | You serve models on a Linux GPU box | `vllm` |

**`llama-server` and LM Studio are now about as fast.** An agent sends the
same long prompt again on every call, with a little more at the end, and both
reuse what they already processed. On the same Qwen3.8-Flash-Next file and
five of Koda's eval tasks (November 26, 2026; LM Studio 0.4.29 with its
llama.cpp runtime 2.83.0), the first token came after 2.2 seconds per 1,000
new prompt tokens on `llama-server` against 2.7 on LM Studio, and both wrote
about 39 tokens a second. A short call waits about half a second longer on LM
Studio. Earlier in November, LM Studio waited 10.6 seconds per 1,000 tokens;
its newer runtime closed most of that gap. `llama-server` is still the one
Koda's own evals use.

LM Studio doesn't say how much of the prompt it reused, so Koda's usage
shows no cached tokens there. That doesn't slow anything down, but
[`turn_token_budget`](./configuration.md#limiting-what-a-turn-spends), which
counts only input not read from the cache, counts every call's whole prompt
on LM Studio and runs out sooner.

## Choose a model

Measured in Koda's own evals ([koda-evals](https://github.com/lijunzh/koda-evals),
Rust and Python coding tasks with hidden tests, on Apple silicon, November
2026). Local models change monthly; treat this as a starting point.

| Model | Size | In Koda's evals |
|---|---|---|
| **Qwen3.8-Flash-Next** | 125B MoE, 6B active (4-bit) | 30 of 31 tasks; the model Koda's local evals use. Takes images with its projector |
| **Ornith-1.5-35B-A3B** | 35B MoE, 3B active | 79 of 93 runs, against Codex's 75 on the same model. Takes images with its projector |
| **Qwen3.8-27B** | 27B dense | Works, but slow at the highest reasoning effort. Takes images with its projector |

A model too small for agent work (a 1–3B model, say) still answers questions
and is useful for checking a setup, but don't expect it to finish coding
tasks.

## Connect Koda

Start the server with a large context window, the model's own chat template,
and every layer on the GPU:

```bash
llama-server -m Qwen3.8-Flash-Next-UD-Q4_K_XL-00001-of-00004.gguf \
  --jinja -c 131072 -ngl 999 --port 8080
```

Then point Koda at it:

```bash
koda --provider llama-cpp
# or any OpenAI-compatible server by address:
koda --base-url http://127.0.0.1:8080/v1
```

Koda lists the server's models, picks the loaded one, and reads the context
window from the server. To make it the default, choose it with `/provider`
in the TUI, or set it in `~/.config/koda/config.toml`:

```toml
model_provider = "llama-cpp"

[model_providers.llama-cpp]
base_url = "http://127.0.0.1:8080/v1"
```

## Give it enough context

Koda's system prompt and tools take about 12,000 tokens before your first
message, and a coding task reads files, runs builds and keeps their
output. **Start the server with at least 16,384 tokens of context** (the
llama-server default of 4,096 fails at once), and with 65,536 or more for real
work. A server that doesn't report its window can be told with
`--max-context-tokens`. When the conversation fills 85% of the window, Koda
[compacts](./context.md) it, which costs a model call; a larger window means
fewer of them.

## Images and PDFs

A local model sees images, and PDF pages as images, only if it takes them,
and Koda asks the server:

- **`llama-server`:** start it with the model's vision projector,
  `--mmproj mmproj-F16.gguf`; `/props` then reports vision.
- **LM Studio:** a vision model (`vlm`).
- **Ollama:** a model with the `vision` capability.

For a server that doesn't say, set `model_input_modalities = ["text",
"image"]` in your config file. A text-only model gets PDFs as text (with
poppler installed) and a note in place of an image. See [File & image
attachments](./attachments.md).

## Reasoning effort

`--effort` (or `model_reasoning_effort`) is passed to the server as
`reasoning_effort`. Whether the model uses it depends on the model and its
chat template. Higher effort is better on hard tasks and much slower
locally: Qwen3.8-27B at its highest effort, under LM Studio's MLX runtime,
timed out on half of a set of tasks in Koda's evals. Start at the model's default and raise it for a task
that needs it.

## What to expect

- **Slower turns.** A long agent turn on a local model takes minutes where
  Claude takes seconds; the evals above ran one to five minutes a task.
- **Guards for local models.** A reply that starts repeating itself is
  stopped and the model asked to continue, and a reply is capped at 32,768
  tokens unless you set `model_max_output_tokens`. Local servers set no limit
  of their own, and a local model can loop inside one reply.
- **Keep `AGENTS.md` short.** Every token of instructions is read on every
  call, and a smaller model follows a long file worse; see
  [Memory](./memory.md).
- **What still leaves the machine:** `WebFetch` and `WebSearch` reach the
  web when the model calls them, MCP servers reach whatever they connect to,
  and Koda asks crates.io for new versions. See [Privacy &
  data](./privacy.md).
