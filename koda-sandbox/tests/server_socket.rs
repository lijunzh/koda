//! A sandboxed command can't reach Koda's shared server.
//!
//! The server (`koda serve`) acts for the user: whoever can send it requests
//! can answer an approval or add an MCP server, which runs outside the
//! sandbox. So the folder of its socket is closed to every sandboxed
//! command, as Codex closes its daemon's (`seatbelt_daemon.rs`, the daemon
//! masks in its `bwrap.rs`).
//!
//! The sandbox reads `HOME` and `XDG_CONFIG_HOME`, so each case runs this
//! test binary again with its own: [`case`] builds the sandboxed command,
//! which runs the binary once more as [`client`], the program trying to
//! connect. Both are ignored in an ordinary run.
#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;

use koda_sandbox::{Loopback, SandboxPolicy, SandboxTransformRequest};

/// A listener at `socket` that greets whoever connects.
fn greet(socket: &Path) {
    let listener = UnixListener::bind(socket).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let _ = stream.unwrap().write_all(b"hello from koda");
        }
    });
}

fn available() -> bool {
    #[cfg(target_os = "linux")]
    return koda_sandbox::bwrap::is_available();
    #[cfg(not(target_os = "linux"))]
    true
}

struct Case {
    _dir: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    xdg: Option<PathBuf>,
    /// The policy lists the project as writable itself, as a sub-agent's
    /// does.
    allow_write: bool,
}

impl Case {
    fn new() -> Self {
        // Not in the temp folder, which a sandboxed command may write: the
        // server's real folder isn't there either.
        let dir = tempfile::Builder::new()
            .prefix("server-socket-")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("h");
        let project = root.join("p");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        Self {
            _dir: dir,
            home,
            project,
            xdg: None,
            allow_write: false,
        }
    }

    /// Koda's folder in this case.
    fn koda(&self) -> PathBuf {
        match &self.xdg {
            Some(xdg) => xdg.join("koda"),
            None => self.home.join(".config/koda"),
        }
    }

    /// Start a server at its usual place; its socket.
    fn serve(&self) -> PathBuf {
        let folder = self.koda().join("serve");
        std::fs::create_dir_all(&folder).unwrap();
        let socket = folder.join("koda.sock");
        greet(&socket);
        socket
    }

    /// What `script` prints in the sandbox, `CLIENT path` connecting to
    /// `path`.
    fn run(&self, script: &str) -> String {
        self.run_with(None, script)
    }

    /// [`Case::run`] in a process that listens at `socket`.
    fn run_protecting(&self, socket: &Path, script: &str) -> String {
        self.run_with(Some(socket), script)
    }

    fn run_with(&self, protect: Option<&Path>, script: &str) -> String {
        let exe = std::env::current_exe().unwrap();
        let mut command = Command::new(&exe);
        command
            .args(["--exact", "case", "--ignored", "--nocapture"])
            .env("HOME", &self.home)
            .env_remove("XDG_CONFIG_HOME")
            .env("CASE_PROJECT", &self.project)
            .env("CASE_SCRIPT", script);
        if let Some(xdg) = &self.xdg {
            command.env("XDG_CONFIG_HOME", xdg);
        }
        if self.allow_write {
            command.env("CASE_ALLOW_WRITE", "1");
        }
        if let Some(socket) = protect {
            command.env("CASE_PROTECT", socket);
        }
        let output = command.output().unwrap();
        let text = String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr);
        assert!(text.contains("case ran"), "{text}");
        text
    }

    #[track_caller]
    fn assert_closed(&self, script: &str) {
        let text = self.run(script);
        assert!(!text.contains("hello from koda"), "{script}\n{text}");
        assert!(text.contains("client:"), "the client never ran: {text}");
    }
}

