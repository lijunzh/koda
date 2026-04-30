# Troubleshooting

## Koda crashed — where do I find the diagnostic info?

When koda hits an unrecoverable error and exits, it leaves a forensic
record in:

```
~/.config/koda/logs/panic.log
```

Each crash appends a delimited block:

```
======================================================================
[2026-04-29T20:55:32Z] PANIC
version:    koda 0.2.23
location:   koda-core/src/foo.rs:42:8
thread:     <unnamed>
message:    assertion failed: x > 0
backtrace:
  0: ...
  1: ...
======================================================================
```

The most recent panic is always at the **bottom** of the file.

### Getting a useful backtrace

By default, the backtrace section will say:

```
backtrace:
  (disabled — re-run with RUST_BACKTRACE=1 to capture)
```

To capture a full backtrace, re-run koda with:

```sh
RUST_BACKTRACE=1 koda
```

Or for symbol resolution at every frame (slower startup, more verbose):

```sh
RUST_BACKTRACE=full koda
```

### What to share when reporting a crash

When filing an issue at <https://github.com/lijunzh/koda/issues>, please
include:

1. The most recent panic block from `~/.config/koda/logs/panic.log`
   (with `RUST_BACKTRACE=1` if possible)
2. The contents of `~/.config/koda/logs/latest` (the per-process log
   for the crashed run — symlinks to the most recent `koda-<pid>.log`)
3. Your provider + model (e.g. `gemini gemini-2.5-pro`,
   `anthropic claude-sonnet-4-5`)

### Log file rotation

Koda caps `panic.log` at 5 MB. When it exceeds that, it's rotated:

- `panic.log.3` — dropped (oldest)
- `panic.log.2` → `panic.log.3`
- `panic.log.1` → `panic.log.2`
- `panic.log`   → `panic.log.1`
- `panic.log`   → recreated empty on the next crash

Three generations are kept. If you want to archive a panic before
rotation, copy `panic.log.1` somewhere safe.

## Koda's terminal looks corrupted after a crash

Koda installs a panic hook that **restores the terminal** (disables raw
mode, leaves the alternate screen, drops mouse capture) before the
panic message prints. If you ever see a corrupted terminal after a
crash, that's a bug — please file an issue including the panic.log
entry above so we can fix the affected code path.

A quick local recovery:

```sh
stty sane && tput cnorm && printf '\033[?1049l\033[?1000l\033[?1006l'
```

## Where else does koda log?

| Path                                       | Contents                            |
|--------------------------------------------|-------------------------------------|
| `~/.config/koda/logs/panic.log`            | Forensic crash records (this page)  |
| `~/.config/koda/logs/koda-<pid>.log`       | Per-process tracing logs            |
| `~/.config/koda/logs/latest` *(symlink)*   | Most recent per-process tracing log |

Tracing verbosity is controlled by the `RUST_LOG` environment variable;
the default is `koda_core=info,koda_cli=info`. Common debug recipes:

```sh
# More chatter from the inference loop
RUST_LOG=koda_core::session=debug koda

# Quiet everything but warnings
RUST_LOG=warn koda
```
