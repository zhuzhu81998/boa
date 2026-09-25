# Experimental JIT pre-link hook

This tool inserts `BOA_JIT_EXTERNAL_SYMBOLS` into the actual Boa runtime LLVM module before
the ordinary native link. Entries refer directly to that module's values, including functions
with LLVM `internal` linkage. The separate resolver archive supplies the ordered address requests;
it is removed from the final link after the table has been inserted.

Build the tool before selecting it as a linker:

```sh
LLVM_SYS_221_PREFIX=/usr/lib/llvm-22 cargo build -p boa_jit_linker
```

Follow the engine JIT README to generate a matching canonical template and enable linker-plugin
LTO. In the final runtime build, replace `-Clinker=/usr/lib/llvm-22/bin/clang` with
`-Clinker=/absolute/path/to/boa/target/debug/boa_jit_linker`. Keep `-Clink-arg=-fuse-ld=lld`.
`BOA_JIT_REAL_LINKER` overrides the default `/usr/lib/llvm-22/bin/clang`; it must name the real
driver, not this wrapper. `LLVM_AR` overrides `/usr/lib/llvm-22/bin/llvm-ar`.

Links without `-lboa_jit_resolver` pass through to the real driver. For JIT links the wrapper
examines bitcode objects and archive members, finds the single module defining
`BOA_JIT_TEMPLATE_HANDLERS`, and writes a modified temporary object/archive. Cargo's input files
are never modified. The temporary files remain alive until the real linker exits and are then
removed. Each link gets its own temporary directory.

The current hook expects x86-64 Unix, linker-plugin bitcode, one module owning the handlers and
their private dependencies, and direct linker arguments. Response-file JIT links and private
targets in other modules are not supported. Missing symbols, mismatched types/calling conventions,
or multiple owning modules are errors. The template must still match the runtime compilation:
this stage deliberately does not guess matches between different Rust symbol hashes.
Anonymous immutable globals marked `unnamed_addr` are matched by LLVM initializer structure,
including exact named references, rather than unstable `alloc_*` labels. Their addresses always
refer to runtime storage: copying an escaping vtable into reclaimable JIT memory is unsafe.

Stencil bytes and ordered requests are generated before Rust compilation from a shallow copy of
the matching canonical template. Non-handler function references become numbered holes; only
handler code and eligible data are copied. Consequently there is no circular dependency between
linking and Rust `include!` metadata. The hook resolves those requests against the actual runtime
definitions. Stencil generation still requires a matching template build. Continuation markers
are lowered to LLVM `musttail` in the shallow stencil module before code generation; the pre-link
hook continues to resolve runtime addresses, not per-instruction dispatch.

Regression tests assemble LLVM fixtures, link and execute calls to internal functions through the
injected array, verify input archives remain unchanged, and reject a missing private target:

```sh
cargo test -p boa_jit_linker
```
