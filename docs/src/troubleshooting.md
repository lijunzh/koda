# Troubleshooting

## Koda crashed — where do I find the diagnostic info?

A crash is logged as an `ERROR` line from `koda_cli::panic`, with Koda's
version, where it panicked, the message and the backtrace, in the log
database with the session it happened in:

```sh
koda-logs --target koda_cli::panic --since 1d
```

Koda stores it before exiting, so it's there even though a release build
stops at once.

### Getting a useful backtrace

By default the line says the backtrace is
`(disabled; run with RUST_BACKTRACE=1 to capture it)`. To capture one, re-run
koda with:

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

1. The crash line from `koda-logs --target koda_cli::panic`, with
   `RUST_BACKTRACE=1` if possible.
2. The lines before it: `koda-logs --session <id> --sessionless --backfill 300`
   (the session id is on the crash line).
3. Your provider and model (e.g. `anthropic claude-sonnet-4-5`).

## Koda's terminal looks corrupted after a crash

Koda installs a panic hook that **restores the terminal** (disables raw
mode, leaves the alternate screen, drops mouse capture) before the
panic message prints. If you ever see a corrupted terminal after a
crash, that's a bug — please file an issue including the crash line
above so we can fix the affected code path.

A quick local recovery:

```sh
stty sane && tput cnorm && printf '\033[?1049l\033[?1000l\033[?1006l'
```

## Where else does koda log?

| Path | Contents |
|------|----------|
| `~/.config/koda/db/logs_1.db` | Every line, by session; read it with `koda-logs` |
| `<log_dir>/koda.log` | Only when `config.toml` sets `log_dir`: every line at `RUST_LOG`'s level |

See [Debugging a run](./debugging.md#the-log). `RUST_LOG` sets the log file's
level; the default is every Koda crate at `info`. Common debug recipes:

```sh
# A log file at ~/.config/koda/logs/koda.log, with prompts and summaries
RUST_LOG=koda_core=debug,koda_cli=info koda -c log_dir=logs

# A log file with warnings only
RUST_LOG=warn koda -c log_dir=logs
```

To work out what a run did from its log, see
[Debugging a run](./debugging.md).
