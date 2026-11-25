//! Builds ugrep and bfs into a static library, as Claude Code builds them
//! into its own executable (see `src/lib.rs`).
//!
//! The sources are vendored unchanged under `vendor/` (each folder's
//! `.commit` names the release). Their own builds are autotools (ugrep) and
//! a `./configure` of compile probes (bfs); this script does what those do
//! for the targets Koda ships (macOS and Linux):
//!
//! - **ugrep 7.8.5** with the features Claude Code's has (`ugrep -V`:
//!   `-P:pcre2jit; -z:zlib,bzip2,zstd,brotli,7z,tar/pax/cpio/zip`). Its
//!   `config.h` is what `./configure` writes on each OS (checked on macOS 27
//!   and Ubuntu 26.04), and PCRE2 10.49 and brotli 1.2.1's decoder are
//!   vendored; zlib, bzip2 and zstd come from their `-sys` crates.
//! - **bfs 4.1.7** configured as Claude Code's is (`bfs -version`:
//!   `--enable-release --with-oniguruma --without-libselinux --without-libacl
//!   --without-liburing --without-libcap`), with oniguruma 6.9.10 vendored.
//!   Its feature probes (`build/has/*.c`) run here as `./configure` runs
//!   them. On macOS they compile for the deployment target and fail on an
//!   API newer than it, and `fdclosedir` (macOS 26.4) is left out, as
//!   Claude Code's build leaves it out.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const UGREP_VERSION: &str = "7.8.5";
const BFS_VERSION: &str = "4.1.7";
const BFS_CONFFLAGS: &str = "--enable-release --with-oniguruma --without-libselinux \
     --without-libacl --without-liburing --without-libcap";

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let vendor = manifest.join("vendor");
    println!("cargo:rerun-if-changed=vendor");
    println!("cargo:rerun-if-changed=src/ugrep_main.cpp");
    println!("cargo:rerun-if-changed=src/bfs_main.c");

    let os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    if os != "macos" && os != "linux" {
        panic!("koda-search builds for macOS and Linux only, not {os}");
    }
    if os == "macos" {
        use_rust_deployment_target();
    }

    let onig_include = build_oniguruma(&vendor.join("oniguruma"), &out);
    let pcre2_include = build_pcre2(&vendor.join("pcre2"), &out);
    let brotli_include = build_brotli(&vendor.join("brotli"));
    build_ugrep(
        &vendor.join("ugrep"),
        &manifest,
        &out,
        &os,
        &pcre2_include,
        &brotli_include,
    );
    build_bfs(&vendor.join("bfs"), &manifest, &out, &os, &onig_include);

    if os == "linux" {
        println!("cargo:rustc-link-lib=rt");
    }
}

/// Compile the C and C++ for the macOS version `koda` itself is linked
/// for. Without `MACOSX_DEPLOYMENT_TARGET`, cc targets the macOS it runs on
/// while rustc links for its own minimum (11.0 on Apple Silicon), and bfs's
/// probes would then find APIs (`fdclosedir`, macOS 26.4) that older
/// systems lack, which would stop `koda` from starting on them.
fn use_rust_deployment_target() {
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    if env::var_os("MACOSX_DEPLOYMENT_TARGET").is_some() {
        return;
    }
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let target = env::var("TARGET").unwrap();
    let out = Command::new(rustc)
        .args(["--print", "deployment-target", "--target", &target])
        .output()
        .expect("run rustc --print deployment-target");
    let text = String::from_utf8_lossy(&out.stdout);
    let version = text
        .trim()
        .strip_prefix("MACOSX_DEPLOYMENT_TARGET=")
        .expect("rustc printed a deployment target");
    // SAFETY: the build script is single-threaded here, before any build.
    unsafe { env::set_var("MACOSX_DEPLOYMENT_TARGET", version) };
}

/// A `cc::Build` with the warnings of vendored code off: they aren't ours
/// to act on.
fn build() -> cc::Build {
    let mut b = cc::Build::new();
    b.warnings(false).extra_warnings(false);
    b
}

