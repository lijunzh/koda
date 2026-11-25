# koda-search

ugrep and bfs inside the [Koda](https://github.com/lijunzh/koda) binary, as Claude Code builds them into its own executable. Started under the name `ugrep` or `bfs` (a link of that name), `koda` runs that program, so Koda's shell can give the model `grep` and `find` that honour `.gitignore`, as Claude Code's does, however Koda was installed.

The programs are ugrep 7.8.5 (with PCRE2 and `-z` for zlib, bzip2, zstd, brotli, 7z and tar/zip) and bfs 4.1.7 (with Oniguruma), configured as Claude Code's are. Their C and C++ sources are vendored unchanged under `vendor/`, each with its license; `build.rs` compiles them, so building needs a C and C++ compiler (Xcode's command-line tools on macOS, `build-essential` on Debian and Ubuntu).

**Versioning:** published so that `cargo install koda-cli` builds. It follows
koda-cli's version, and its Rust API makes no semver promise of its own: a
0.7.x release may change it. The Koda crates pin each other exactly.

- [User manual](https://lijunzh.github.io/koda/)
- [API docs](https://docs.rs/koda-search)

## License

Koda's own code in this crate is MIT. The vendored programs and libraries keep their own licenses: ugrep and PCRE2 BSD-3-Clause, Oniguruma BSD-2-Clause, bfs 0BSD and Brotli MIT (see `vendor/` and the repository's NOTICE).
