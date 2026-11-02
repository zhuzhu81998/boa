# GSoC 2026 Report: Copy-and-Patch JIT for Boa

- **Contributor:** zhuzhu
- **Project Repository:** [boa-dev/boa](https://github.com/boa-dev/boa)
- **Implementation Branch:** [zhuzhu/cp-jit-v2](https://github.com/zhuzhu81998/boa/tree/zhuzhu/cp-jit-v2)
- **Revision Covered:** [`2121ac38`](https://github.com/zhuzhu81998/boa/commit/2121ac386e6748dee78476aaa2e3a882dc6bd3e6)

## Executive Summary

I worked on an experimental copy-and-patch baseline JIT for Boa, the JavaScript engine written in Rust. This is a proof of concept intended to demonstrate the feasibility of copy-and-patch compilation in Boa, not a production-ready JIT. One of the goals is to reduce interpreter dispatch and repeated operand decoding while reusing Boa's existing opcode implementations.

The implementation extracts native code templates, called stencils, from LLVM-compiled handlers. At runtime, it copies these stencils, patches instruction operands and addresses, and connects them into native execution chains. The interpreter remains available when a native entry is unavailable. In particular, no runtime information is used due to lack of infrastructure. The JIT path is taken whenever available.

The main contribution is establishing which approach works for Boa's Rust implementation and demonstrating its feasibility through conformance testing and the `simple-loop.js` performance results. The exact code is exploratory and will likely need substantial reworking. The current implementation supports **x86-64 Linux with ELF only**; substantial work remains before it can become a production implementation.

## Technical Implementation Details

### Initial Research and Toy Project Experiments

I started with a small, controlled toy project before integrating anything into Boa. The experiment involved copying and connecting a few native stencils and patching their references to see whether the approach worked. This kept the initial investigation separate from the complexity of JavaScript execution and garbage collection.

I investigated LuaJIT, CPython, Rust copy-and-patch prototypes, other Rust JIT approaches, and Cranelift as an alternative to LLVM. Much of the work involved understanding these different designs and which assumptions could carry over to Boa.

A Rust copy-and-patch example is [Patchouly](https://github.com/gudzpoz/patchouly). Its stencil extractor rejects references to functions or data defined in the object, while allowing explicitly supplied function pointers and allowlisted runtime symbols. Boa needs automatic symbol handling as much as possible, since opcode handlers depend on many runtime helpers, including compiler-generated drop glue associated with garbage-collected values. Those helpers may be private functions in the same executable, making manual registration impractical. The toy experiments established the basic feasibility of stencil composition, while the broader investigation identified automatic runtime dependency resolution as an essential requirement. These findings led to the LLVM IR approach described below.

### Phase 1: Stencil Generation and Runtime Linking

**CPython Specialties:** I investigated [CPython's copy-and-patch JIT](https://peps.python.org/pep-0744/) as a reference. Its build pipeline compiles generated C micro-op templates with Clang, extracts native code and relocations, and generates C emitters and a runtime symbol-address table in C. See CPython's  [JIT tool](https://github.com/python/cpython/blob/main/Tools/jit).

**Adapting the Design to Rust:** CPython's C implementation can refer to runtime helpers through C declarations and linkable names in C, without Rust's mangled names and more. C still has linkage and visibility constraints, and CPython explicitly handles platform symbol conventions and relocations. Boa adds the challenge of preserving access to compiler-generated private functions, drop glue, and globals that cannot simply be named from separately generated Rust source.

I therefore moved stencil preparation and runtime-symbol resolution into **LLVM IR**, where function types, calling conventions, and internal definitions are available directly. The pipeline replaces dependencies with numbered holes and inserts their address table into the actual runtime module before linking can discard private definitions. This preserves exact mangled identities rather than reconstructing names or making every helper public. Native object extraction and runtime machine-code patching remain later stages. LLVM is used during the build, not to compile JavaScript at runtime.

**Problem:** Extracted handlers reference private Rust helpers, constants, and thread-local data. Their addresses must resolve to the actual runtime definitions, and private symbol names cannot be reliably assumed to match across independent compilations.

**Core Work:**

- **Canonical LLVM Input:** Built stencil extraction and resolver requests from one matching LLVM module, with configuration fingerprints to detect incompatible templates.
- **Shallow Extraction:** Kept handler bodies while replacing external function and global references with patchable holes. Runtime globals, including escaping constants such as vtables, retain runtime-owned storage.
- **Pre-link Integration:** Added a [linker wrapper](https://github.com/zhuzhu81998/boa/tree/zhuzhu/cp-jit-v2/tools/jit-linker) that inserts an address table into the runtime module before linking, preserving access to private helpers.

### Phase 2: Native Execution and VM Integration

**Problem:** Native handlers need to continue across instructions, calls, and returns while preserving Boa's JavaScript behavior and allowing interpreter fallback.

**Core Work:**

- **Operand Patching:** Patched fixed-size operands and successor bytecode positions into native code, avoiding repeated decoding for those values.
- **Native Chaining:** Used LLVM tail transfers between handlers, with direct jumps to ordinary successors and address-table lookup for other transfers. Cached function bodies can continue across frame changes without returning to the driver.
- **Code Lifetime and Fallback:** Added a per-VM-run cache that retains compiled code and roots its code blocks. Unsupported opcodes use interpreter handlers.  Tracing, fuzz instruction limits, and budgeted execution retain interpreter execution.



## Conformance Results

The previous conformance run completed with **all tests (passed by main branch) passing**.

## Benchmark: `simple-loop.js`

The committed [`simple-loop.js`](https://github.com/zhuzhu81998/boa/blob/2121ac386e6748dee78476aaa2e3a882dc6bd3e6/simple-loop.js) runs two nested loops of 32,767 iterations, calling `addOne` **1,073,676,289 times**. Each timed run must print `1073676289`.

**Method:** Compare JIT execution with `BOA_JIT_DISABLE=1` in the same release executable, built through `tools/jit-run.sh --build-only` with normal CLI defaults, fat LTO, and `panic=abort`. Run three fresh-process trials per mode, alternating pair order and pinning execution to logical CPU 1. Measure wall-clock time with tracing disabled and no separate warmup. Timings include process startup and runtime JIT compilation; the build is excluded.

**Environment:** Intel Core Ultra 7, x86-64 Linux, Rust 1.99.0, LLVM 23.1.1.

**Results (31 October 2026):**

| Execution mode | Trial 1 | Trial 2 | Trial 3 | Median |
| --- | ---: | ---: | ---: | ---: |
| Baseline JIT | 117.77 s | 133.04 s | 123.68 s | **123.68 s** |
| Interpreter (`BOA_JIT_DISABLE=1`) | 156.74 s | 166.08 s | 163.49 s | **163.49 s** |

The ratio of median times is **1.32× in favor of the JIT**, equivalent to **24.4% less wall-clock time**. All six runs produced the expected output. These are three samples per mode on a development machine, with visible run-to-run variation.

To reproduce the two execution modes from the repository root:

```sh
./tools/jit-run.sh --build-only
time env -u BOA_JIT_TRACE -u BOA_JIT_DISABLE \
  taskset -c 1 target/jit-cli/release/boa simple-loop.js
time env -u BOA_JIT_TRACE BOA_JIT_DISABLE=1 \
  taskset -c 1 target/jit-cli/release/boa simple-loop.js
```

This microbenchmark exercises repeated loops and function calls. It provides evidence about this workload only; it does not establish general JavaScript performance, full correctness, or production readiness.

## Future Work & Extensions

- **Library Embedding and Build Integration:** Using the JIT through `boa_engine` as a dependency is currently very cumbersome. Enabling the `jit` Cargo feature alone is insufficient: an embedding application must generate a canonical template matching its engine configuration and integrate the compiler/linker wrappers into its final executable build. The Rust toolchain, target, dependency features, panic mode, and optimization settings must remain consistent.
- **Runtime Profiling and Adaptive Compilation:** Collect execution frequencies, value types, and other runtime information to identify hot code and guide specialization. The current baseline generates code at runtime but does not use profiling to make compilation decisions. Developing it into an adaptive, optimizing JIT requires this infrastructure.
- **Superstencils:** Combine multiple operations into larger stencils so the build-time compiler can optimize across instruction boundaries and remove intermediate work. Superstencils will be needed to pursue substantial performance gains beyond the current per-opcode baseline.
- **Coverage and Portability:** Only x86-64 Linux with ELF is currently supported. Supporting other operating systems, object formats, and architectures requires further implementation and validation.
- **Runtime Robustness and Reworking the Prototype:** Investigate unwinding through generated frames, which currently require `panic=abort`, and rework the exploratory implementation as needed. The durable outcome is the validated approach and the lessons from implementing it; the current code is a starting point for further development.