fn build_oniguruma(dir: &Path, out: &Path) -> PathBuf {
    let src = dir.join("src");
    let config = out.join("onig");
    fs::create_dir_all(&config).unwrap();
    // What onig_sys writes for LP64 Unix targets.
    fs::write(
        config.join("config.h"),
        "#define HAVE_PROTOTYPES 1\n#define STDC_HEADERS 1\n#define HAVE_STRING_H 1\n\
         #define HAVE_STDARG_H 1\n#define HAVE_STDLIB_H 1\n#define HAVE_LIMITS_H 1\n\
         #define HAVE_INTTYPES_H 1\n#define SIZEOF_INT 4\n#define SIZEOF_SHORT 2\n\
         #define SIZEOF_LONG 8\n#define SIZEOF_VOIDP 8\n#define SIZEOF_LONG_LONG 8\n",
    )
    .unwrap();
    let mut b = build();
    b.include(&config)
        .include(&src)
        .define("HAVE_UNISTD_H", "1")
        .define("HAVE_SYS_TYPES_H", "1")
        .define("HAVE_SYS_TIME_H", "1")
        .define("HAVE_ALLOCA_H", "1");
    for f in [
        "regexec.c",
        "regerror.c",
        "regparse.c",
        "regext.c",
        "regcomp.c",
        "reggnu.c",
        "regenc.c",
        "regsyntax.c",
        "regtrav.c",
        "regversion.c",
        "st.c",
        "onig_init.c",
        "unicode.c",
        "ascii.c",
        "utf8.c",
        "utf16_be.c",
        "utf16_le.c",
        "utf32_be.c",
        "utf32_le.c",
        "euc_jp.c",
        "sjis.c",
        "iso8859_1.c",
        "iso8859_2.c",
        "iso8859_3.c",
        "iso8859_4.c",
        "iso8859_5.c",
        "iso8859_6.c",
        "iso8859_7.c",
        "iso8859_8.c",
        "iso8859_9.c",
        "iso8859_10.c",
        "iso8859_11.c",
        "iso8859_13.c",
        "iso8859_14.c",
        "iso8859_15.c",
        "iso8859_16.c",
        "euc_tw.c",
        "euc_kr.c",
        "big5.c",
        "gb18030.c",
        "koi8_r.c",
        "cp1251.c",
        "euc_jp_prop.c",
        "sjis_prop.c",
        "unicode_unfold_key.c",
        "unicode_fold1_key.c",
        "unicode_fold2_key.c",
        "unicode_fold3_key.c",
    ] {
        b.file(src.join(f));
    }
    b.compile("koda_search_onig");
    src
}

fn build_pcre2(dir: &Path, out: &Path) -> PathBuf {
    let src = dir.join("src");
    // NON-AUTOTOOLS-BUILD: the generic config.h and pcre2.h, and the
    // default character tables.
    let gendir = out.join("pcre2");
    fs::create_dir_all(&gendir).unwrap();
    fs::copy(src.join("config.h.generic"), gendir.join("config.h")).unwrap();
    fs::copy(src.join("pcre2.h.generic"), gendir.join("pcre2.h")).unwrap();
    fs::copy(
        src.join("pcre2_chartables.c.dist"),
        gendir.join("pcre2_chartables.c"),
    )
    .unwrap();
    let mut b = build();
    b.include(&gendir)
        .include(&src)
        .define("PCRE2_CODE_UNIT_WIDTH", "8")
        .define("HAVE_CONFIG_H", "1")
        .define("PCRE2_STATIC", "1")
        .define("SUPPORT_UNICODE", "1")
        .define("SUPPORT_JIT", "1");
    for f in [
        "pcre2_auto_possess.c",
        "pcre2_chkdint.c",
        "pcre2_compile.c",
        "pcre2_compile_cgroup.c",
        "pcre2_compile_class.c",
        "pcre2_config.c",
        "pcre2_context.c",
        "pcre2_convert.c",
        "pcre2_dfa_match.c",
        "pcre2_error.c",
        "pcre2_extuni.c",
        "pcre2_find_bracket.c",
        "pcre2_jit_compile.c",
        "pcre2_maketables.c",
        "pcre2_match.c",
        "pcre2_match_data.c",
        "pcre2_match_next.c",
        "pcre2_newline.c",
        "pcre2_ord2utf.c",
        "pcre2_pattern_info.c",
        "pcre2_script_run.c",
        "pcre2_serialize.c",
        "pcre2_string_utils.c",
        "pcre2_study.c",
        "pcre2_substitute.c",
        "pcre2_substring.c",
        "pcre2_tables.c",
        "pcre2_ucd.c",
        "pcre2_valid_utf.c",
        "pcre2_xclass.c",
    ] {
        b.file(src.join(f));
    }
    b.file(gendir.join("pcre2_chartables.c"));
    b.compile("koda_search_pcre2");
    gendir
}

