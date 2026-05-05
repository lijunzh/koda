# Security Policy

## Supported Versions

Only the latest published release receives security fixes. We do not
backport patches to older versions.

## Reporting a Vulnerability

Please **do not** open a public GitHub issue for security vulnerabilities.

Open a [GitHub Security Advisory][advisory] with:

- A description of the vulnerability
- Steps to reproduce
- Potential impact assessment

You will receive a response within 48 hours. We aim to release a patch
within 7 days of a confirmed vulnerability.

[advisory]: https://github.com/lijunzh/koda/security/advisories/new

## Sandbox Security Model

Koda enforces filesystem-write and exec policy via a kernel-backed
sandbox (Seatbelt on macOS, bubblewrap on Linux; not supported on
Windows). The sandbox protects credential files and koda's own
configuration even in `--mode auto` trust mode. Auto requires the
kernel sandbox; if the backend is unavailable, koda refuses Auto with
an actionable setup hint and users can opt into `--mode safe`.

For the full model — file-tool read policy, write restrictions,
credential protection, agent-file protection, sub-agent inheritance,
platform backends, accepted risks, and the trust-mode × tool-effect
matrix — see the **[Sandbox reference](docs/src/sandbox.md)** in the
user docs.

## Dependency Security

CI runs `cargo audit --deny unsound --deny yanked` on every PR to catch
known vulnerabilities (RUSTSEC advisories) and yanked crate versions.
