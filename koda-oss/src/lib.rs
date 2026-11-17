//! `--oss`: run Koda on a local open-weight model, as Codex's `--oss` does,
//! with llama.cpp's `llama-server` started for the run and stopped after it.
//!
//! Codex's `--oss` connects to LM Studio or Ollama, which run as services,
//! and downloads its default model through them. `llama-server` isn't a
//! service, so for `llama-cpp` (the default local provider) Koda starts it
//! with the model, waits until it answers, points the session at it, and
//! stops it when Koda exits. `lm-studio` and `ollama` only connect.
//!
//! **The model** is picked by the machine's memory: Qwen3.8-Flash-Next on a
//! 128 GB Mac, the model Koda's local evals use; otherwise LFM3-1.7B, a 1 GB
//! quick try that shows Koda working but is too small for real coding.
//! `oss_model` in `config.toml` (or `--model <repo>:<quant>`) picks another.
//! llama.cpp downloads and caches it from Hugging Face itself (`-hf`).
//!
//! **One server per model, shared:** a second `koda --oss` on the same model
//! reuses the running server instead of loading a second copy (Flash-Next
//! is 105 GB). Each Koda using it leaves a file named by its pid under
//! `<config dir>/oss/<model>.clients/`; the last to exit stops the server.
//! A Koda that was killed leaves its file behind, so every start and stop
//! prunes files whose process is gone, and a server nobody uses any more is
//! stopped at the next start.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use std::os::unix::fs::OpenOptionsExt;

use anyhow::{Context, Result, bail};

/// A model `--oss` can run on llama.cpp.
#[derive(Debug, Clone, PartialEq)]
pub struct OssModel {
    /// The Hugging Face `<repo>:<quant>` llama.cpp downloads (`-hf`).
    pub spec: String,
    /// A short name, for messages and the server's model id.
    pub name: String,
    /// The context window to start the server with, in tokens.
    pub context: u32,
    /// The reasoning effort to run at, unless the user set one.
    pub effort: Option<&'static str>,
    /// About how many GB the download is, when Koda knows.
    pub size_gb: Option<f64>,
    /// Whether this is the quick try, too small for real coding.
    pub quick: bool,
}

/// The model for a 128 GB Mac: the one Koda's local evals use (30 of 31
/// tasks), run as Koda's author runs it daily.
pub fn full_model() -> OssModel {
    OssModel {
        spec: "unsloth/Qwen3.8-Flash-Next-GGUF:UD-Q4_K_XL".into(),
        name: "Qwen3.8-Flash-Next".into(),
        context: 262_144,
        effort: Some("xhigh"),
        size_gb: Some(105.0),
        quick: false,
    }
}

/// The quick try for any other machine: 1 GB, quick to download, and
/// enough to show Koda reading, running and editing.
pub fn quick_model() -> OssModel {
    OssModel {
        spec: "LiquidAI/LFM3-1.7B-GGUF:Q4_K_M".into(),
        name: "LFM3-1.7B".into(),
        context: 65_536,
        effort: None,
        size_gb: Some(1.1),
        quick: true,
    }
}

/// Memory at or above which Koda runs [`full_model`]. A 128 GB Mac reports
/// exactly 128 GiB; the margin covers machines that report a little less.
pub const FULL_MODEL_MEMORY: u64 = 120 * 1024 * 1024 * 1024;

/// The local provider `--oss` uses, as Codex's `--local-provider`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalProvider {
    /// llama.cpp's `llama-server`, which Koda starts and stops.
    LlamaCpp,
    /// LM Studio, a service Koda connects to.
    LmStudio,
    /// Ollama, a service Koda connects to.
    Ollama,
}

impl LocalProvider {
    /// From `--local-provider` or `oss_provider`.
    pub fn parse(name: &str) -> Result<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "llama-cpp" | "llamacpp" | "llama.cpp" => Ok(Self::LlamaCpp),
            "lm-studio" | "lmstudio" => Ok(Self::LmStudio),
            "ollama" => Ok(Self::Ollama),
            other => bail!(
                "--oss runs on llama-cpp, lm-studio or ollama, not {other:?} \
                 (--local-provider, or oss_provider in config.toml)"
            ),
        }
    }

    /// The provider id the session uses.
    pub fn provider_id(self) -> &'static str {
        match self {
            Self::LlamaCpp => "llama-cpp",
            Self::LmStudio => "lm-studio",
            Self::Ollama => "ollama",
        }
    }
}

