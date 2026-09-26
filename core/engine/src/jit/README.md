# Baseline JIT stencil build

For the existing Boa CLI, use the automated build-and-run driver from the repository root:

```sh
./tools/jit-run.sh simple-loop.js
```

It builds the linker wrapper, captures template IR using the CLI's actual dependency features,
and rebuilds the CLI with the JIT. Unchanged builds are reused. Both build passes enable normal
CLI default features, including the fast allocator (jemalloc on x86_64 GNU/Linux), bundled
internationalization, and fetch support.
The driver inherits the normal release optimization level, fat LTO, codegen units, and symbol
stripping. After compiling the engine, its compiler wrapper inserts the runtime address table
into the engine archive, before rustc's CLI fat-LTO pass can remove private helpers. The final
link reuses that prepared table. Named struct declarations may resolve to Rust's byte-array
storage definitions only with matching size, sufficient alignment, and matching address space;
TLS and unrelated type mismatches remain rejected. Function signatures remain strictly checked.
The driver does not enable `link-dead-code`. The remaining JIT-specific settings
are linker-plugin bitcode/the linker wrapper and `panic=abort`: unwinding through generated
frames is not supported yet. Thus these builds are comparable, but not compiler-flag-identical
to the ordinary CLI.
LLVM 22 must be installed. The driver uses Git to enumerate build inputs; ripgrep is not required.
For build-only use `./tools/jit-run.sh --build-only`.
The resulting executable is `target/jit-cli/release/boa`; do not confuse it with an older
interpreter-only `target/release/boa`. Set `BOA_JIT_TRACE=1` to see native-chain entries.

Other workspace binaries use the same driver, with their own default dependency features:

```sh
./tools/jit-run.sh -p boa_tester -- run --suite test/language/expressions -v
./tools/jit-run.sh --build-only -p boa_tester
# For packages whose binary name differs, add --bin BINARY.
```

Use `--` to separate driver options from program arguments. Non-default targets are built in
`target/jit-packages/PACKAGE/BINARY`, keeping their templates separate from the CLI template.
For example, the tester executable is
`target/jit-packages/boa_tester/boa_tester/release/boa_tester`.
Set `BOA_JIT_DISABLE=1` to compare interpreter execution using that same binary.
The tester also uses `panic=abort`: a Rust panic terminates the process instead of being caught
as an individual test failure. JavaScript exceptions are unaffected.

Compare both execution modes with the same executable (build before timing):

```sh
./tools/jit-run.sh --build-only
time taskset -c 1 target/jit-cli/release/boa navier-stokes.js
time BOA_JIT_DISABLE=1 taskset -c 1 target/jit-cli/release/boa navier-stokes.js
```

`BOA_JIT_DISABLE` disables JIT execution when present, including when set to `0`.
Unset `BOA_JIT_TRACE` for timings. Both paths now share allocator, features, and build flags;
the interpreter path is not necessarily identical to a separately built default CLI.

The lower-level build steps below are for embedding and pipeline development.
They use late resolver insertion with rustc-side LTO disabled. Use the automated driver above
for fat LTO; it additionally prepares the engine archive before compiling the CLI.

LLVM 22 development libraries and tools are required. The template and final engine must use the
same checkout, Rust toolchain, target, features, panic mode, optimization level, and codegen
settings. Rust externals are preserved and resolved by their exact mangled names.

One linker-plugin-LTO LLVM module is the canonical input for both stencil code generation and exact
resolver type/linkage validation. Generating both artifacts from this module avoids trying to match
unstable private Rust symbol hashes across independent native and LLVM-IR compilations.

Build the pre-link wrapper first, before selecting it as the linker:

```sh
LLVM_SYS_221_PREFIX=/usr/lib/llvm-22 cargo build -p boa_jit_linker
```

Generate the canonical LLVM module:

```sh
CARGO_TARGET_DIR=/tmp/boa-jit-template-plugin-ir \
LLVM_SYS_221_PREFIX=/usr/lib/llvm-22 \
BOA_JIT_BUILD_TEMPLATE=1 \
CARGO_PROFILE_RELEASE_LTO=off \
RUSTFLAGS="-Cpanic=abort -Clinker-plugin-lto" \
  cargo rustc --release -p boa_engine --features jit --lib --crate-type rlib -- \
  --emit=llvm-ir=/tmp/boa-jit-template.ll -Ccodegen-units=1
```

Build or run Boa with linker-plugin LTO. Disable rustc-side fat LTO so
linker-plugin mode leaves unmerged bitcode for LLVM's native linker. The pre-link wrapper inserts
the helper address table into the runtime module before that link. Replace the wrapper path below
with the absolute path to your checkout.

