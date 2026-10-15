//! Builds Jim Tcl from the upstream release without running its `configure`.
//!
//! Everything `configure`/`make` would generate is written to `OUT_DIR`: the two
//! config headers, the static extension table and the Tcl extensions converted
//! to C. The configuration mirrors OpenOCD's MSYS2/MinGW build of jimtcl
//! (`--with-ext=json --without-ext="binary ensemble tclprefix zlib"
//! --disable-utf8 --disable-lineedit --disable-ssl`).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

const JIM_VERSION: &str = "0.83";
/// SHA-256 of `https://github.com/msteveb/jimtcl/archive/refs/tags/<JIM_VERSION>.tar.gz`.
const JIM_SHA256: &str = "6f2df00009f5ac4ad654c1ae1d2f8ed18191de38d1f5a88a54ea99cc16936686";

/// Extensions implemented in C (`jim-<name>.c`).
const C_EXTS: &[&str] = &[
    "aio", "array", "clock", "eventloop", "exec", "file", "history", "interp", "json", "load",
    "namespace", "pack", "package", "readdir", "regexp",
];
/// Extensions implemented in Tcl (`<name>.tcl`, compiled in as `_<name>.c`).
const TCL_EXTS: &[&str] = &["glob", "jsonencode", "nshelper", "oo", "stdlib", "tclcompat", "tree"];

/// Interpreter core (what jimtcl's Makefile always builds).
const CORE_SOURCES: &[&str] = &[
    "jim-subcmd.c",
    "jim-interactive.c",
    "jim-format.c",
    "jim.c",
    "utf8.c",
    "jimregexp.c",
    "jimiocompat.c",
    "jim-win32compat.c",
    "jim-nosignal.c",
    "jsmn/jsmn.c",
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=JIMTCL_SRC_DIR");
    println!("cargo:rustc-env=JIM_VERSION={JIM_VERSION}");

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let src = source_dir(&out);
    let gen = out.join("gen");
    fs::create_dir_all(&gen).unwrap();

    let target = env::var("TARGET").unwrap();
    let windows_gnu = env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu");
    if !windows_gnu {
        panic!("jimtcl: no Jim Tcl configuration for target {target} (only Windows with MinGW is supported)");
    }
    write_config_headers(&gen, AUTOCONF_MINGW);
    write_load_static_exts(&gen);
    for ext in TCL_EXTS {
        write_tcl_ext(&src, &gen, ext);
    }

    let mut b = cc::Build::new();
    b.include(&gen)
        .include(&src)
        .define("_WIN32_WINNT", "0x0601")
        .flag_if_supported("-Wno-deprecated-declarations")
        .flag_if_supported("-fno-unwind-tables")
        .flag_if_supported("-fno-asynchronous-unwind-tables")
        .opt_level(2)
        .warnings(false);
    for f in CORE_SOURCES {
        b.file(src.join(f));
    }
    for ext in C_EXTS {
        b.file(src.join(format!("jim-{ext}.c")));
    }
    b.file(gen.join("_load-static-exts.c"));
    for ext in TCL_EXTS {
        b.file(gen.join(format!("_{ext}.c")));
    }
    b.compile("jimtcl");

    // For crates compiling against jim.h (DEP_JIMTCL_INCLUDE).
    println!("cargo:include={}", env::join_paths([&gen, &src]).unwrap().to_str().unwrap());
}