/// What `--oss` will do, decided before anything starts.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub provider: LocalProvider,
    /// The model to start, for llama.cpp.
    pub model: Option<OssModel>,
}

/// Decide the provider and model: `--local-provider`, else `oss_provider`,
/// else llama.cpp; for llama.cpp, `--model <repo>:<quant>`, else
/// `oss_model`, else the model for this much `unified_memory`: memory the
/// GPU shares, which only a Mac has ([`unified_memory`]). Elsewhere a big
/// model in system RAM would run on the CPU, far too slowly, so the
/// default is the quick try.
pub fn plan(
    local_provider: Option<&str>,
    oss_provider: Option<&str>,
    model_flag: Option<&str>,
    oss_model: Option<&str>,
    unified_memory: Option<u64>,
) -> Result<Plan> {
    let provider = match local_provider.or(oss_provider) {
        Some(name) => LocalProvider::parse(name)?,
        None => LocalProvider::LlamaCpp,
    };
    if provider != LocalProvider::LlamaCpp {
        return Ok(Plan {
            provider,
            model: None,
        });
    }
    let named = model_flag.filter(|m| m.contains('/')).or(oss_model);
    let model = match named {
        Some(spec) => {
            let (full, quick) = (full_model(), quick_model());
            if spec == full.spec {
                full
            } else if spec == quick.spec {
                quick
            } else {
                OssModel {
                    spec: spec.to_string(),
                    name: if spec.ends_with(".gguf") {
                        gguf_name(Path::new(spec))
                    } else {
                        spec.split(['/', ':'])
                            .nth(1)
                            .unwrap_or(spec)
                            .trim_end_matches("-GGUF")
                            .to_string()
                    },
                    context: 65_536,
                    effort: None,
                    size_gb: None,
                    quick: false,
                }
            }
        }
        None if unified_memory.is_some_and(|m| m >= FULL_MODEL_MEMORY) => full_model(),
        None => quick_model(),
    };
    Ok(Plan {
        provider,
        model: Some(model),
    })
}

/// The machine's memory in bytes, when Koda can read it.
pub fn memory_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        meminfo
            .lines()
            .find_map(|line| line.strip_prefix("MemTotal:"))
            .and_then(|rest| {
                rest.trim()
                    .trim_end_matches("kB")
                    .trim()
                    .parse::<u64>()
                    .ok()
            })
            .map(|kb| kb * 1024)
    }
}

/// The memory the GPU shares with the CPU, in bytes: a Mac's whole memory.
/// `None` elsewhere, where the GPU's own memory decides what fits.
pub fn unified_memory() -> Option<u64> {
    if cfg!(target_os = "macos") {
        memory_bytes()
    } else {
        None
    }
}

/// What to tell the person before the server starts. `unified_memory` is
/// [`unified_memory`]'s answer: `None` off a Mac.
pub fn start_notice(model: &OssModel, unified_memory: Option<u64>) -> Vec<String> {
    let mut lines = Vec::new();
    if model.quick {
        let gb = unified_memory.map(|m| m / (1024 * 1024 * 1024));
        lines.push(format!(
            "--oss: running {}, a {} GB quick try: it shows Koda reading, running and \
             editing, but is too small for real coding.{}",
            model.name,
            model.size_gb.unwrap_or(1.0),
            match gb {
                Some(gb) if gb < 128 => format!(
                    " This machine has {gb} GB of memory; with 128 GB Koda runs \
                     Qwen3.8-Flash-Next. Set oss_model in config.toml to pick another \
                     model (see the Local models page)."
                ),
                Some(_) => " Set oss_model in config.toml to pick another model.".to_string(),
                None => " Koda picks a larger model only on a Mac, whose GPU shares its \
                     memory; here, set oss_model in config.toml to a model sized for your \
                     GPU (see the Local models page)."
                    .to_string(),
            }
        ));
    }
    lines
}

/// A model's name from its file: the stem, without a shard suffix such as
/// `-00001-of-00004`.
fn gguf_name(file: &Path) -> String {
    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    // `<name>-00001-of-00004`: drop the last two dash-separated parts and
    // the `of` between them when they are a shard number pair.
    let parts: Vec<&str> = stem.rsplitn(4, '-').collect();
    match parts.as_slice() {
        [total, "of", shard, name]
            if [total, shard]
                .iter()
                .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())) =>
        {
            name.to_string()
        }
        _ => stem,
    }
}

