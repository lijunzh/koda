// build.rs — compile-time platform gate.
//
// koda requires Unix (macOS or Linux). The Bash tool uses `sh` which
// does not exist on Windows. Fail at compile time so users get a clear
// message instead of a broken binary.

fn main() {
    if std::env::var("CARGO_CFG_UNIX").is_err() {
        panic!(
            "koda requires a Unix-like operating system (macOS or Linux). \
             Windows is not supported. On Windows, use WSL2 instead: \
             https://learn.microsoft.com/windows/wsl"
        );
    }
}
