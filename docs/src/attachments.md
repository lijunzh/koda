# File & image attachments

## Mentioning files

Type `@` anywhere in your message to mention a file or directory:

```text
> explain @src/auth.rs
> compare @old_impl.rs and @new_impl.rs
> what's in @src/components?
```

As you type after `@`, a fuzzy file picker appears. Press `Tab` to cycle
through matches or select with `Enter`. Your message keeps the `@path` as you
typed it, and Koda's host reads the file and sends the model its contents
after your text, as Claude Code does:

- **A file** is read the way the `Read` tool reads one: numbered lines, up to
  Read's limit (larger with a larger context window), with a note saying how
  to read the rest.
- **A directory** gives its listing, as the `List` tool does, not its files.
- **An image** (PNG, JPEG, GIF, WebP) is sent as an image, for models that
  take them.
- **A binary file** (a PDF, an archive, a program) isn't read: the model gets
  the path and a note saying what it is, and can inspect it with Bash.
- **A path that doesn't exist** comes back with that, so the model knows.

Paths are relative to the project, or absolute. A mention can name any file
the `Read` tool can, which is any path except Koda's own secret folders; Koda
reads it on the machine its host runs on, so a client of
[`koda serve`](./serve.md) on another machine names the host's files.

To send only the paths, for the model to read itself as Codex does, set
`file_mentions = false` in `config.toml`.

## Images

Three ways to attach an image from your own machine, as in Claude Code:

- **Paste it:** copy an image (a screenshot, say) and press `Ctrl+V`, or
  `Alt+V` where the terminal takes `Ctrl+V` for itself. In iTerm2 `Cmd+V`
  works too. Koda sends it as a PNG, scaled down to 1,568 pixels on its long
  edge if it is larger, the most Anthropic recommends.
- **Drag it** onto the terminal: the file becomes a marker.
- **Type its path:** `what does /Users/me/Desktop/screenshot.png show?`

A pasted or dropped image shows in the input box as `[Image #1]`,
`[Image #2]` and so on, where you put it, so you can refer to it: "compare
[Image #1] with [Image #2]". Delete a marker and its image stays behind.
The marker stays in your message as typed.

Over SSH, the clipboard and your files are on your own machine, not the one
Koda runs on, so pasting and dragging don't reach Koda there; copy the image
to that machine and give its path.

Supported formats: PNG, JPEG, GIF, WEBP. Images are base64-encoded and
sent to the model with your message. `@screenshot.png` works too, read by the
host as above.

**An image stays in the conversation,** as in Claude Code: Koda stores it
with your message, so the model still sees it after its first tool call, in
later turns, and when you resume the session. Each image counts about 1,600
tokens toward the context window, and compaction summarizes it away like
any other early message. A request carries at most 100 images, Anthropic's
limit; older ones beyond that are sent as a short note.

**Which models get images:** a local server says whether its model takes
images, and Koda asks it when a session starts or the model changes, as it
asks for the context window:

- **LM Studio:** a vision model (`"type": "vlm"`, or `vision` among its
  capabilities).
- **llama.cpp's `llama-server`:** started with a vision projector
  (`--mmproj`), which `/props` reports as `"modalities": {"vision": true}`.
- **Ollama:** a model that lists `vision` among its capabilities.

Otherwise Koda's table decides:

| Provider | Model | Takes |
|---|---|---|
| Anthropic | any | text, images, PDFs |
| Gemini, OpenAI | any | text, images |
| any other | a Claude or GPT model (as through OpenRouter) | text, images |
| any other | anything else, local models included | text |

A model that doesn't take images gets a note where each image was, and the
stored image comes back if you switch to one that does. If a server doesn't
say, or says wrong, set what the model takes in `config.toml`, which wins
over both:

```toml
model_input_modalities = ["text", "image"]
```

## Large pastes

Pasting 200 characters or more into the input attaches it as a reference block
to keep prompts clean:

```text
📋 Pasted text (1234 chars)
```

The paste is still sent to the model; it just doesn't clutter the display.
