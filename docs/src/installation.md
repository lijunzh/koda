# Installation

Koda runs on macOS and Linux, as one binary.

## Install

**Homebrew** (Apple Silicon Macs and Linux):

```bash
brew tap lijunzh/koda
brew install koda
```

**Cargo** (any Mac or Linux with Rust 1.100.0 or newer; this is the way to
install on an Intel Mac, which has no prebuilt binary):

```bash
cargo install koda-cli --locked
```

**A release binary:** each [GitHub
release](https://github.com/lijunzh/koda/releases) has archives for Apple
Silicon Macs (`aarch64-apple-darwin`) and Linux on x86_64 and ARM64. Unpack
one and put `koda` on your `PATH`.

**From source:**

```bash
git clone https://github.com/lijunzh/koda.git
cd koda && cargo build --release -p koda-cli
# the binary is target/release/koda
```

## What Koda needs from the system

- **Linux: bubblewrap,** for the [sandbox](./sandbox.md) every command runs
  in: `apt install bubblewrap`, `dnf install bubblewrap` or `pacman -S
  bubblewrap`. Without it Koda won't start in Auto, its default [trust
  mode](./approval.md); `koda --mode safe` runs without a kernel sandbox,
  asking before every change. macOS has its sandbox built in.
- **Poppler, to read PDFs** (optional): `brew install poppler` on macOS,
  `apt install poppler-utils` on Debian and Ubuntu. Without it a short PDF
  still reaches a Claude model whole; see [PDFs](./attachments.md#pdfs).

## First run

```bash
koda
```

The first time, Koda opens its provider picker: choose a provider and enter
its API key, which Koda keeps in its local database, or give the address of a
local server (LM Studio, Ollama, llama.cpp, vLLM). A first run that already
names one skips the picker: `--provider`, `--base-url` (Koda takes the
provider from the address) or a model alias such as `--model claude-opus`.
See [Providers & model aliases](./providers.md).

Then type a request, or follow the [Quickstart](./quickstart.md) through a
first session. `/help` lists the commands and keys.

## Updating

```bash
brew upgrade koda                   # Homebrew
cargo install koda-cli --locked     # Cargo: installs the newest version over the old
```

The TUI tells you when a newer version is out (it asks crates.io). Coming
from 0.6? Read [Upgrading to 0.7](./upgrading-0.7.md).

## Uninstalling

```bash
brew uninstall koda                 # or: cargo uninstall koda-cli
rm -rf ~/.config/koda               # Koda's settings, keys, sessions and logs
```
