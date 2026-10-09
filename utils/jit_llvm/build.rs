//! Link LLVM before compiling the engine build script that calls its C API.
use std::{env, fs, path::PathBuf, process::Command};

fn rustc(argument: &str) -> String {
    let compiler = env::var_os("RUSTC").expect("Cargo must set RUSTC");
    let output = Command::new(compiler)
        .args(argument.split_whitespace())
        .output()
        .expect("cannot inspect Cargo's Rust compiler");
    assert!(output.status.success(), "rustc {argument} failed");
    String::from_utf8(output.stdout)
        .expect("rustc output is not UTF-8")
        .trim()
        .to_owned()
}

fn main() {
    println!("cargo::rerun-if-changed=../../Cargo.toml");
    println!("cargo::rerun-if-changed=../../rust-toolchain.toml");
    assert!(
        cfg!(target_os = "linux"),
        "bundled JIT LLVM linking currently supports Linux hosts"
    );
    assert_eq!(
        env::var("HOST").unwrap(),
        env::var("TARGET").unwrap(),
        "the JIT LLVM helper must be built for the compiler host"
    );
    let info = rustc("-vV");
    let field = |prefix| {
        info.lines()
            .find_map(|line| line.strip_prefix(prefix))
            .unwrap_or_else(|| panic!("rustc -vV did not report {prefix}"))
    };
    let release = field("release: ");
    let host = field("host: ");
    let llvm_version = field("LLVM version: ");
    let mut llvm_parts = llvm_version.split('.');
    let llvm_major: u32 = llvm_parts.next().unwrap().parse().unwrap();
    let llvm_minor: u32 = llvm_parts.next().unwrap().parse().unwrap();
    // llvm-sys encodes LLVM's major and minor as its package major (23.1 -> 231).
    let bindings: u32 = include_str!("../../Cargo.toml")
        .lines()
        .find_map(|line| line.strip_prefix("llvm-sys = { version = \""))
        .expect("workspace llvm-sys version is missing")
        .split('.')
        .next()
        .unwrap()
        .parse()
        .expect("invalid llvm-sys version");
    assert_eq!(
        bindings, llvm_major * 10 + llvm_minor,
        "rustc uses LLVM {llvm_version}; update the workspace llvm-sys bindings to match"
    );
    let sysroot = PathBuf::from(rustc("--print sysroot"));
    let libdir = sysroot.join("lib");
    let prefix = format!("libLLVM-{llvm_major}-rust-{release}");
    let mut libraries = fs::read_dir(&libdir)
        .expect("cannot read the Rust sysroot library directory")
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".so"))
        });
    let library_path = libraries.next().unwrap_or_else(|| {
        panic!(
            "cannot find rustc's bundled {prefix}*.so in {}",
            libdir.display()
        )
    });
    assert!(libraries.next().is_none(), "ambiguous bundled LLVM libraries");
    let library = library_path
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .strip_prefix("lib")
        .unwrap()
        .strip_suffix(".so")
        .unwrap();
    let bindir = libdir.join("rustlib").join(host).join("bin");
    let lld = bindir.join("gcc-ld/ld.lld");
    assert!(lld.is_file(), "missing bundled LLD: {}", lld.display());
    println!("cargo::rerun-if-changed={}", library_path.display());
    // Cargo drops native runtime search paths outside target/. Stage symlinks in
    // OUT_DIR so build-script executables can load LLVM before their main runs.
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    for entry in fs::read_dir(&libdir).expect("cannot read the Rust sysroot library directory") {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if !name.to_str().is_some_and(|name| name.starts_with("libLLVM")) {
            continue;
        }
        let destination = out.join(&name);
        let source = entry.path();
        if fs::read_link(&destination).ok().as_ref() == Some(&source) {
            continue;
        }
        if destination.symlink_metadata().is_ok() {
            fs::remove_file(&destination).expect("cannot replace staged LLVM library");
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&source, &destination)
            .expect("cannot stage bundled LLVM library");
    }
    println!("cargo::rustc-link-search=native={}", out.display());
    println!("cargo::rustc-link-lib=dylib={library}");
    println!("cargo::rustc-env=BOA_JIT_LLVM_LIBDIR={}", libdir.display());
    println!("cargo::rustc-env=BOA_JIT_LLVM_BINDIR={}", bindir.display());
    println!("cargo::rustc-env=BOA_JIT_LLD={}", lld.display());
}
