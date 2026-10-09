//! Use llvm-sys bindings with Rust's bundled LLVM library and preview tools.
use std::path::PathBuf;

/// Configure tool availability and standalone executable library lookup.
pub(crate) fn configure() -> Result<(), String> {
    boa_jit_llvm::configure()
}

/// Resolve an explicit override or a tool from the pinned Rust distribution.
pub(crate) fn tool(variable: &str, name: &str) -> Result<PathBuf, String> {
    boa_jit_llvm::tool(variable, name)
}
