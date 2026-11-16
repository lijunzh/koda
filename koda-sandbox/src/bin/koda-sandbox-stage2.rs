//! `koda-sandbox-stage2` — in-sandbox helper that runs after
//! `bwrap --unshare-net` strips host networking, before the user
//! command (Phase 3c.1.c of #934).
//!
//! See `koda_sandbox::stage2` (Linux-only module) for the architecture
//! diagram and the full lifecycle. This file is just the binary shim.
//!
//! Installed Koda doesn't use this binary: `koda` runs stage 2 itself
//! when its first argument is `koda_sandbox::stage2::STAGE2_ARG1` (see
//! `koda_sandbox::dispatch_helpers`), as Codex's binary runs its sandbox
//! helpers, because the release archives and `cargo install` carry only
//! `koda`. This binary stays for test binaries, which aren't `koda`.
//!
//! ## Why it was first a separate binary
//!
//! - **Tiny binary.** Stage 2 doesn't link tokio, the LLM client, the
//!   TUI, MCP, etc. — just std + libc. Faster startup, smaller
//!   memory, smaller bind-mount footprint.
//! - **Same precedent as `koda-fs-worker`.** koda-sandbox already
//!   ships one helper binary and the discovery story (`current_exe`
//!   sibling lookup, `KODA_FS_WORKER_BIN` override) is a known
//!   pattern. This is just one more.
//! - **Test isolation.** Cargo sets `CARGO_BIN_EXE_koda-sandbox-stage2`
//!   for integration tests, so e2e tests can find the binary without
//!   knowing the install path.

#[cfg(target_os = "linux")]
fn main() {
    // Read argv as raw OsStrings — the user command may contain bytes
    // that aren't valid UTF-8 (e.g. filenames in unusual encodings).
    koda_sandbox::stage2::main(std::env::args_os().collect());
}

#[cfg(not(target_os = "linux"))]
fn main() {
    // Stage 2 is Linux-only — the bwrap kernel-enforced egress proxy
    // doesn't run anywhere else. We still build a stub on macOS so
    // `cargo build --workspace` works without per-target plumbing.
    eprintln!("koda-sandbox-stage2: Linux only (this build target lacks bwrap support).");
    std::process::exit(1);
}
