fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    boa_jit_llvm::configure().unwrap_or_else(|error| panic!("cannot configure JIT LLVM tools: {error}"));
}
