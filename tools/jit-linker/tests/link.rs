//! These integration tests require the LLVM 22 tools used by the experimental JIT.
use std::{fs, path::Path, process::Command};

const RUNTIME: &str = r#"
target triple = "x86_64-unknown-linux-gnu"
target datalayout = "e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-f80:128-n8:16:32:64-S128"
@BOA_JIT_TEMPLATE_HANDLERS = constant [1 x ptr] [ptr @handler]
@BOA_JIT_EXTERNAL_SYMBOLS = external constant [8 x i8]
define void @handler() {
  ret void
}
define internal i32 @private_drop() noinline {
  ret i32 42
}
define i32 @main() {
  %p = load ptr, ptr @BOA_JIT_EXTERNAL_SYMBOLS
  %v = call i32 %p()
  %r = sub i32 %v, 42
  ret i32 %r
}
"#;
const REQUEST: &str = r#"
target triple = "x86_64-unknown-linux-gnu"
target datalayout = "e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-f80:128-n8:16:32:64-S128"
declare i32 @private_drop()
@BOA_JIT_EXTERNAL_SYMBOLS = constant [1 x ptr] [ptr @private_drop]
"#;

fn tool(name: &str) -> Command {
    Command::new(format!("/usr/lib/llvm-22/bin/{name}"))
}

fn assemble(dir: &Path, name: &str, ir: &str) -> std::path::PathBuf {
    let input = dir.join(format!("{name}.ll"));
    let output = dir.join(format!("{name}.bc"));
    fs::write(&input, ir).unwrap();
    assert!(
        tool("llvm-as")
            .arg(input)
            .arg("-o")
            .arg(&output)
            .status()
            .unwrap()
            .success()
    );
    output
}

fn check_link(archive: bool) {
    check_link_ir(archive, RUNTIME, REQUEST);
}

fn check_link_ir(archive: bool, runtime_ir: &str, request_ir: &str) {
    let dir = tempfile::tempdir().unwrap();
    let runtime = assemble(dir.path(), "runtime", runtime_ir);
    let request = assemble(dir.path(), "boa_jit_resolver", request_ir);
    assert!(
        tool("llvm-ar")
            .arg("crs")
            .arg(dir.path().join("libboa_jit_resolver.a"))
            .arg(request)
            .status()
            .unwrap()
            .success()
    );
    let input = if archive {
        let archive = dir.path().join("libboa_engine.rlib");
        assert!(
            tool("llvm-ar")
                .arg("crs")
                .arg(&archive)
                .arg(&runtime)
                .status()
                .unwrap()
                .success()
        );
        archive
    } else {
        let object = dir.path().join("runtime.o");
        fs::copy(runtime, &object).unwrap();
        object
    };
    let original = fs::read(&input).unwrap();
    let executable = dir.path().join("program");
    let result = Command::new(env!("CARGO_BIN_EXE_boa_jit_linker"))
        .arg(&input)
        .arg("-L")
        .arg(dir.path())
        .arg("-lboa_jit_resolver")
        .arg("-fuse-ld=lld")
        .arg("-o")
        .arg(&executable)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(Command::new(executable).status().unwrap().success());
    assert_eq!(
        fs::read(input).unwrap(),
        original,
        "Cargo input was modified"
    );
}

#[test]
fn internal_function_in_bitcode_object() {
    check_link(false);
}

#[test]
fn internal_function_in_rust_archive() {
    check_link(true);
}

#[test]
fn identified_global_types_are_compared_structurally() {
    let runtime = RUNTIME
        .replace("define internal i32 @private_drop() noinline {\n  ret i32 42\n}",
                 "%Header = type { i32, i32 }\n@private_header = internal constant %Header { i32 42, i32 0 }")
        .replace("%v = call i32 %p()", "%v = load i32, ptr %p");
    let request = REQUEST
        .replace(
            "declare i32 @private_drop()",
            "%Header = type { i32, i32 }\n@private_header = external constant %Header",
        )
        .replace("ptr @private_drop", "ptr @private_header");
    check_link_ir(false, &runtime, &request);
}

#[test]
fn anonymous_constant_graph_uses_runtime_storage_despite_different_labels() {
    let runtime = RUNTIME
        .replace("define i32 @main()", "@runtime_inner = private unnamed_addr constant ptr @private_drop\n@runtime_outer = private unnamed_addr constant { ptr } { ptr @runtime_inner }\ndefine i32 @main()")
        .replace("%v = call i32 %p()", "%inner = load ptr, ptr %p\n  %function = load ptr, ptr %inner\n  %v = call i32 %function()");
    let request = REQUEST
        .replace("@BOA_JIT_EXTERNAL_SYMBOLS =", "@request_inner = private unnamed_addr constant ptr @private_drop\n@request_outer = private unnamed_addr constant { ptr } { ptr @request_inner }\n@BOA_JIT_EXTERNAL_SYMBOLS =")
        .replace("[ptr @private_drop]", "[ptr @request_outer]");
    check_link_ir(false, &runtime, &request);
}

#[test]
fn missing_private_function_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = assemble(dir.path(), "runtime", RUNTIME);
    let object = dir.path().join("runtime.o");
    fs::rename(runtime, &object).unwrap();
    let request = assemble(
        dir.path(),
        "boa_jit_resolver",
        &REQUEST.replace("private_drop", "wrong_hash"),
    );
    assert!(
        tool("llvm-ar")
            .arg("crs")
            .arg(dir.path().join("libboa_jit_resolver.a"))
            .arg(request)
            .status()
            .unwrap()
            .success()
    );
    let result = Command::new(env!("CARGO_BIN_EXE_boa_jit_linker"))
        .arg(object)
        .arg("-L")
        .arg(dir.path())
        .arg("-lboa_jit_resolver")
        .arg("-fuse-ld=lld")
        .arg("-o")
        .arg(dir.path().join("program"))
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("runtime is missing hole 0: wrong_hash")
    );
    assert!(!dir.path().join("program").exists());
}
