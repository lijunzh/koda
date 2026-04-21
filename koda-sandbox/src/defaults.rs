//! Hardcoded baselines: credential paths, protected project subdirs, etc.
//!
//! These are the kernel-enforced defaults that apply *regardless* of the
//! [`crate::policy::SandboxPolicy`] passed in. The policy can *augment*
//! these baselines (Phase 1+) but cannot weaken them — they're the floor.
//!
//! All lists ported verbatim from `koda-core/src/sandbox.rs` to preserve
//! Phase 0's byte-identical-behavior guarantee.

// ── Sensitive credential paths ───────────────────────────────────────────────
//
// Two tiers of credential protection:
//
// 1. **Write-only deny** (most paths) — CLI tools can *read* their own
//    credentials (e.g. `gh`, `aws`, `kubectl`, `ssh`) but sandboxed
//    commands cannot modify them. Matches Codex's model.
//
//    Risk accepted: a sandboxed command *can* read credentials and could
//    exfiltrate them over the network. Network-level egress restriction
//    (Phase 3) is the proper mitigation — blocking reads without blocking
//    network is security theater (#855).
//
// 2. **Full read+write deny** (`koda/db` only) — koda's own API keys live
//    in a SQLite DB at `~/.config/koda/db/koda.db`. The koda process runs
//    *outside* the sandbox and doesn't need sandboxed-command access.
//    Blocking reads here prevents a `sqlite3` one-liner from dumping
//    every stored API key (#847).

/// Credential *directories* under `$HOME` that are **write-protected**.
/// Reads are allowed so CLI tools can authenticate.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const CREDENTIAL_SUBDIRS: &[&str] = &[
    ".ssh",            // SSH private keys, authorized_keys, known_hosts
    ".aws",            // AWS access key ID, secret key, session tokens
    ".gnupg",          // GPG private keys and trust database
    ".kube",           // kubeconfig with cluster tokens and client certs
    ".azure",          // Azure CLI token cache (msal_token_cache.bin, etc.)
    ".password-store", // pass(1) GPG-encrypted password store
    ".terraform.d",    // Terraform cloud tokens and plugin cache
    ".claude",         // Claude Code settings and session tokens
    ".android",        // Android SDK debug keystores and signing keys
];

/// Credential directories under `$HOME/.config/` that are **write-protected**.
/// Reads are allowed so CLI tools can authenticate.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const CREDENTIAL_CONFIG_SUBDIRS: &[&str] = &[
    "gcloud",  // gcloud CLI credentials and service-account key files
    "gh",      // GitHub CLI personal access tokens (hosts.yml)
    "op",      // 1Password CLI session tokens
    "helm",    // Helm registry auth
    "netlify", // Netlify CLI access tokens
    "vercel",  // Vercel CLI credentials
    "fly",     // Fly.io CLI auth tokens
    "doppler", // Doppler secrets manager tokens
    "stripe",  // Stripe CLI API keys
    "heroku",  // Heroku CLI OAuth tokens
];

/// Credential directories under `$HOME/.config/` where **both reads and
/// writes** are denied. These contain koda's own secrets that sandboxed
/// commands have no legitimate reason to access.
pub const CREDENTIAL_CONFIG_FULL_DENY: &[&str] = &[
    "koda/db", // SQLite DB with plaintext API keys in kv_store table (#847)
];

/// Individual credential *files* under `$HOME` that are **write-protected**.
/// Reads are allowed so tools like `curl`, `git`, `npm`, and `docker` can
/// authenticate.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const CREDENTIAL_FILES: &[&str] = &[
    ".netrc",              // FTP/HTTP credentials (curl, wget, Netrc crate)
    ".git-credentials",    // git-credential-store plaintext token file
    ".npmrc",              // npm registry auth token
    ".pypirc",             // PyPI upload API token
    ".docker/config.json", // Docker Hub credentials (auths, credsStore)
    ".vault-token",        // HashiCorp Vault session token
    ".env",                // dotenv secrets (common project-level pattern)
];

// ── Agent-file write protection ──────────────────────────────────────────────
//
// Prevent sandboxed commands from modifying koda agent definitions or the
// project-level system prompt. Same pattern as Claude Code blocking writes
// to `.claude/settings.json` — modifying these files could alter system
// prompts, tool access, or sandbox policy on next session.

/// Directories under the project root that are write-protected. Agent JSON
/// files control system prompts and tool access — a sandboxed command must
/// not be able to modify them.
pub const PROTECTED_PROJECT_SUBDIRS: &[&str] = &[
    ".koda/agents", // Agent definitions (system prompt, tools, sub-agents)
    ".koda/skills", // Skill definitions (auto-discovered, full capabilities)
];

// ── Default writable system paths ────────────────────────────────────────────

/// Filesystem locations every sandboxed process may write to regardless of
/// policy. Phase 0 hardcodes the list; Phase 1 will let `FsPolicy.allow_write`
/// extend it.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const DEFAULT_WRITE_LITERALS: &[&str] =
    &["/dev/null", "/dev/stderr", "/dev/stdout", "/dev/urandom"];

/// Subpaths under `$HOME` that are writable by default — common package
/// caches that would otherwise force re-downloads on every invocation.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const DEFAULT_WRITE_HOME_SUBDIRS: &[&str] = &[".cargo", ".npm", ".cache"];