/// A model already on disk: its file (the first shard of a split one) and
/// the vision projector beside it, if there is one.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalFile {
    pub model: PathBuf,
    pub projector: Option<PathBuf>,
}

/// `model` on disk, if it is: `oss_model` given as a path to a `.gguf`
/// file (`~/` is the home folder), or a Hugging Face model LM Studio has
/// already downloaded to `~/.lmstudio/models/<org>/<repo>/`, so a model on
/// disk isn't downloaded a second time.
pub fn local_file(model: &OssModel, home: Option<&Path>) -> Option<LocalFile> {
    let spec = model.spec.as_str();
    if spec.ends_with(".gguf") {
        let file = match (spec.strip_prefix("~/"), home) {
            (Some(rest), Some(home)) => home.join(rest),
            _ => PathBuf::from(spec),
        };
        return file.is_file().then(|| LocalFile {
            projector: projector_beside(&file),
            model: file,
        });
    }
    let (repo, quant) = spec.split_once(':')?;
    let dir = home?.join(".lmstudio/models").join(repo);
    let quant = quant.to_ascii_lowercase();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            name.ends_with(".gguf") && !name.starts_with("mmproj") && name.contains(&quant)
        })
        .collect();
    files.sort();
    let file = files.into_iter().next()?;
    Some(LocalFile {
        projector: projector_beside(&file),
        model: file,
    })
}

/// The vision projector (`mmproj*.gguf`) in `file`'s folder.
fn projector_beside(file: &Path) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(file.parent()?)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("mmproj"))
        })
        .collect();
    found.sort();
    found.into_iter().next()
}

/// The `llama-server` arguments for `model` on `port`: the file on disk
/// when there is one, else `-hf`, which downloads and caches it.
pub fn server_args(model: &OssModel, local: Option<&LocalFile>, port: u16) -> Vec<String> {
    let mut args: Vec<String> = match local {
        Some(local) => {
            let mut args = vec!["-m".into(), local.model.display().to_string()];
            if let Some(projector) = &local.projector {
                args.extend(["--mmproj".into(), projector.display().to_string()]);
            }
            args
        }
        None => vec!["-hf".into(), model.spec.clone()],
    };
    args.extend([
        "--jinja".into(),
        "-c".into(),
        model.context.to_string(),
        "-ngl".into(),
        "999".into(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string(),
        "--alias".into(),
        model.name.clone(),
    ]);
    args
}

/// A model's file names under `<config dir>/oss/`.
fn slug(model: &OssModel) -> String {
    model
        .spec
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Whether process `pid` is alive.
fn alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 only checks that the process exists.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Whether process `pid` is a `llama-server`, so Koda never signals a
/// process that reused a dead server's pid.
fn is_llama_server(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).contains("llama-server"))
        .unwrap_or(false)
}

/// Why the GPU isn't free for another model, if it isn't, as koda-evals'
/// `gpu-check` asks before every local run: another model server is
/// running (`llama-server`, oMLX), LM Studio has a model loaded, or the GPU
/// is busy (over 15% on average across 3 seconds, by `ioreg`). macOS only;
/// elsewhere `None`.
pub fn gpu_busy() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let output = |program: &str, args: &[&str]| {
        std::process::Command::new(program)
            .args(args)
            .output()
            .ok()
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    if let Some(found) = output("pgrep", &["-fl", "llama-server|omlx serve|omlx-server"])
        && let Some(line) = found.lines().find(|l| !l.trim().is_empty())
    {
        let pid = line.split_whitespace().next().unwrap_or("?");
        return Some(format!("another model server is running (pid {pid})"));
    }
    if let Some(loaded) = output("lms", &["ps", "--json"])
        && lms_has_loaded(&loaded)
    {
        return Some("a model is loaded in LM Studio (lms ps)".into());
    }
    let mut samples = Vec::new();
    for i in 0..3 {
        if i > 0 {
            std::thread::sleep(Duration::from_secs(1));
        }
        let text = output("ioreg", &["-r", "-d", "1", "-c", "IOAccelerator"]).unwrap_or_default();
        samples.push(gpu_utilization(&text).unwrap_or(0));
    }
    let mean = samples.iter().sum::<u32>() / samples.len() as u32;
    (mean >= 15).then(|| format!("it is {mean}% busy"))
}

