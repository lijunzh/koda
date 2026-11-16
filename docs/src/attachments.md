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
- **A Jupyter notebook** gives its cells and their outputs, as Read shows
  one ([Notebooks](#notebooks)); output images become a note.
- **A directory** gives its listing, as the `List` tool does, not its files.
- **An image** (PNG, JPEG, GIF, WebP) is sent as an image, for models that
  take them.
- **A PDF** of up to 10 pages is sent whole, for models that take PDFs; a
  longer one sends its path ([PDFs](#pdfs)).
- **A binary file** (an archive, a program) isn't read: the model gets
  the path and a note saying what it is, and can inspect it with Bash.
- **A path that doesn't exist** comes back with that, so the model knows.

Paths are relative to the project, or absolute. A mention can name any file
the `Read` tool can, which is any path except Koda's own secret folders; Koda
reads it on the machine its host runs on, so a client of
[`koda serve`](./serve.md) on another machine names the host's files.

To send only the paths, for the model to read itself as Codex does, set
`file_mentions = false` in `config.toml`.

## Images

Four ways to attach an image from your own machine, as in Claude Code and
Codex:

- **Paste it:** copy an image (a screenshot, say) and press `Ctrl+V`, or
  `Alt+V` where the terminal takes `Ctrl+V` for itself. In iTerm2 `Cmd+V`
  works too. Koda sends it as a PNG, scaled down to 1,568 pixels on its long
  edge if it is larger, the most Anthropic recommends.
- **Drag it** onto the terminal: the file becomes a marker.
- **Type its path:** `what does /Users/me/Desktop/screenshot.png show?`
- **On the command line,** as Codex's `-i`: `koda -i shot.png "what's wrong
  here?"`, `koda exec -i a.png,b.png "compare them"`. Repeatable, or
  comma-separated. A path that isn't an image, or can't be read, stops the
  start. In the TUI the images go with the prompt as `[Image #N]` markers.

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

**The model can read images too:** `Read` on a PNG, JPEG, GIF or WebP file
returns the image itself, as Claude Code's Read does, scaled and recompressed
like a pasted one; a model that doesn't take images gets told so instead. A
tool's images go inside its result for Anthropic and for llama.cpp's
`llama-server`, which accept them there; other servers (LM Studio, for one,
rejects them) get them in a message right after the tool results, as pi and
qwen-code send them.

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

## Notebooks

`Read` on a Jupyter notebook (`.ipynb`) returns its cells rather than its
JSON, as Claude Code's Read does: each cell's type, ID and source, then its
outputs, meaning printed text, results and error tracebacks (without their
colour codes). An output image (a plot, say) comes back as an image for a
model that takes images, scaled like any other, and as a one-line note for
one that doesn't. An `@` mention of a notebook sends the same cells.

Claude Code's limits apply: cells that come to more than 256 KB, or more
than one tool result holds, are an error that suggests reading part of the
notebook with `jq`, and a notebook file over 100 MB is refused. To see the
JSON itself, as `Edit` matches it, give `Read` a `start_line` or
`num_lines`.

## PDFs

`Read` on a PDF returns it in the richest form the model takes, as Claude
Code's Read does, with qwen-code's text fallback for text-only models:

| Model | A PDF of up to 10 pages | With `pages` ("3", "1-5"; at most 20) |
|---|---|---|
| Takes PDFs (Claude) | the PDF itself | the pages as images |
| Takes images but not PDFs (a vision model on `llama-server`) | its pages as images | the pages as images |
| Text only | its pages' text | the pages' text |

A longer PDF read without `pages` gets its page count and a request to read
it in ranges. Page images are drawn at most 1,568 pixels on the long edge;
text comes under a `--- page N ---` line per page. A scan with no text
layer can't be read by a text-only model.

**PDFs need poppler,** as Claude Code's page reads do: Koda runs its
`pdfinfo`, `pdftoppm` and `pdftotext`. Install it with `brew install
poppler` on macOS or `apt-get install poppler-utils` on Debian and Ubuntu.
Without it, a PDF still reaches a model that takes PDFs whole, and anything
else says how to install it.

**A PDF stays in the conversation** like an image, counted as 25,000 tokens
(10 pages at about 2,500) until the API reports the real figure. A model
that doesn't take PDFs gets a note in its place, and a request carries at
most 10 PDFs, which keeps it within Anthropic's 100 pages. An `@` mention of
a PDF of up to 10 pages sends it whole; a longer one sends its path, with
how to read it in ranges.

## Large pastes

A paste of more than 800 characters or more than three lines goes in the
input box as a marker where the cursor is, as Claude Code does, so the box
stays usable:

```text
> why does this fail? [Pasted text #1 +120 lines]
```

The full text is sent where the marker is, tagged as pasted. Koda tells the
model that pasted text came from somewhere else and can carry instructions
you didn't write, to follow only as far as your own words ask. More than
40,000 characters of one paste are cut, with a note; for a whole file or a
long log, give its path or `@` it instead.

- **Delete the marker** and the paste stays behind.
- **Up, Down and `Ctrl+R`** bring a prompt back with its pastes, so sending
  it again sends them; `Ctrl+R` also finds a prompt by a word inside a paste.
  Koda keeps them with your input history.
- **A paste during a turn** works the same way, sent with that message.
- A shorter paste goes in as typed text.