fn build_brotli(dir: &Path) -> PathBuf {
    let c = dir.join("c");
    let include = c.join("include");
    let mut b = build();
    b.include(&include);
    for sub in ["common", "dec"] {
        for f in c_files(&c.join(sub), "c") {
            b.file(f);
        }
    }
    b.compile("koda_search_brotli");
    include
}

/// The include folders a `-sys` crate passes to dependents (`DEP_<links>_INCLUDE`).
fn dep_includes(links: &str) -> Vec<PathBuf> {
    env::var(format!("DEP_{links}_INCLUDE"))
        .map(|v| {
            v.split([',', ';'])
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

fn build_ugrep(
    dir: &Path,
    manifest: &Path,
    out: &Path,
    os: &str,
    pcre2_include: &Path,
    brotli_include: &Path,
) {
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let target = env::var("TARGET").unwrap();
    let gendir = out.join("ugrep");
    fs::create_dir_all(&gendir).unwrap();
    fs::write(gendir.join("config.h"), ugrep_config(os)).unwrap();

    // ugrep's own SIMD choice (configure.ac): NEON on aarch64; SSE2 with
    // AVX2 and AVX512BW kernels picked at run time on x86_64.
    let (simd, avx2, avx512): (&[&str], &[&str], &[&str]) = match arch.as_str() {
        "aarch64" => (&["-DHAVE_NEON"], &[], &[]),
        "x86_64" => (&["-msse2", "-DHAVE_AVX512BW"], &["-mavx2"], &["-mavx512bw"]),
        _ => (&[], &[], &[]),
    };

    let mut includes: Vec<PathBuf> = vec![
        gendir.clone(),
        dir.join("include"),
        dir.join("src"),
        dir.join("lzma").join("C"),
        pcre2_include.to_path_buf(),
        brotli_include.to_path_buf(),
    ];
    for links in ["Z", "BZIP2", "ZSTD"] {
        includes.extend(dep_includes(links));
    }

    let cxx = |extra: &[&str]| {
        let mut b = build();
        b.cpp(true).std("c++17").flag("-pthread");
        for i in &includes {
            b.include(i);
        }
        b.define("HAVE_CONFIG_H", None)
            .define("WITH_NO_INDENT", None)
            .define("WITH_COLOR", None)
            .define("PCRE2_STATIC", None)
            .define("PLATFORM", format!("\"{target}\"").as_str())
            .define("GREP_PATH", "\"/usr/local/share/ugrep/patterns\"");
        for f in simd.iter().chain(extra) {
            b.flag(f);
        }
        b
    };

    // RE/flex, ugrep's regex library (lib/Makefile.am), with the AVX
    // kernels in their own objects so only they use AVX instructions.
    let lib = dir.join("lib");
    let mut reflex = cxx(&[]);
    reflex.define("WITH_NO_CODEGEN", None);
    for f in [
        "convert.cpp",
        "debug.cpp",
        "error.cpp",
        "input.cpp",
        "matcher.cpp",
        "pattern.cpp",
        "posix.cpp",
        "simd.cpp",
        "unicode.cpp",
        "utf8.cpp",
        "block_scripts.cpp",
        "language_scripts.cpp",
        "letter_scripts.cpp",
        "letter_case.cpp",
        "composer.cpp",
    ] {
        reflex.file(lib.join(f));
    }
    reflex.compile("koda_search_reflex");
    let mut b = cxx(avx2);
    b.define("WITH_NO_CODEGEN", None)
        .file(lib.join("matcher_avx2.cpp"))
        .file(lib.join("simd_avx2.cpp"));
    b.compile("koda_search_reflex_avx2");
    let mut b = cxx(avx512);
    b.define("WITH_NO_CODEGEN", None)
        .file(lib.join("matcher_avx512bw.cpp"))
        .file(lib.join("simd_avx512bw.cpp"));
    b.compile("koda_search_reflex_avx512bw");

    // 7-Zip's decoder, as ugrep's lzma/C/Makefile.am builds it.
    let lz = dir.join("lzma").join("C");
    let mut b = build();
    b.include(&lz)
        .define("Z7_PPMD_SUPPORT", None)
        .define("Z7_EXTRACT_ONLY", None)
        .define("NDEBUG", None)
        .define("_REENTRANT", None)
        .define("_FILE_OFFSET_BITS", "64")
        .define("_LARGEFILE_SOURCE", None);
    for f in [
        "viizip.c",
        "7zAlloc.c",
        "7zArcIn.c",
        "7zBuf.c",
        "7zBuf2.c",
        "7zCrc.c",
        "7zCrcOpt.c",
        "7zDec.c",
        "7zFile.c",
        "7zStream.c",
        "Bcj2.c",
        "Bra.c",
        "Bra86.c",
        "BraIA64.c",
        "CpuArch.c",
        "Delta.c",
        "Lzma2Dec.c",
        "LzmaDec.c",
        "Ppmd7.c",
        "Ppmd7Dec.c",
    ] {
        b.file(lz.join(f));
    }
    b.compile("koda_search_viiz");

    // zopen.c, the one C file of ugrep (src/Makefile.am).
    let mut b = build();
    for i in &includes {
        b.include(i);
    }
    b.define("HAVE_CONFIG_H", None)
        .file(dir.join("src").join("zopen.c"));
    b.compile("koda_search_ugrep_c");

    // ugrep itself; `src/ugrep_main.cpp` includes ugrep.cpp with its
    // `main` renamed.
    let src = dir.join("src");
    let mut b = cxx(&[]);
    for f in [
        "cnf.cpp",
        "glob.cpp",
        "output.cpp",
        "query.cpp",
        "screen.cpp",
        "stats.cpp",
        "vkey.cpp",
    ] {
        b.file(src.join(f));
    }
    b.file(manifest.join("src").join("ugrep_main.cpp"));
    b.compile("koda_search_ugrep");
}

/// ugrep's `config.h` as its `./configure` writes it on each OS, with the
/// libraries Claude Code's ugrep has (PCRE2, zlib, bzip2, zstd, brotli and
/// 7-Zip).
fn ugrep_config(os: &str) -> String {
    let mut defs = vec![
        "HAVE_7ZIP",
        "HAVE_CXX11",
        "HAVE_DIRENT_H",
        "HAVE_GETPAGESIZE",
        "HAVE_INTTYPES_H",
        "HAVE_LIBBROTLI",
        "HAVE_LIBBZ2",
        "HAVE_LIBZ",
        "HAVE_LIBZSTD",
        "HAVE_MMAP",
        "HAVE_PCRE2",
        "HAVE_PTHREAD",
        "HAVE_PTHREAD_PRIO_INHERIT",
        "HAVE_SCHED_H",
        "HAVE_STATFS",
        "HAVE_STATVFS",
        "HAVE_STDINT_H",
        "HAVE_STDIO_H",
        "HAVE_STDLIB_H",
        "HAVE_STRINGS_H",
        "HAVE_STRING_H",
        "HAVE_STRNCASECMP",
        "HAVE_STRUCT_DIRENT_D_INO",
        "HAVE_STRUCT_DIRENT_D_TYPE",
        "HAVE_SYS_MOUNT_H",
        "HAVE_SYS_PARAM_H",
        "HAVE_SYS_RESOURCE_H",
        "HAVE_SYS_STATVFS_H",
        "HAVE_SYS_STAT_H",
        "HAVE_SYS_TIME_H",
        "HAVE_SYS_TYPES_H",
        "HAVE_UNISTD_H",
        "STDC_HEADERS",
    ];
    if os == "macos" {
        defs.extend([
            "HAVE_F_RDAHEAD",
            "HAVE_PTHREAD_SET_QOS_CLASS_SELF_NP",
            "HAVE_STRUCT_STAT_ST_ATIMESPEC",
            "HAVE_STRUCT_STAT_ST_CTIMESPEC",
            "HAVE_STRUCT_STAT_ST_MTIMESPEC",
        ]);
    } else {
        defs.extend([
            "HAVE_O_NOATIME",
            "HAVE_SCHED_SETAFFINITY",
            "HAVE_STRUCT_STAT_ST_ATIM",
            "HAVE_STRUCT_STAT_ST_CTIM",
            "HAVE_STRUCT_STAT_ST_MTIM",
        ]);
    }
    let mut s = String::from("/* Written by koda-search's build.rs */\n");
    for d in defs {
        writeln!(s, "#define {d} 1").unwrap();
    }
    for (k, v) in [
        ("PACKAGE", "ugrep"),
        (
            "PACKAGE_BUGREPORT",
            "https://github.com/Genivia/ugrep/issues",
        ),
        ("PACKAGE_NAME", "ugrep"),
        ("PACKAGE_TARNAME", "ugrep"),
        ("PACKAGE_URL", "https://ugrep.com"),
        ("PACKAGE_VERSION", UGREP_VERSION),
        ("VERSION", UGREP_VERSION),
    ] {
        writeln!(s, "#define {k} \"{v}\"").unwrap();
    }
    writeln!(s, "#define PACKAGE_STRING \"ugrep {UGREP_VERSION}\"").unwrap();
    s
}

fn build_bfs(dir: &Path, manifest: &Path, out: &Path, os: &str, onig_include: &Path) {
    let src = dir.join("src");
    let gendir = out.join("bfs");
    fs::create_dir_all(&gendir).unwrap();
    let probe_dir = out.join("bfs-probes");
    fs::create_dir_all(&probe_dir).unwrap();

    // Not `build()`: its `-w` would hide the availability error below.
    let compiler = cc::Build::new().get_compiler();
    let mut base: Vec<String> = vec![
        "-pthread".into(),
        format!("-I{}", src.display()),
        format!("-I{}", gendir.display()),
        "-include".into(),
        src.join("prelude.h").display().to_string(),
    ];
    if os == "macos" {
        // An API newer than the deployment target counts as missing.
        base.push("-Werror=unguarded-availability-new".into());
    }
    let libs: &[&str] = if os == "linux" { &["-lrt"] } else { &[] };

    let try_cc = |file: &Path, flags: &[String], tag: &str| -> bool {
        let mut cmd = Command::new(compiler.path());
        cmd.args(compiler.args())
            .args(&base)
            .args(flags)
            .arg(file)
            .arg("-o")
            .arg(probe_dir.join(tag))
            .args(libs);
        cmd.output().is_ok_and(|o| o.status.success())
    };

    // build/flags/std.c: the newest C standard the compiler has.
    let std = ["-std=c23", "-std=c2x", "-std=c17"]
        .into_iter()
        .find(|f| try_cc(&dir.join("build/flags/std.c"), &[f.to_string()], "std"))
        .expect("no C17 compiler for bfs");

    // build/has/*.c, each defining BFS_HAS_<NAME> as `./configure` does.
    let mut config = String::from(
        "// Written by koda-search's build.rs\n#ifndef BFS_CONFIG_H\n#define BFS_CONFIG_H\n",
    );
    for pkg in ["libacl", "libcap", "libselinux", "liburing", "oniguruma"] {
        let on = pkg == "oniguruma";
        writeln!(config, "#define {} {on}", macro_name("with", pkg)).unwrap();
    }
    let mut probes = c_files(&dir.join("build/has"), "c");
    probes.sort();
    for probe in probes {
        let slug = probe.file_stem().unwrap().to_string_lossy().into_owned();
        let groups = probe_flag_groups(&fs::read_to_string(&probe).unwrap());
        // `fdclosedir` (macOS 26.4) passes its probe behind a run-time
        // `__builtin_available`, which needs `__isPlatformVersionAtLeast`;
        // `koda`'s LTO link doesn't have it. Claude Code's build leaves
        // `fdclosedir` out on macOS, and so does this one: bfs then closes
        // directories as on every macOS before 26.4.
        let skip = os == "macos" && slug == "fdclosedir";
        let ok = !skip
            && groups.iter().any(|g| {
                let mut flags = vec![std.to_string()];
                flags.extend(g.iter().cloned());
                try_cc(&probe, &flags, &slug)
            });
        writeln!(config, "#define {} {ok}", macro_name("has", &slug)).unwrap();
    }
    config.push_str("#endif // BFS_CONFIG_H\n");
    fs::write(gendir.join("config.h"), config).unwrap();

    // gen/*.i: the strings `bfs -version` prints, embedded as bytes.
    let cc_name = compiler.path().display().to_string();
    for (name, text) in [
        ("version.i", BFS_VERSION),
        ("confflags.i", BFS_CONFFLAGS),
        ("cc.i", cc_name.as_str()),
        ("cppflags.i", "-include src/prelude.h -DNDEBUG"),
        ("cflags.i", std),
        ("ar.i", ""),
        ("arflags.i", ""),
        ("ldflags.i", ""),
        ("ldlibs.i", "-lonig"),
    ] {
        fs::write(gendir.join(name), embed(text)).unwrap();
    }

    let mut b = build();
    b.include(&src)
        .include(&gendir)
        .include(onig_include)
        .flag("-include")
        .flag(src.join("prelude.h").display().to_string())
        .flag(std)
        .flag("-pthread")
        .define("NDEBUG", None);
    for f in c_files(&src, "c") {
        if f.file_name().is_some_and(|n| n != "main.c") {
            b.file(f);
        }
    }
    b.file(manifest.join("src").join("bfs_main.c"));
    b.compile("koda_search_bfs");
}

/// `BFS_<KIND>_<NAME>`, as build/define-if.sh spells it.
fn macro_name(kind: &str, slug: &str) -> String {
    format!("BFS_{kind}/{slug}")
        .chars()
        .map(|c| match c {
            '/' | '-' => '_',
            c => c.to_ascii_uppercase(),
        })
        .collect()
}

/// The alternative flag sets a probe asks for with `/// ` lines (groups
/// split by `/// ---`), as build/flags-if.sh reads them; one empty set if
/// it asks for none.
fn probe_flag_groups(source: &str) -> Vec<Vec<String>> {
    let mut groups = vec![Vec::new()];
    for line in source.lines() {
        let Some(line) = line.strip_prefix("/// ") else {
            continue;
        };
        if line == "---" {
            groups.push(Vec::new());
        } else if let Some((_, flag)) = line.split_once("= ") {
            groups.last_mut().unwrap().push(flag.to_string());
        } else {
            groups.last_mut().unwrap().push(line.to_string());
        }
    }
    groups
}

/// `text` as a NUL-terminated C array body, as build/embed.sh writes it.
fn embed(text: &str) -> String {
    let mut s = String::new();
    for b in text.bytes().chain([0]) {
        write!(s, "0x{b:02x},").unwrap();
    }
    s.push('\n');
    s
}

fn c_files(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == ext))
        .collect();
    v.sort();
    v
}