/// The GPU's utilization from `ioreg`'s `"Device Utilization %"=N`.
fn gpu_utilization(ioreg: &str) -> Option<u32> {
    let rest = ioreg.split("\"Device Utilization %\"=").nth(1)?;
    rest.split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// Whether `lms ps --json` lists a loaded model.
fn lms_has_loaded(json: &str) -> bool {
    let compact: String = json.chars().filter(|c| !c.is_whitespace()).collect();
    !compact.is_empty() && compact != "[]"
}

/// `llama-server` on `PATH`, or how to install it.
fn find_llama_server() -> Result<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join("llama-server"))
        .find(|candidate| candidate.is_file())
        .with_context(|| {
            "--oss runs local models with llama.cpp's llama-server, which isn't on PATH. \
             Install it with `brew install llama.cpp`, or use --local-provider lm-studio \
             or ollama."
        })
}

/// A free port on 127.0.0.1.
fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// The running server's record: its pid and port.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct State {
    pid: u32,
    port: u16,
    spec: String,
    /// The server's API key: llama-server allows any web origin, so
    /// without one a page in a browser could find the port and use it.
    #[serde(default)]
    key: String,
}

/// A random API key for one server.
fn random_key() -> Result<String> {
    let mut bytes = [0u8; 16];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Write `state` readable by its owner only, since it holds the key.
fn write_state(path: &Path, state: &State) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    std::io::Write::write_all(&mut file, &serde_json::to_vec(state)?)?;
    Ok(())
}

/// A `llama-server` this Koda uses: started by it, or shared with another
/// `koda --oss` on the same model. [`Server::release`] (or dropping it)
/// stops the server once no Koda uses it.
pub struct Server {
    port: u16,
    key: String,
    dir: PathBuf,
    slug: String,
}

/// The server this Koda uses, until it leaves: one per process. Leaving
/// goes through this lock, so `release`, `Drop`, the signal watcher and the
/// TUI's own signal handler leave once between them, and a second caller
/// waits until the first has finished before the process exits.
static ACTIVE: std::sync::Mutex<Option<(PathBuf, String)>> = std::sync::Mutex::new(None);

/// Leave the server this Koda uses, if any: its client file goes, and the
/// server stops if no other Koda uses it. Safe to call from any exit path,
/// any number of times; the TUI's signal handler calls it before exiting.
pub fn leave_active() {
    let mut active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((dir, slug)) = active.take() {
        leave(&dir, &slug);
    }
}

impl Server {
    /// The server's OpenAI-compatible address.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// The server's API key, which Koda's llama-cpp provider sends as
    /// `KODA_API_KEY`.
    pub fn api_key(&self) -> &str {
        &self.key
    }

