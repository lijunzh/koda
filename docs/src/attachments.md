# File & image attachments

## Attaching files

Type `@` anywhere in your message to attach files as context:

```text
> explain @src/auth.rs
> compare @old_impl.rs and @new_impl.rs
> what's wrong with @error.log
```

As you type after `@`, a fuzzy file picker appears. Press `Tab` to cycle
through matches or select with `Enter`. The file's full contents are
injected into the message before it's sent to the model.

## Images

For vision-capable models (Claude, Gemini, GPT-4o), attach images directly:

```text
> what does @screenshot.png show?
> explain the architecture in @diagram.png
```

Supported formats: PNG, JPEG, GIF, WEBP. Images are base64-encoded and
sent inline to the model API.

Non-image files (SVG, PDF, etc.) attached via `@` are sent as text content,
not as vision input.

## Large pastes

Pasting more than ~500 characters into the input is automatically wrapped
in a reference block to keep prompts clean:

```text
> [pasted 1,234 chars — attached as reference]
> what's the bug in this code?
```

The paste is still sent to the model; it just doesn't clutter the display.