/// Returns the unpacked jimtcl source tree: `JIMTCL_SRC_DIR`, or the release
/// tarball downloaded and unpacked into `OUT_DIR`.
fn source_dir(out: &Path) -> PathBuf {
    if let Some(dir) = env::var_os("JIMTCL_SRC_DIR") {
        let dir = PathBuf::from(dir);
        assert!(dir.join("jim.h").is_file(), "jimtcl: JIMTCL_SRC_DIR={} has no jim.h", dir.display());
        println!("cargo:rerun-if-changed={}", dir.display());
        return dir;
    }

    let dir = out.join(format!("jimtcl-{JIM_VERSION}"));
    // Holds the checksum of the archive the tree was unpacked from.
    let stamp = dir.join(".unpacked");
    if fs::read_to_string(&stamp).is_ok_and(|s| s == JIM_SHA256) {
        return dir;
    }

    let archive = format!("jimtcl-{JIM_VERSION}.tar.gz");
    let archive_path = out.join(&archive);
    if !archive_path.is_file() || sha256_file(&archive_path) != JIM_SHA256 {
        let url = format!("https://github.com/msteveb/jimtcl/archive/refs/tags/{JIM_VERSION}.tar.gz");
        let part = format!("{archive}.part");
        // Relative paths: an MSYS tar/curl first on PATH would read "C:\..." as host:path.
        run(
            Command::new("curl").current_dir(out).args(["-sSfL", "--retry", "3", "-o", &part, &url]),
            &format!("downloading {url} (set JIMTCL_SRC_DIR to build from a local jimtcl {JIM_VERSION} tree)"),
        );
        let got = sha256_file(&out.join(&part));
        if got != JIM_SHA256 {
            panic!("jimtcl: checksum mismatch for {url}\n  expected {JIM_SHA256}\n  got      {got}");
        }
        fs::rename(out.join(&part), &archive_path).unwrap();
    }

    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    run(Command::new("tar").current_dir(out).args(["-xzf", &archive]), &format!("unpacking {archive}"));
    assert!(dir.join("jim.h").is_file(), "jimtcl: {archive} did not unpack to {}", dir.display());
    fs::write(&stamp, JIM_SHA256).unwrap();
    dir
}

fn run(cmd: &mut Command, what: &str) {
    match cmd.status() {
        Ok(s) if s.success() => {}
        Ok(s) => panic!("jimtcl: {what} failed ({s})"),
        Err(e) => panic!("jimtcl: {what} failed: could not run {:?}: {e}", cmd.get_program()),
    }
}

fn sha256_file(path: &Path) -> String {
    let data = fs::read(path).unwrap();
    Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect()
}

fn write_config_headers(gen: &Path, autoconf: &str) {
    let version = JIM_VERSION.replace('.', "").trim_start_matches('0').to_string();
    fs::write(
        gen.join("jim-config.h"),
        format!(
            "#ifndef _JIM_CONFIG_H\n#define _JIM_CONFIG_H\n#define HAVE_LONG_LONG 1\n\
             /* #undef JIM_UTF8 */\n#define JIM_VERSION {version}\n#define SIZEOF_INT 4\n#endif\n"
        ),
    )
    .unwrap();

    let mut exts: Vec<&str> = C_EXTS.iter().chain(TCL_EXTS).copied().collect();
    exts.sort();
    let mut h = String::from("#ifndef _JIMAUTOCONF_H\n#define _JIMAUTOCONF_H\n");
    h += autoconf;
    h += &format!("#define JIM_VERSION {version}\n");
    for ext in exts {
        h += &format!("#define jim_ext_{ext} 1\n");
    }
    h += "#endif\n";
    fs::write(gen.join("jimautoconf.h"), h).unwrap();
}

/// Port of jimtcl's make-load-static-exts.tcl.
fn write_load_static_exts(gen: &Path) {
    // Some extensions depend on others being loaded first.
    let priority = |ext: &str| match ext {
        "stdlib" => 0,
        "glob" | "tree" | "binary" => 2,
        _ => 1,
    };
    let mut exts: Vec<(u32, &str)> = C_EXTS.iter().chain(TCL_EXTS).map(|e| (priority(e), *e)).collect();
    exts.sort();

    let mut c = String::from(
        "\n/* autogenerated - do not edit */\n#include \"jim.h\"\n#include \"jimautoconf.h\"\n\
         int Jim_InitStaticExtensions(Jim_Interp *interp)\n\n{\n",
    );
    for (_, ext) in &exts {
        c += &format!("\textern int Jim_{ext}Init(Jim_Interp *);\n");
    }
    for (_, ext) in &exts {
        c += &format!("\tJim_{ext}Init(interp);\n");
    }
    c += "\treturn JIM_OK;\n}\n";
    fs::write(gen.join("_load-static-exts.c"), c).unwrap();
}

