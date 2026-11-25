# How Koda works

Koda is a loop around a model. The model decides what to do; Koda gives it
tools, asks you when an action needs your approval, keeps each command inside
a sandbox, and keeps track of the conversation. This page is the map; each
part links to its own chapter.

## The loop

When you send a message, Koda:

1. **Builds the request:** its system prompt, your project's instructions
   (`AGENTS.md`), the conversation so far, and the definitions of the tools
   the model may call.
2. **Streams the model's reply** to your screen as it arrives.
3. **Runs the tools the model asks for.** Each call is checked against your
   [trust mode](./approval.md) first: run it, ask you, or refuse. Calls that
   can't interfere with each other, such as several reads, run in parallel.
4. **Sends the results back** and asks the model again.

It repeats until the model replies without calling a tool, which ends the
turn. Koda doesn't plan or decide anything itself: the model does, and the
loop carries out its calls. A guard stops a turn whose replies keep repeating
themselves, and `Esc` stops a turn at any point.

## Tools

The model works through [tools](./tools.md):

| What | Tools |
|---|---|
| Read and find | `Read` (text, images, PDFs, notebooks); search with `grep` and `find` through `Bash`, as Claude Code does ([details](./tools.md#searching-through-the-shell)) |
| Change files | `Edit`, `Write` |
| Run commands | `Bash`, and `Cargo` for Rust projects |
| The web | `WebFetch`, `WebSearch` |
| Delegate | `InvokeAgent` (a [sub-agent](./agents.md) with its own context), `ListAgents` |
| Remember | `MemoryRead`, `MemoryWrite`, `RecallContext` |
| Organise | `TodoWrite`, `AskUser`, and [skills](./skills.md) (`ListSkills`, `ActivateSkill`) |

[MCP servers](./mcp.md) add their own tools beside these.

## Approval and the sandbox

Koda classifies every tool call by its effect: **read-only**, a **local
change** (an edit, a build), a **remote action**, or **destructive** (`rm
-rf`, `git push --force`). Your trust mode decides what happens to each:

- **Auto** (the default) runs reads and local changes in the project, and
  asks before anything destructive or outside the project.
- **Safe** asks before every change.
- **Plan** only looks, then proposes a plan for you to approve.

Under all three, every command runs in a [kernel sandbox](./sandbox.md):
Seatbelt on macOS, bubblewrap on Linux. Writes stay in the project and
temporary folders, credential folders are blocked, and the network goes
through Koda's proxy. The approval decides whether a command runs; the
sandbox limits what it can do when it does. When the sandbox blocks something
you need, such as `git push` over SSH, the model can ask to run that one
command outside it, and you approve it.

## Context

A model sees a fixed amount of text at once: its context window. The status
bar shows how much is left. Everything in the request counts: the system
prompt and tools (about 12,000 tokens), your instructions, the conversation,
tool results, images and PDFs.

When the conversation fills 85% of the window, Koda
[compacts](./context.md) it: the model summarises the older part, your own
messages are kept word for word, and the newest part stays as it was. A
[sub-agent](./agents.md) helps here too: it does a piece of work in its own
context and returns only its findings.

## What Koda reads into every request

- **Your project's instructions:** `AGENTS.md` (or `CLAUDE.md` if there is
  none) from the git root down to the folder you're in, after your own in
  `~/.config/koda/`. Keep them short; see [Memory](./memory.md).
- **Koda's notes,** which the model writes with `MemoryWrite` when you ask it
  to remember something.
- **The skills available,** by name and description; the model loads one
  when it needs it.

## Sessions

Every conversation is a [session](./sessions.md), saved in Koda's database on
your machine as it goes. `koda resume` picks one up again, and `koda fork`
copies one to try something different. A session keeps its model, mode and
history; `/new` starts a fresh one.

## Clients and the host

The TUI, `koda exec` and `koda serve` are all clients of one **client
protocol**. Behind them a host runs the sessions: it loads settings, starts
MCP servers, applies the rules for each trust mode, and drives the engine
that runs the loop above. So a run behaves the same however you start it, and
another program can drive Koda through [`koda serve --stdio`](./serve.md).

## Where things are

| What | Where |
|---|---|
| Settings | `~/.config/koda/config.toml` ([Configuration](./configuration.md)) |
| Sessions, API keys, logs | `~/.config/koda/db/` |
| Your instructions and Koda's notes | `~/.config/koda/AGENTS.md`, `~/.config/koda/memories/` |
| Your agents and skills | `~/.config/koda/agents/`, `~/.config/koda/skills/` |
| A project's agents and skills | `.koda/agents/`, `.koda/skills/` |