/// Run by [`Case::run`]: `CASE_SCRIPT` in the sandbox.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "run by the other tests"]
async fn case() {
    let exe = std::env::current_exe().unwrap();
    let client = format!(
        "client() {{ CLIENT_SOCKET=\"$1\" '{}' --exact client --ignored --nocapture 2>&1; }}; ",
        exe.display()
    );
    let script = client
        + &std::env::var("CASE_SCRIPT")
            .unwrap()
            .replace("CLIENT", "client");
    let project = PathBuf::from(std::env::var("CASE_PROJECT").unwrap());
    if let Some(socket) = std::env::var_os("CASE_PROTECT") {
        koda_sandbox::protect_socket(Path::new(&socket));
    }
    let mut policy = SandboxPolicy::strict_default();
    if std::env::var_os("CASE_ALLOW_WRITE").is_some() {
        policy.fs.allow_write = vec![project.clone().into()];
    }
    let request = SandboxTransformRequest {
        command: &script,
        project_root: &project,
        policy: &policy,
        proxy_port: None,
        loopback: &Loopback::default(),
    };
    let mut command = koda_sandbox::current_runtime()
        .transform(request)
        .unwrap()
        .command;
    let output = command.output().await.unwrap();
    println!(
        "{}{}case ran",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Run by [`case`], in the sandbox: connect to `CLIENT_SOCKET`.
#[test]
#[ignore = "run by the other tests"]
fn client() {
    let socket = std::env::var("CLIENT_SOCKET").unwrap();
    let mut said = String::new();
    match UnixStream::connect(&socket) {
        Ok(mut stream) => {
            stream.read_to_string(&mut said).ok();
            println!("client: connected: {said}");
        }
        Err(e) => println!("client: refused: {e}"),
    }
}

#[test]
fn a_sandboxed_command_cannot_reach_the_server() {
    if !available() {
        koda_sandbox::skip_test("no working sandbox here");
        return;
    }
    let case = Case::new();
    let socket = case.serve();
    case.assert_closed(&format!("CLIENT '{}'", socket.display()));
    // The folder is closed whole: nothing to see, and no socket of the
    // command's own where clients look for Koda's.
    let planted = socket.with_file_name("planted");
    let text = case.run(&format!(
        "ls -la '{}'; touch '{}'; rm -f '{}'",
        socket.parent().unwrap().display(),
        planted.display(),
        socket.display()
    ));
    assert!(!text.contains("koda.sock\n"), "{text}");
    assert!(!planted.exists(), "{text}");
    assert!(UnixStream::connect(&socket).is_ok(), "{text}");
}

/// A session started in the home folder makes Koda's own folder part of
/// what a command may write. Its settings stay read-only all the same, as
/// Codex keeps `.codex` inside a writable root: an added MCP server would
/// run outside the sandbox in the next session.
#[test]
fn kodas_settings_cant_be_written_from_a_session_in_the_home_folder() {
    if !available() {
        koda_sandbox::skip_test("no working sandbox here");
        return;
    }
    let mut case = Case::new();
    case.project = case.home.clone();
    std::fs::create_dir_all(case.koda()).unwrap();
    let config = case.koda().join("config.toml");
    std::fs::write(&config, "model = \"stub\"\n").unwrap();
    let planted = case.koda().join("skills-new.md");
    let note = case.home.join("note.txt");

    let text = case.run(&format!(
        "echo 'mcp_servers = 1' >> '{}'; echo x > '{}'; echo ok > '{}'; cat '{}'",
        config.display(),
        planted.display(),
        note.display(),
        config.display(),
    ));
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        "model = \"stub\"\n",
        "{text}"
    );
    assert!(!planted.exists(), "{text}");
    // The rest of the folder is the session's to write, and the settings
    // can still be read.
    assert!(note.exists(), "{text}");
    assert!(text.contains("model = \"stub\""), "{text}");
}

/// The checkouts Koda makes for `--worktree` and for sub-agents are inside
/// its folder: a session in one writes its checkout, and nothing else of
/// Koda's.
#[test]
fn a_session_in_one_of_kodas_worktrees_writes_its_checkout_only() {
    if !available() {
        koda_sandbox::skip_test("no working sandbox here");
        return;
    }
    let mut case = Case::new();
    case.project = case.koda().join("worktrees/repo/wt-1");
    std::fs::create_dir_all(&case.project).unwrap();
    let config = case.koda().join("config.toml");
    std::fs::write(&config, "model = \"stub\"\n").unwrap();
    let edited = case.project.join("a.txt");

    let text = case.run(&format!(
        "echo ok > '{}'; echo 'mcp_servers = 1' >> '{}'",
        edited.display(),
        config.display(),
    ));
    assert!(edited.exists(), "{text}");
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        "model = \"stub\"\n",
        "{text}"
    );
}

/// Koda's folder moves with `XDG_CONFIG_HOME`, and the rule with it.
#[test]
fn a_server_under_xdg_config_home_is_closed_too() {
    if !available() {
        koda_sandbox::skip_test("no working sandbox here");
        return;
    }
    let mut case = Case::new();
    case.xdg = Some(case.home.join("x"));
    let socket = case.serve();
    case.assert_closed(&format!("CLIENT '{}'", socket.display()));
}

