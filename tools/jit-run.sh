#!/usr/bin/env bash
# Build a matching template/runtime pair using the CLI's actual dependency graph.
set -euo pipefail

# Cargo invokes this same script as RUSTC_WORKSPACE_WRAPPER. Only the engine gets
# an additional IR output; its ordinary Cargo crate type/features remain unchanged.
if [[ ${BOA_JIT_DRIVER_COMPILER:-} == 1 ]]; then
    compiler=$1
    shift
    previous=
    engine=0
    for argument in "$@"; do
        if [[ $previous == --crate-name && $argument == boa_engine ]]; then
            engine=1
        fi
        previous=$argument
    done
    if [[ $engine == 1 && ${BOA_JIT_BUILD_TEMPLATE:-} == 1 ]]; then
        exec "$compiler" "$@" "--emit=llvm-ir=$BOA_JIT_DRIVER_IR"
    fi
    exec "$compiler" "$@"
fi

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
driver="$repo/tools/jit-run.sh"
build_only=0
if [[ ${1:-} == --build-only ]]; then
    build_only=1
    shift
fi
if [[ ${1:-} == --help ]]; then
    echo 'Usage: ./tools/jit-run.sh [--build-only] [Boa CLI arguments...]'
    echo 'Example: ./tools/jit-run.sh simple-loop.js'
    echo 'Builds the experimental JIT CLI without optional CLI default features.'
    exit 0
fi

build_dir="$repo/target/jit-cli"
tool_dir="$repo/target/jit-tools"
binary="$build_dir/release/boa"
template="$build_dir/boa_engine_template.ll"
stamp="$build_dir/source-fingerprint"
mkdir -p "$build_dir"
exec 9>"$build_dir/driver.lock"
flock 9

# Preserve the caller's directory for relative JavaScript file arguments.
fingerprint=$(
    cd "$repo"
    {
        rustc -vV
        git ls-files --cached --others --exclude-standard -z -- \
            core cli utils tools/jit-linker .cargo Cargo.toml Cargo.lock tools/jit-run.sh |
            while IFS= read -r -d '' file; do
                # Tracked files may have been deleted in the working tree.
                if [[ -f $file ]]; then
                    printf '%s\0' "$file"
                fi
            done |
            sort -z | xargs -0 sha256sum
    } | sha256sum
)

if [[ ! -x $binary || ! -f $template || ! -f $stamp || $(<"$stamp") != "$fingerprint" ]]; then
    # An interrupted template pass must never leave an older success stamp usable.
    : > "$stamp"
    (
        cd "$repo"
        # This driver owns its compilation settings; do not accidentally inherit a
        # template-only build, another wrapper, or flags that change Rust symbol identities.
        unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER
        unset BOA_JIT_BUILD_TEMPLATE BOA_JIT_TEMPLATE_LLVM_IR BOA_JIT_REUSE_GENERATED
        export LLVM_SYS_221_PREFIX=${LLVM_SYS_221_PREFIX:-/usr/lib/llvm-22}
        export CARGO_PROFILE_RELEASE_LTO=off
        export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1

        echo 'Building JIT linker wrapper...' >&2
        CARGO_TARGET_DIR="$tool_dir" cargo build -p boa_jit_linker

        export CARGO_TARGET_DIR="$build_dir"
        export RUSTC_WORKSPACE_WRAPPER="$driver"
        export BOA_JIT_DRIVER_COMPILER=1
        export BOA_JIT_DRIVER_IR="$template"
        flags=(-Cpanic=abort -Clink-dead-code -Clinker-plugin-lto
            "-Clinker=$tool_dir/debug/boa_jit_linker" -Clink-arg=-fuse-ld=lld)
        printf -v CARGO_ENCODED_RUSTFLAGS '%s\x1f' "${flags[@]}"
        export CARGO_ENCODED_RUSTFLAGS=${CARGO_ENCODED_RUSTFLAGS%$'\x1f'}

        echo 'Building canonical template with CLI dependency features...' >&2
        BOA_JIT_BUILD_TEMPLATE=1 cargo build --release -p boa_cli \
            --no-default-features --features boa_engine/jit
        [[ -s $template ]] || { echo 'Missing generated engine LLVM IR' >&2; exit 1; }

        echo 'Building JIT-enabled Boa CLI...' >&2
        BOA_JIT_TEMPLATE_LLVM_IR="$template" cargo build --release -p boa_cli \
            --no-default-features --features boa_engine/jit
    )
    printf '%s\n' "$fingerprint" > "$stamp"
fi

flock -u 9
exec 9>&-

if [[ $build_only == 1 ]]; then
    echo "$binary"
else
    exec "$binary" "$@"
fi
