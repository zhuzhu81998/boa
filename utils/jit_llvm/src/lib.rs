//! Link llvm-sys consumers to Rust's bundled LLVM and locate its preview tools.
mod stencils;

pub use stencils::lower as lower_stencils;

use std::{env, path::PathBuf};

/// Resolve an explicit override, a system C compiler driver, or a bundled LLVM tool.
pub fn tool(variable: &str, name: &str) -> Result<PathBuf, String> {
    if let Some(path) = env::var_os(variable) {
        return Ok(PathBuf::from(path));
    }
    // The C driver forwards native linker arguments to bundled LLD; it does not parse IR.
    if name == "cc" {
        return Ok(PathBuf::from("cc"));
    }
    let path = if name == "ld.lld" {
        PathBuf::from(env!("BOA_JIT_LLD"))
    } else {
        PathBuf::from(env!("BOA_JIT_LLVM_BINDIR")).join(name)
    };
    if !path.is_file() {
        return Err(format!(
            "missing bundled LLVM tool {}; run rustup component add llvm-tools-preview",
            path.display()
        ));
    }
    Ok(path)
}

/// Check required tools and embed library lookup for executables run outside Cargo.
pub fn configure() -> Result<(), String> {
    for variable in ["LLVM_LLC", "LLVM_AR", "BOA_JIT_REAL_LINKER", "BOA_JIT_LLD"] {
        println!("cargo::rerun-if-env-changed={variable}");
    }
    for (variable, name) in [("LLVM_LLC", "llc"), ("LLVM_AR", "llvm-ar")] {
        tool(variable, name)?;
    }
    // Cargo loads build scripts from the staged library directory. A standalone
    // linker wrapper also needs its own runtime search path into the Rust sysroot.
    println!(
        "cargo::rustc-link-arg=-Wl,-rpath,{}",
        env!("BOA_JIT_LLVM_LIBDIR")
    );
    Ok(())
}
