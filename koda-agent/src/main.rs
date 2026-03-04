fn main() {
    eprintln!();
    eprintln!("  \x1b[33m\u{26a0}  koda-agent is deprecated.\x1b[0m");
    eprintln!();
    eprintln!("  The crate has been split into two packages:");
    eprintln!("    \x1b[36mkoda-core\x1b[0m  — engine library");
    eprintln!("    \x1b[36mkoda-cli\x1b[0m   — CLI binary");
    eprintln!();
    eprintln!("  To install the latest version:");
    eprintln!("    \x1b[1mcargo install koda-cli\x1b[0m");
    eprintln!();
    eprintln!("  Then run \x1b[1mkoda\x1b[0m as before.");
    eprintln!();
    std::process::exit(1);
}