```sh
CARGO_TARGET_DIR=/tmp/boa-jit-runtime \
LLVM_SYS_221_PREFIX=/usr/lib/llvm-22 \
BOA_JIT_TEMPLATE_LLVM_IR=/tmp/boa-jit-template.ll \
CARGO_PROFILE_RELEASE_LTO=off \
RUSTFLAGS="-Cpanic=abort -Clinker-plugin-lto \
  -Clinker=/absolute/path/to/boa/target/debug/boa_jit_linker -Clink-arg=-fuse-ld=lld \
  -Clink-arg=-Wl,--lto-O3" \
  cargo rustc --release -p boa_engine --features jit --lib --crate-type rlib
```

The build script uses LLVM 22's `llc` and `llvm-ar` to lower the canonical module into a
function-sectioned native archive for extraction. Set `LLVM_LLC` or `LLVM_AR` to override their
default paths under `/usr/lib/llvm-22/bin`.

The rlib command builds the engine, not an executable. Final executable links must also use the
wrapper and matching dependency features. When manually linking a smoke test, select the exact
hashed rlib reported by Cargo; a leftover un-hashed archive may contain a template-only engine.
Cargo examples/tests can unify additional dependency features and require a corresponding template.

The LLVM transformation keeps the handler bodies and replaces other function references with
numbered `BOA_JIT_HOLE_*` declarations. Their definitions, including private drop glue, stay in
the runtime. The generated resolver request maps those holes back to the original LLVM values;
the [pre-link wrapper](../../../../tools/jit-linker/README.md) puts their addresses in the runtime
table. A separate resolver link is insufficient for LLVM-internal targets, so use the wrapper.

All LLVM global addresses remain holes, including unnamed constants: vtables and other pointers
can escape a handler and must remain valid after its generated code is released.
Native per-instance data such as jump tables retain relocations. Direct LLVM TLS-address intrinsics
in handlers become accessor-function holes. The pre-link hook emits those small helpers against
the actual runtime TLS globals, so the current thread's address is obtained on every access.
Other unsupported TLS forms still disable the affected opcode.

Executable allocations use a best-effort nearby hint. Reachable helper branches are patched
directly; distant targets use local trampolines. The per-run cache retains callers and callees,
including failed compilation attempts, until that VM run ends. Separate/reentrant runs still
have separate caches.

Handlers use a uniform C ABI carrying the context, bytecode PC, chain state, and an
output slot. LLVM replaces the explicit continuation marker with `musttail`; a nontrivial cleanup
after that marker is rejected, never silently dropped. Fixed-size operands and the next PC are
patched into native immediates, rather than decoded on each execution. Variable-length operands
retain their bytecode decoder. Scalar holes preserve integer and floating-point bit patterns.
Each instruction owns its native jump-table/constant bundle, including relocations back into
its specialized handler; these cannot share another instruction's patched operands.
If the frame is unchanged and execution continues at the ordinary successor PC, a patched direct
tail jump avoids the native address-table lookup. Other transfers still use the address table;
this is not fallthrough stitching. Missing successors use the existing driver fallback.
On frame changes, handlers select the next code block from an immutable, sorted snapshot of the
cache and tail-transfer to its entry. The active block and frame depth are updated in the chain
state. Cache misses, missing entries, and final completions return to the driver. Boa's existing
completion conversion handles JavaScript exceptions before continuation is considered.
Tracing, fuzz instruction limits, and budgeted execution use the interpreter path.

The chain ABI changes the template configuration fingerprint. Regenerate the canonical template
when updating from the old single-instruction ABI; old generated stencils cannot be reused.

`BOA_JIT_REUSE_GENERATED=1` skips stencil regeneration when the generated files exist and the
canonical input, configuration, and generator-source fingerprint match. It is intended for local
iteration. `jit_runtime_holes.txt` in the engine build output lists the requested addresses in order.
`BOA_JIT_TRACE=1` reports compilation counts, failures, native-chain entries, and per-run native
and interpreter opcode counts. `native_frame_transfers` counts frame changes continued without
returning to the driver. Calls and returns between cached blocks should not need new chain entries.

The handler table is the authoritative opcode mapping. Validation is performed independently per
opcode. Unsupported sections, relocations, or symbols disable only that opcode, and runtime dispatch
falls back to its interpreter handler. Supported emitters copy actual optimized handler bytes and
apply only internal code/data and exact typed external relocations.