    /// Start `model`'s server, or join the one running, and wait until it
    /// answers. `home` is Koda's config directory.
    /// `check_gpu` refuses to start a new server while the GPU is busy
    /// ([`gpu_busy`]); joining a running one skips it.
    pub async fn start(model: &OssModel, home: &Path, check_gpu: bool) -> Result<Self> {
        let binary = find_llama_server()?;
        let local = local_file(model, std::env::var_os("HOME").as_deref().map(Path::new));
        let dir = home.join("oss");
        let slug = slug(model);
        let clients = dir.join(format!("{slug}.clients"));
        std::fs::create_dir_all(&clients)?;
        let (port, key, started, log) = {
            let _lock = Lock::take(&dir, &slug)?;
            prune(&clients);
            let state_path = dir.join(format!("{slug}.json"));
            let running = std::fs::read(&state_path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<State>(&bytes).ok())
                .filter(|state| alive(state.pid) && is_llama_server(state.pid));
            let log = home.join("logs").join(format!("llama-server-{slug}.log"));
            let (port, key, started) = match running {
                Some(state) => (state.port, state.key, false),
                None => {
                    if check_gpu && let Some(reason) = gpu_busy() {
                        bail!(
                            "--oss: the GPU is busy: {reason}. Loading {} beside it could \
                             run out of memory. Stop it and try again, or set \
                             oss_check_gpu = false in config.toml to start anyway.",
                            model.name
                        );
                    }
                    let port = free_port()?;
                    let key = random_key()?;
                    std::fs::create_dir_all(home.join("logs"))?;
                    let out = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&log)?;
                    let mut command = std::process::Command::new(&binary);
                    command
                        .args(server_args(model, local.as_ref(), port))
                        .args(["--api-key", &key])
                        .stdin(std::process::Stdio::null())
                        .stdout(out.try_clone()?)
                        .stderr(out);
                    // Its own process group: Ctrl+C in the terminal reaches
                    // Koda, which decides whether the server stops.
                    std::os::unix::process::CommandExt::process_group(&mut command, 0);
                    let child = command
                        .spawn()
                        .with_context(|| format!("couldn't start {}", binary.display()))?;
                    let state = State {
                        pid: child.id(),
                        port,
                        spec: model.spec.clone(),
                        key: key.clone(),
                    };
                    write_state(&state_path, &state)?;
                    (port, key, true)
                }
            };
            std::fs::write(clients.join(std::process::id().to_string()), b"")?;
            (port, key, started, log)
        };
        // Other models' servers that no live Koda uses any more.
        reap_orphans(&dir, &slug);
        *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some((dir.clone(), slug.clone()));
        let server = Self {
            port,
            key,
            dir,
            slug,
        };
        server.watch_signals();
        if started {
            eprintln!(
                "--oss: starting llama-server with {} on port {port}{}",
                model.name,
                match (&local, model.size_gb) {
                    (Some(local), _) => format!(" from {}", local.model.display()),
                    (None, Some(gb)) => format!(
                        "; the first run downloads it (about {gb} GB) into the \
                         Hugging Face cache"
                    ),
                    (None, None) => String::new(),
                }
            );
        }
        server
            .wait_until_ready(&model.name, &log, local.is_some())
            .await?;
        Ok(server)
    }

    /// Wait until the server answers `/health`, saying what it's doing every
    /// 15 seconds. A server that exits, or Ctrl+C, stops the start.
    async fn wait_until_ready(&self, name: &str, log: &Path, on_disk: bool) -> Result<()> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?;
        let url = format!("http://127.0.0.1:{}/health", self.port);
        let started = Instant::now();
        let mut said = Instant::now();
        loop {
            if let Ok(response) = client.get(&url).send().await
                && response.status().is_success()
            {
                if started.elapsed() > Duration::from_secs(2) {
                    eprintln!("--oss: {name} is ready ({} s)", started.elapsed().as_secs());
                }
                return Ok(());
            }
            if let Some(state) = self.state()
                && !alive(state.pid)
            {
                let tail = std::fs::read_to_string(log)
                    .map(|text| {
                        let lines: Vec<&str> = text.lines().collect();
                        lines[lines.len().saturating_sub(8)..].join("\n")
                    })
                    .unwrap_or_default();
                bail!(
                    "llama-server stopped before {name} was ready. Its log, {}:\n{tail}",
                    log.display()
                );
            }
            if said.elapsed() >= Duration::from_secs(15) {
                eprintln!(
                    "--oss: {name} is still {} ({} s); its log is {}",
                    if on_disk {
                        "loading"
                    } else {
                        "downloading or loading"
                    },
                    started.elapsed().as_secs(),
                    log.display()
                );
                said = Instant::now();
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                _ = tokio::signal::ctrl_c() => bail!("--oss: stopped while {name} was starting"),
            }
        }
    }

    fn state(&self) -> Option<State> {
        std::fs::read(self.dir.join(format!("{}.json", self.slug)))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    }

    /// Leave the server: stop it if no other Koda uses it.
    pub fn release(mut self) {
        self.leave();
    }

    fn leave(&mut self) {
        leave_active();
    }

    /// A closed terminal (SIGHUP) or a `kill` (SIGTERM) ends Koda without
    /// running its destructors, which would leave the server running. While
    /// this Koda uses it, those signals leave the server first, then exit
    /// as the signal would have.
    fn watch_signals(&self) {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut hangup), Ok(mut terminate)) = (
            signal(SignalKind::hangup()),
            signal(SignalKind::terminate()),
        ) else {
            return;
        };
        tokio::spawn(async move {
            let code = tokio::select! {
                _ = hangup.recv() => 128 + libc::SIGHUP,
                _ = terminate.recv() => 128 + libc::SIGTERM,
            };
            tracing::info!("--oss: signal {}, leaving the server", code - 128);
            leave_active();
            std::process::exit(code);
        });
    }
}

/// This Koda leaves `slug`'s server, once: its client file goes, and the
/// server stops if no other Koda uses it.
fn leave(dir: &Path, slug: &str) {
    let Ok(_lock) = Lock::take(dir, slug) else {
        return;
    };
    let clients = dir.join(format!("{slug}.clients"));
    let _ = std::fs::remove_file(clients.join(std::process::id().to_string()));
    tracing::info!("--oss: left {slug}'s server");
    stop_if_unused(dir, slug);
}

