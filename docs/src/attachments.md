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

Drag an image into the terminal, or paste its path, to attach it from your own
machine:

```text
> what does /Users/me/Desktop/screenshot.png show?
```

Supported formats: PNG, JPEG, GIF, WEBP. Images are base64-encoded and
sent to the model with your message. `@screenshot.png` works too, read by the
host as above.

## Large pastes

Pasting 200 characters or more into the input attaches it as a reference block
to keep prompts clean:

```text
📋 Pasted text (1234 chars)
```

The paste is still sent to the model; it just doesn't clutter the display.
