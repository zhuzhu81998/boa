# Baseline JIT stencil build

LLVM 22 development libraries and the LLVM 22 Clang driver are required. The stencil and final
engine must use the same checkout, Rust toolchain, target, features, panic mode, optimization level,
and codegen settings. Rust externals are preserved and resolved by their exact mangled names.

The current first version uses an optimized native template for stencil bytes and a separately
emitted linker-plugin-LTO LLVM module for exact type and linkage validation.

Generate the native template:

```sh
CARGO_TARGET_DIR=/tmp/boa-jit-template-native \
LLVM_SYS_221_PREFIX=/usr/lib/llvm-22 \
BOA_JIT_BUILD_TEMPLATE=1 \
RUSTFLAGS="-Cpanic=abort -Clink-dead-code" \
  cargo rustc --release -p boa_engine --features jit --lib -- \
  --emit=llvm-ir,link -Ccodegen-units=1
```

Generate the LLVM module with the final linkers LTO mode. This module is used for exact symbol,
type, and linkage validation; its rlib is not used as the stencil archive.

```sh
CARGO_TARGET_DIR=/tmp/boa-jit-template-plugin-ir \
LLVM_SYS_221_PREFIX=/usr/lib/llvm-22 \
BOA_JIT_BUILD_TEMPLATE=1 \
CARGO_PROFILE_RELEASE_LTO=off \
RUSTFLAGS="-Cpanic=abort -Clink-dead-code -Clinker-plugin-lto" \
  cargo rustc --release -p boa_engine --features jit --lib -- \
  --emit=llvm-ir -Ccodegen-units=1
```

Build or run Boa with linker-plugin LTO. The release profiles rustc-side fat LTO is disabled here
because LTO is instead performed by LLVMs native linker plugin, allowing the generated resolver
bitcode to participate before symbol internalization.

```sh
CARGO_TARGET_DIR=/tmp/boa-jit-runtime \
LLVM_SYS_221_PREFIX=/usr/lib/llvm-22 \
BOA_JIT_TEMPLATE_ARCHIVE=/tmp/boa-jit-template-native/release/deps/libboa_engine.rlib \
BOA_JIT_TEMPLATE_LLVM_IR=/tmp/boa-jit-template-plugin-ir/release/deps/boa_engine.ll \
CARGO_PROFILE_RELEASE_LTO=off \
RUSTFLAGS="-Cpanic=abort -Clink-dead-code -Clinker-plugin-lto \
  -Clinker=/usr/lib/llvm-22/bin/clang -Clink-arg=-fuse-ld=lld" \
  cargo build --release -p boa_engine --features jit
```

Set `BOA_JIT_TRACE=1` to print every stencil execution. `BOA_JIT_REUSE_GENERATED=1` skips stencil
regeneration only when all generated files already exist in the current Cargo output directory; it
is intended for local iteration.

The handler table is the authoritative opcode mapping. Validation is performed independently per
opcode. Unsupported sections, relocations, or symbols disable only that opcode, and runtime dispatch
falls back to its interpreter handler. Supported emitters copy actual optimized handler bytes and
apply only internal code/data and exact typed external relocations.
