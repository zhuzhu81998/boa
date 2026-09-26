#!/usr/bin/env bash
# Build a matching template/runtime pair using the selected package's dependency graph.
set -euo pipefail

# Cargo invokes this same script as RUSTC_WORKSPACE_WRAPPER. Only the engine gets
# an additional IR output; its ordinary Cargo crate type/features remain unchanged.
if [[ ${BOA_JIT_DRIVER_COMPILER:-} == 1 ]]; then
    compiler=$1
    shift
    previous=
    engine=0
    crate_name=
    output_dir=
    extra_filename=
    for argument in "$@"; do
        if [[ $previous == --crate-name ]]; then crate_name=$argument; fi
        if [[ $previous == --out-dir ]]; then output_dir=$argument; fi
        if [[ $previous == -C && $argument == extra-filename=* ]]; then extra_filename=${argument#extra-filename=}; fi
        if [[ $argument == -Cextra-filename=* ]]; then extra_filename=${argument#-Cextra-filename=}; fi
        if [[ $previous == --crate-name && $argument == boa_engine ]]; then
            engine=1
        fi
        previous=$argument
    done
    if [[ $engine == 1 && ${BOA_JIT_BUILD_TEMPLATE:-} == 1 ]]; then
        exec "$compiler" "$@" "--emit=llvm-ir=$BOA_JIT_DRIVER_IR"
    fi
    if [[ -n ${BOA_JIT_TEMPLATE_LLVM_IR:-} ]]; then
        if [[ $engine == 1 ]]; then
            "$compiler" "$@"
            "$BOA_JIT_DRIVER_LINKER" --prepare-runtime \
                "$output_dir/libboa_engine$extra_filename.rlib" "$OUT_DIR"
            exit 0
        else
            # All consumers use the engine archive prepared above, regardless of
            # their crate name. The linker only acts on this flag for JIT links.
            export BOA_JIT_RESOLVER_PREPARED=1
        fi
    fi
    exec "$compiler" "$@"
fi

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
driver="$repo/tools/jit-run.sh"
build_only=0
package=boa_cli
bin=
while (($#)); do
    case $1 in
        --build-only) build_only=1; shift ;;
        -p|--package|--bin)
            if [[ $# -lt 2 || ! $2 =~ ^[a-zA-Z0-9_][a-zA-Z0-9_-]*$ ]]; then
                echo "$1 requires a Cargo package/binary name" >&2
                exit 2
            fi
            if [[ $1 == --bin ]]; then bin=$2; else package=$2; fi
            shift 2
            ;;
        --help)
            echo 'Usage: ./tools/jit-run.sh [--build-only] [-p PACKAGE] [--bin BINARY] [--] [program arguments...]'
            echo 'Defaults: package boa_cli, binary boa. Other packages default to a binary with the package name.'
            echo 'Example: ./tools/jit-run.sh -p boa_tester -- run --suite test/language/expressions -v'
            echo 'Builds with the selected package’s default features plus boa_engine/jit.'
            exit 0
            ;;
        --) shift; break ;;
        *) break ;;
    esac
done
if [[ -z $bin ]]; then
    if [[ $package == boa_cli ]]; then bin=boa; else bin=$package; fi
fi

build_dir="$repo/target/jit-cli"
if [[ $package != boa_cli || $bin != boa ]]; then
    # Keep canonical templates isolated when consumer feature graphs differ.
    build_dir="$repo/target/jit-packages/$package/$bin"
fi
tool_dir="$repo/target/jit-tools"
binary="$build_dir/release/$bin"
template="$build_dir/boa_engine_template.ll"
stamp="$build_dir/source-fingerprint"
mkdir -p "$build_dir"
exec 9>"$build_dir/driver.lock"
flock 9

# Preserve the caller's directory for program arguments.
fingerprint=$(
    cd "$repo"
    {
        rustc -vV
        printf '%s\n' "$package" "$bin"
        git ls-files --cached --others --exclude-standard -z -- \
            core cli tests tools utils examples .cargo Cargo.toml Cargo.lock |
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
        unset BOA_JIT_RESOLVER_PREPARED
        export LLVM_SYS_221_PREFIX=${LLVM_SYS_221_PREFIX:-/usr/lib/llvm-22}
        # Inherit release optimization, fat LTO, codegen units, and stripping.

        echo 'Building JIT linker wrapper...' >&2
        CARGO_TARGET_DIR="$tool_dir" cargo build -p boa_jit_linker

        export CARGO_TARGET_DIR="$build_dir"
        export RUSTC_WORKSPACE_WRAPPER="$driver"
        export BOA_JIT_DRIVER_COMPILER=1
        export BOA_JIT_DRIVER_IR="$template"
        export BOA_JIT_DRIVER_LINKER="$tool_dir/debug/boa_jit_linker"
        # Native JIT frames do not yet support Rust unwinding.
        flags=(-Cpanic=abort -Clinker-plugin-lto
            "-Clinker=$tool_dir/debug/boa_jit_linker" -Clink-arg=-fuse-ld=lld
            -Clink-arg=-Wl,--lto-O3)
        printf -v CARGO_ENCODED_RUSTFLAGS '%s\x1f' "${flags[@]}"
        export CARGO_ENCODED_RUSTFLAGS=${CARGO_ENCODED_RUSTFLAGS%$'\x1f'}

        cargo_args=(build --release -p "$package" --bin "$bin" --features boa_engine/jit)
        echo "Building canonical template for $package ($bin)..." >&2
        BOA_JIT_BUILD_TEMPLATE=1 cargo "${cargo_args[@]}"
        [[ -s $template ]] || { echo 'Missing generated engine LLVM IR' >&2; exit 1; }

        echo "Building JIT-enabled $bin..." >&2
        BOA_JIT_TEMPLATE_LLVM_IR="$template" cargo "${cargo_args[@]}"
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
