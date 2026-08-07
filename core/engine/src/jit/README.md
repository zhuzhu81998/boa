# Baseline JIT stencil build

LLVM 22 development libraries are required. Set `LLVM_SYS_221_PREFIX` when LLVM is not on the
normal `llvm-config` search path. The stencil and final engine must use the same checkout,
toolchain, features, profile, panic mode, optimization level, and other codegen flags because Rust
external symbols are retained by their exact mangled names.

Generate the template without adding a source-visible cfg (which would change symbol hashes):

```sh
LLVM_SYS_221_PREFIX=/usr/lib/llvm-22 \
BOA_JIT_BUILD_TEMPLATE=1 \
RUSTFLAGS='-Copt-level=0 -Cpanic=abort -Clink-dead-code' \
  cargo rustc --release -p boa_engine --features jit --lib -- \
  --crate-type=staticlib -Ccodegen-units=1 --emit=llvm-ir,link
```

Then build the engine with the paired artifacts and matching codegen flags:

```sh
LLVM_SYS_221_PREFIX=/usr/lib/llvm-22 \
BOA_JIT_TEMPLATE_ARCHIVE=target/release/deps/libboa_engine.a \
BOA_JIT_TEMPLATE_LLVM_IR=target/release/deps/boa_engine.ll \
RUSTFLAGS='-Copt-level=0 -Cpanic=abort -Clink-dead-code' \
  cargo build --release -p boa_engine --features jit
```

The archive handler table is the authoritative opcode mapping. The generator rejects missing
handlers, non-symbol relocations, unsupported relocation encodings, and external names that the
LLVM resolver cannot reconnect. Generated runtime emitters are derived from the archive; they do
not contain hand-written instruction bytes or forwarding handlers.
