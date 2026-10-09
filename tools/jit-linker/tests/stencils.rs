use object::{Object, ObjectSection, ObjectSymbol, RelocationTarget, SymbolKind, SymbolSection};
use std::{fs, process::Command};

#[test]
fn escaping_unnamed_constant_is_a_runtime_hole() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("runtime.ll");
    let output = dir.path().join("shallow.bc");
    fs::write(
        &input,
        r#"
@BOA_JIT_TEMPLATE_HANDLERS = constant <{ ptr }> <{ ptr @handler }>
@vtable = private unnamed_addr constant { i64, i64 } { i64 8, i64 8 }
define ptr @handler() {
  ret ptr @vtable
}
"#,
    )
    .unwrap();
    let holes = boa_jit_llvm::lower_stencils(&input, &output).unwrap();
    let hole = holes
        .iter()
        .find(|(_, name)| name.as_str() == "vtable")
        .unwrap()
        .0;
    let disassembly = Command::new(boa_jit_llvm::tool("LLVM_DIS", "llvm-dis").unwrap())
        .arg(output)
        .args(["-o", "-"])
        .output()
        .unwrap();
    assert!(disassembly.status.success());
    let ir = String::from_utf8(disassembly.stdout).unwrap();
    assert!(ir.contains(&format!("@{hole} = external global")), "{ir}");
    assert!(ir.contains(&format!("ret ptr @{hole}")), "{ir}");
    assert!(!ir.contains("@vtable"), "the constant must not be copied");
}

#[test]
fn private_helper_becomes_a_hole_without_copying_its_tls_body() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("runtime.ll");
    let output = dir.path().join("shallow.bc");
    fs::write(&input, r#"
target triple = "x86_64-unknown-linux-gnu"
target datalayout = "e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-f80:128-n8:16:32:64-S128"
@BOA_JIT_TEMPLATE_HANDLERS = constant <{ ptr }> <{ ptr @handler }>
@tls = internal thread_local global i32 42
define internal i32 @private_drop() {
  %v = load i32, ptr @tls
  ret i32 %v
}
define i32 @handler() {
  %v = call i32 @private_drop()
  ret i32 %v
}
"#).unwrap();
    let holes = boa_jit_llvm::lower_stencils(&input, &output).unwrap();
    let hole = holes
        .iter()
        .find(|(_, name)| name.as_str() == "private_drop")
        .unwrap()
        .0;
    let native = dir.path().join("stencil.o");
    let result = Command::new(boa_jit_llvm::tool("LLVM_LLC", "llc").unwrap())
        .args([
            "--filetype=obj",
            "--function-sections",
            "--data-sections",
            "--relocation-model=pic",
            "--x86-relax-relocations=false",
        ])
        .arg(output)
        .arg("-o")
        .arg(&native)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let bytes = fs::read(native).unwrap();
    let file = object::File::parse(bytes.as_slice()).unwrap();
    let definitions: Vec<_> = file
        .symbols()
        .filter(|s| s.kind() == SymbolKind::Text && s.is_definition())
        .collect();
    assert_eq!(
        definitions.len(),
        1,
        "only the handler may have a code body"
    );
    assert_eq!(definitions[0].name().unwrap(), "handler");
    let section = file.section_by_name(".text.handler").unwrap();
    let calls: Vec<_> = section.relocations().collect();
    assert_eq!(
        calls.len(),
        1,
        "the TLS dependency must remain inside the runtime helper"
    );
    let RelocationTarget::Symbol(index) = calls[0].1.target() else {
        panic!("expected symbol relocation")
    };
    let symbol = file.symbol_by_index(index).unwrap();
    assert_eq!(symbol.name().unwrap(), hole);
    assert_eq!(symbol.section(), SymbolSection::Undefined);
}

const TAIL_RUNTIME: &str = r#"
@BOA_JIT_TEMPLATE_HANDLERS = constant <{ ptr }> <{ ptr @handler }>
declare void @boa_jit_tail(ptr, ptr, i64, ptr, ptr)
declare void @boa_jit_next(ptr, i64, ptr, ptr)
declare void @llvm.lifetime.end.p0(ptr)
define void @handler(ptr %context, i64 %pc, ptr %chain, ptr %output) {
entry:
  %local = alloca i8
  %fallback = icmp eq ptr %context, null
  br i1 %fallback, label %return, label %transfer
transfer:
  call void @boa_jit_next(ptr %context, i64 %pc, ptr %chain, ptr %output)
  br label %return
return:
  call void @llvm.lifetime.end.p0(ptr %local)
  ret void
}
"#;

#[test]
fn continuation_accepts_lifetime_end_in_shared_return_block() {
    check_tail_lifetime(TAIL_RUNTIME);
}

#[test]
fn continuation_accepts_lifetime_end_before_immediate_return() {
    let runtime = TAIL_RUNTIME
        .replace("  br label %return\nreturn:\n", "")
        .replace("  %fallback = icmp eq ptr %context, null\n  br i1 %fallback, label %return, label %transfer", "  br label %transfer");
    check_tail_lifetime(&runtime);
}

fn check_tail_lifetime(runtime: &str) {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("runtime.ll");
    let output = dir.path().join("shallow.bc");
    fs::write(&input, runtime).unwrap();
    boa_jit_llvm::lower_stencils(&input, &output).unwrap();
    let disassembly = Command::new(boa_jit_llvm::tool("LLVM_DIS", "llvm-dis").unwrap())
        .arg(output)
        .args(["-o", "-"])
        .output()
        .unwrap();
    assert!(disassembly.status.success());
    let ir = String::from_utf8(disassembly.stdout).unwrap();
    assert!(ir.contains("musttail call void @BOA_JIT_CONST_6"), "{ir}");
}

#[test]
fn continuation_rejects_real_cleanup_after_lifetime_end() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("runtime.ll");
    let output = dir.path().join("shallow.bc");
    let runtime = TAIL_RUNTIME
        .replace("define void @handler", "declare void @cleanup(ptr)\ndefine void @handler")
        .replace("  ret void", "  call void @cleanup(ptr %output)\n  ret void");
    fs::write(&input, runtime).unwrap();
    let error = boa_jit_llvm::lower_stencils(&input, &output).unwrap_err();
    assert!(error.contains("nontrivial cleanup after marker"), "{error}");
}