/// Port of jimtcl's make-c-ext.tcl: wraps `<ext>.tcl` in a C init function.
/// Comment lines are blanked, not removed, so line numbers in errors still match.
fn write_tcl_ext(src: &Path, gen: &Path, ext: &str) {
    let path = src.join(format!("{ext}.tcl"));
    let text = String::from_utf8(fs::read(&path).unwrap()).unwrap();
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let body: Vec<String> = lines
        .iter()
        .map(|line| {
            let line = if line.trim_start_matches([' ', '\t']).starts_with('#') { "" } else { line };
            let escaped = line.replace('\\', "\\\\").replace('"', "\\\"").replace('\r', "");
            format!("\"{escaped}\\n\"")
        })
        .collect();

    let c = format!(
        "/* autogenerated - do not edit */\n#include <jim.h>\nint Jim_{ext}Init(Jim_Interp *interp)\n{{\n\
         \tif (Jim_PackageProvide(interp, \"{ext}\", \"1.0\", JIM_ERRMSG)) return JIM_ERR;\n\
         \treturn Jim_EvalSource(interp, \"{ext}.tcl\", 1, {});\n}}\n",
        body.join("\n")
    );
    fs::write(gen.join(format!("_{ext}.c")), c).unwrap();
}

/// jimautoconf.h as produced by jimtcl's configure under MSYS2/MinGW (taken from
/// OpenOCD's build), minus the version and extension defines added above.
const AUTOCONF_MINGW: &str = "\
#define HAVE_CFLAG_FNO_ASYNCHRONOUS_UNWIND_TABLES 1
#define HAVE_CFLAG_FNO_UNWIND_TABLES 1
#define HAVE_DECL_ISINF 1
#define HAVE_DECL_ISNAN 1
#define HAVE_DECL_S_IRWXG 1
#define HAVE_DECL_S_IRWXO 1
#define HAVE_DECL_S_IXUSR 1
#define HAVE_DIRENT_H 1
#define HAVE_DLOPEN_COMPAT 1
#define HAVE_DUP 1
#define HAVE_EXECVPE 1
#define HAVE_FCNTL_H 1
#define HAVE_FSTAT64 1
#define HAVE_GMTIME 1
#define HAVE_ISASCII 1
#define HAVE_ISATTY 1
#define HAVE_LFS 1
#define HAVE_LOCALTIME 1
#define HAVE_LONG_LONG 1
#define HAVE_LSEEK64 1
#define HAVE_MATH_H 1
#define HAVE_MKDIR_ONE_ARG 1
#define HAVE_MKSTEMP 1
#define HAVE_OPENDIR 1
#define HAVE_RESTRICT 1
#define HAVE_SIN 1
#define HAVE_SLEEP 1
#define HAVE_STAT64 1
#define HAVE_STDLIB_H 1
#define HAVE_STRINGS_H 1
#define HAVE_SYSTEM 1
#define HAVE_SYS_STAT_H 1
#define HAVE_SYS_TIME_H 1
#define HAVE_SYS_TYPES_H 1
#define HAVE_TIME_H 1
#define HAVE_UMASK 1
#define HAVE_UNISTD_H 1
#define HAVE_USLEEP 1
#define HAVE_WINCONSOLE 1
#define HAVE_WINDOWS 1
#define HAVE__FULLPATH 1
#define JIM_DOCS 1
#define JIM_IPV6 1
#define JIM_MATH_FUNCTIONS 1
#define JIM_REFERENCES 1
#define JIM_REGEXP 1
#define JIM_STATICLIB 1
#define SIZEOF_INT 4
#define SIZEOF_OFF_T 8
#define SIZEOF_TIME_T 8
#define TCL_LIBRARY \"/usr/local/lib/jim\"
#define TCL_PLATFORM_OS \"mingw\"
#define TCL_PLATFORM_PATH_SEPARATOR \";\"
#define TCL_PLATFORM_PLATFORM \"windows\"
#define _FILE_OFFSET_BITS 64
";