/// Another name for the socket is the same socket.
#[test]
fn another_path_to_the_socket_is_closed_too() {
    if !available() {
        koda_sandbox::skip_test("no working sandbox here");
        return;
    }
    let case = Case::new();
    let socket = case.serve();
    let path = socket.display();
    let pid = std::process::id();
    let mut scripts = vec![
        format!("ln -s '{path}' s; CLIENT s"),
        format!(
            "ln -s '{}' d; CLIENT d/serve/koda.sock",
            case.koda().display()
        ),
        format!("cd '{}' && CLIENT serve/koda.sock", case.koda().display()),
        format!("ln '{path}' hard 2>/dev/null; CLIENT hard"),
    ];
    if cfg!(target_os = "macos") {
        scripts.push(format!("CLIENT '/System/Volumes/Data{path}'"));
        scripts.push(format!(
            "CLIENT '{}'",
            path.to_string()
                .replace("serve/koda.sock", "SERVE/KODA.SOCK")
        ));
    } else {
        scripts.push(format!("CLIENT '/proc/{pid}/root{path}'"));
        scripts.push(format!("CLIENT '/proc/self/root{path}'"));
        // A namespace of the command's own can't take the mask off.
        scripts.push(format!(
            "unshare -Urm sh -c 'umount {folder}; umount -l {folder}' 2>/dev/null; CLIENT '{path}'; \
             unshare -Urm sh -c 'umount -l {folder}; CLIENT {path}; mkdir -p m; \
             mount --bind {koda} m && CLIENT m/serve/koda.sock; \
             mount --rbind {koda} m && CLIENT m/serve/koda.sock' 2>/dev/null",
            folder = socket.parent().unwrap().display(),
            koda = case.koda().display()
        ));
    }
    for script in scripts {
        case.assert_closed(&script);
    }
}

/// In a project that holds Koda's folder (the home folder itself), a command
/// may write around the server's folder, and still can't move it somewhere
/// the rule doesn't name.
#[test]
fn moving_a_folder_above_the_socket_does_not_open_it() {
    if !available() {
        koda_sandbox::skip_test("no working sandbox here");
        return;
    }
    for allow_write in [false, true] {
        let mut case = Case::new();
        case.allow_write = allow_write;
        let socket = case.serve();
        case.project = case.home.clone();
        case.assert_closed(&format!(
            "cd '{}'; mv serve s2; CLIENT s2/koda.sock; mv ../koda ../k2; CLIENT ../k2/serve/koda.sock; \
             mv ../../.config ../../c2; CLIENT '{}/c2/koda/serve/koda.sock'; CLIENT '{}'",
            case.koda().display(),
            case.home.display(),
            socket.display(),
        ));
        // Nor for the next command.
        case.assert_closed(
            "for s in $(find . -name koda.sock 2>/dev/null) .config/koda/serve/koda.sock; \
             do CLIENT $s; done",
        );
        assert!(UnixStream::connect(&socket).is_ok(), "it moved");
        let text = case.run("touch .config/fine && echo wrote it");
        assert!(text.contains("wrote it"), "{text}");
    }
}

/// A command that started before the server did.
#[test]
fn a_server_that_starts_later_is_closed_to_a_running_command() {
    if !available() {
        koda_sandbox::skip_test("no working sandbox here");
        return;
    }
    let case = Case::new();
    std::fs::create_dir_all(case.koda()).unwrap();
    let ready = case.project.join("ready");
    let script = format!(
        "touch started; while [ ! -e ready ]; do sleep 0.1; done; CLIENT '{}'",
        case.koda().join("serve/koda.sock").display()
    );
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !case.project.join("started").exists() {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            case.serve();
            std::fs::write(&ready, "").unwrap();
        });
        case.assert_closed(&script);
    });
}

/// `koda serve --listen unix://…`: the server's own sessions can't reach it
/// either, wherever its socket is.
#[test]
fn a_socket_somewhere_else_is_closed_to_its_own_sessions() {
    if !available() {
        koda_sandbox::skip_test("no working sandbox here");
        return;
    }
    let case = Case::new();
    let socket = case.project.join("own.sock");
    greet(&socket);
    let text = case.run("CLIENT own.sock");
    assert!(text.contains("hello from koda"), "the control: {text}");
    let text = case.run_protecting(&socket, "CLIENT own.sock; mv own.sock o2; CLIENT o2");
    assert!(!text.contains("hello from koda"), "{text}");
    assert!(text.contains("client:"), "{text}");
}