/// Stop `slug`'s server and drop its record if no live Koda uses it. The
/// caller holds the lock.
fn stop_if_unused(dir: &Path, slug: &str) {
    let clients = dir.join(format!("{slug}.clients"));
    prune(&clients);
    let users = std::fs::read_dir(&clients)
        .map(|entries| entries.flatten().count())
        .unwrap_or(0);
    if users > 0 {
        return;
    }
    let state_path = dir.join(format!("{slug}.json"));
    let state = std::fs::read(&state_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<State>(&bytes).ok());
    if let Some(state) = state
        && alive(state.pid)
        && is_llama_server(state.pid)
    {
        tracing::info!("--oss: stopping {slug}'s server (pid {})", state.pid);
        stop(state.pid);
    }
    let _ = std::fs::remove_file(state_path);
}

/// Stop other models' servers that no live Koda uses any more: left by a
/// Koda killed outright (`kill -9`), which no handler can catch. `current`
/// is the model being started, handled under its own lock.
fn reap_orphans(dir: &Path, current: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(slug) = name.strip_suffix(".json") else {
            continue;
        };
        if slug == current {
            continue;
        }
        if let Ok(_lock) = Lock::take(dir, slug) {
            stop_if_unused(dir, slug);
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.leave();
    }
}

/// Stop `pid`: SIGTERM, then SIGKILL after 10 seconds.
fn stop(pid: u32) {
    // SAFETY: signalling a pid checked to be a llama-server this Koda started.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(pid) && Instant::now() < deadline {
        // A server this Koda started is its child, and stays a zombie (still
        // "alive" to `kill -0`) until reaped; one another Koda started is
        // reaped by init.
        // SAFETY: WNOHANG never blocks; a pid that isn't our child is ECHILD.
        if unsafe { libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) }
            == pid as libc::pid_t
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if alive(pid) {
        // SAFETY: as above.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }
}

/// Remove the client files of Koda processes that are gone.
fn prune(clients: &Path) {
    if let Ok(entries) = std::fs::read_dir(clients) {
        for entry in entries.flatten() {
            let gone = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
                .is_none_or(|pid| !alive(pid));
            if gone {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// An exclusive lock on one model's records, so two Kodas starting at once
/// start one server.
struct Lock(std::fs::File);

impl Lock {
    fn take(dir: &Path, slug: &str) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(format!("{slug}.lock")))?;
        file.lock()?;
        Ok(Self(file))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn a_128_gb_mac_gets_flash_next_and_anything_else_the_quick_try() {
        let at = |gb| plan(None, None, None, None, Some(gb * GIB)).unwrap();
        assert_eq!(at(128).model, Some(full_model()));
        assert_eq!(at(192).model, Some(full_model()));
        assert_eq!(at(64).model, Some(quick_model()));
        assert_eq!(at(16).model, Some(quick_model()));
        assert_eq!(
            plan(None, None, None, None, None).unwrap().model,
            Some(quick_model())
        );
        assert_eq!(at(128).provider, LocalProvider::LlamaCpp);
    }

    #[test]
    fn a_named_model_wins_over_memory() {
        let spec = "ornith-ai/Ornith-1.5-35B-A3B-GGUF:Q4_K_M";
        let configured = plan(None, None, None, Some(spec), Some(128 * GIB)).unwrap();
        let model = configured.model.unwrap();
        assert_eq!(model.spec, spec);
        assert_eq!(model.name, "Ornith-1.5-35B-A3B");
        assert!(!model.quick);
        // --model with a repo beats oss_model; a plain model id doesn't.
        let flagged = plan(
            None,
            None,
            Some("LiquidAI/LFM3-1.7B-GGUF:Q4_K_M"),
            Some(spec),
            None,
        );
        assert_eq!(flagged.unwrap().model, Some(quick_model()));
        let plain = plan(None, None, Some("lfm3"), Some(spec), None).unwrap();
        assert_eq!(plain.model.unwrap().spec, spec);
    }

    #[test]
    fn a_service_provider_is_only_connected_to() {
        for (flag, config, want) in [
            (Some("lm-studio"), None, LocalProvider::LmStudio),
            (None, Some("ollama"), LocalProvider::Ollama),
            (Some("llama.cpp"), Some("ollama"), LocalProvider::LlamaCpp),
        ] {
            let plan = plan(flag, config, None, None, Some(128 * GIB)).unwrap();
            assert_eq!(plan.provider, want);
            assert_eq!(plan.model.is_some(), want == LocalProvider::LlamaCpp);
        }
        assert!(plan(Some("vllm"), None, None, None, None).is_err());
    }

    #[test]
    fn the_server_runs_the_model_with_its_template_window_and_gpu() {
        let args = server_args(&full_model(), None, 41234).join(" ");
        assert_eq!(
            args,
            "-hf unsloth/Qwen3.8-Flash-Next-GGUF:UD-Q4_K_XL --jinja -c 262144 -ngl 999 \
             --host 127.0.0.1 --port 41234 --alias Qwen3.8-Flash-Next"
        );
    }

    #[test]
    fn a_model_on_disk_is_used_where_it_is() {
        let home = tempfile::tempdir().unwrap();
        let dir = home
            .path()
            .join(".lmstudio/models/unsloth/Qwen3.8-Flash-Next-GGUF");
        std::fs::create_dir_all(&dir).unwrap();
        for file in [
            "Qwen3.8-Flash-Next-UD-Q4_K_XL-00002-of-00004.gguf",
            "Qwen3.8-Flash-Next-UD-Q4_K_XL-00001-of-00004.gguf",
            "mmproj-F16.gguf",
        ] {
            std::fs::write(dir.join(file), b"").unwrap();
        }
        // The default model, found in LM Studio's folder: no download.
        let local = local_file(&full_model(), Some(home.path())).unwrap();
        assert!(
            local
                .model
                .ends_with("Qwen3.8-Flash-Next-UD-Q4_K_XL-00001-of-00004.gguf")
        );
        assert!(
            local
                .projector
                .as_ref()
                .unwrap()
                .ends_with("mmproj-F16.gguf")
        );
        let args = server_args(&full_model(), Some(&local), 1).join(" ");
        assert!(
            args.starts_with("-m ") && args.contains(" --mmproj "),
            "{args}"
        );
        // Not there: downloaded with -hf.
        assert!(local_file(&quick_model(), Some(home.path())).is_none());

        // oss_model as a path, with ~/.
        let path = "~/.lmstudio/models/unsloth/Qwen3.8-Flash-Next-GGUF/Qwen3.8-Flash-Next-UD-Q4_K_XL-00001-of-00004.gguf";
        let model = plan(None, None, None, Some(path), None)
            .unwrap()
            .model
            .unwrap();
        assert_eq!(model.name, "Qwen3.8-Flash-Next-UD-Q4_K_XL");
        assert_eq!(
            local_file(&model, Some(home.path())).unwrap().model,
            local.model
        );
    }

    #[test]
    fn the_busy_check_reads_ioreg_and_lms() {
        let ioreg = r#"| "PerformanceStatistics" = {"Tiler Utilization %"=3,"Device Utilization %"=42,"Renderer Utilization %"=40}"#;
        assert_eq!(gpu_utilization(ioreg), Some(42));
        assert_eq!(gpu_utilization("no GPU here"), None);
        assert!(!lms_has_loaded(""));
        assert!(!lms_has_loaded("[ ]\n"));
        assert!(lms_has_loaded(r#"[{"identifier":"qwen3.8-27b"}]"#));
    }

    #[test]
    fn off_a_mac_the_quick_try_says_to_name_a_model() {
        let notice = start_notice(&quick_model(), None).join(" ");
        assert!(notice.contains("only on a Mac"), "{notice}");
    }

    #[test]
    fn the_quick_try_says_what_it_is() {
        let notice = start_notice(&quick_model(), Some(32 * GIB)).join(" ");
        assert!(notice.contains("too small for real coding"), "{notice}");
        assert!(notice.contains("32 GB"), "{notice}");
        assert!(start_notice(&full_model(), Some(128 * GIB)).is_empty());
    }

    #[test]
    fn clients_that_are_gone_are_pruned() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(std::process::id().to_string()), b"").unwrap();
        // A pid far above any real one.
        std::fs::write(dir.path().join("999999999"), b"").unwrap();
        std::fs::write(dir.path().join("not-a-pid"), b"").unwrap();
        prune(dir.path());
        let left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec![std::process::id().to_string()]);
    }
}
