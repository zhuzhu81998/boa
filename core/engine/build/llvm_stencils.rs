//! Make a shallow stencil module before native code generation.
#![allow(unsafe_op_in_unsafe_fn)]

use llvm_sys::{
    analysis::{LLVMVerifierFailureAction, LLVMVerifyModule},
    bit_writer::LLVMWriteBitcodeToFile,
    core::*,
    ir_reader::LLVMParseIRInContext2,
    prelude::*,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{CStr, CString},
    path::Path,
    ptr,
};

/// Numbered stencil symbols map to exact values in the original runtime IR.
pub(crate) fn lower(input: &Path, output: &Path) -> Result<BTreeMap<String, String>, String> {
    unsafe {
        let context = LLVMContextCreate();
        let result = lower_inner(context, input, output);
        LLVMContextDispose(context);
        result
    }
}

struct Module(LLVMModuleRef);
impl Drop for Module {
    fn drop(&mut self) {
        unsafe {
            LLVMDisposeModule(self.0);
        }
    }
}

unsafe fn message(ptr: *mut i8) -> String {
    if ptr.is_null() {
        return "LLVM failed without a diagnostic".into();
    }
    let result = CStr::from_ptr(ptr).to_string_lossy().into_owned();
    LLVMDisposeMessage(ptr);
    result
}

unsafe fn value_name(value: LLVMValueRef) -> Result<String, String> {
    let mut len = 0;
    let ptr = LLVMGetValueName2(value, &mut len);
    if len == 0 {
        return Err("unnamed runtime dependency".into());
    }
    String::from_utf8(std::slice::from_raw_parts(ptr.cast::<u8>(), len).to_vec())
        .map_err(|e| e.to_string())
}

unsafe fn lower_inner(
    context: LLVMContextRef,
    input: &Path,
    output: &Path,
) -> Result<BTreeMap<String, String>, String> {
    let input = CString::new(input.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
    let mut buffer = ptr::null_mut();
    let mut error = ptr::null_mut();
    if LLVMCreateMemoryBufferWithContentsOfFile(input.as_ptr(), &mut buffer, &mut error) != 0 {
        return Err(message(error));
    }
    let mut module = ptr::null_mut();
    let failed = LLVMParseIRInContext2(context, buffer, &mut module, &mut error);
    LLVMDisposeMemoryBuffer(buffer);
    if failed != 0 {
        return Err(message(error));
    }
    let module = Module(module);
    if !LLVMGetFirstGlobalAlias(module.0).is_null() {
        return Err("stencil module aliases need explicit lowering".into());
    }
    let table = LLVMGetNamedGlobal(module.0, c"BOA_JIT_TEMPLATE_HANDLERS".as_ptr());
    if table.is_null() || LLVMIsDeclaration(table) != 0 {
        return Err("missing handler table".into());
    }
    let initializer = LLVMGetInitializer(table);
    let mut roots = BTreeSet::new();
    for index in 0..LLVMGetNumOperands(initializer) {
        let value = LLVMGetOperand(initializer, index as u32);
        if LLVMIsAFunction(value).is_null() {
            return Err("handler entry is not a function".into());
        }
        roots.insert(value as usize);
    }
    lower_tail_calls(module.0, &roots)?;
    lower_constants(module.0, &roots)?;
    outline_tls(module.0, &roots)?;
    // These retain unrelated runtime definitions and have no purpose in the extracted module.
    for name in [c"llvm.used", c"llvm.compiler.used"] {
        let value = LLVMGetNamedGlobal(module.0, name.as_ptr());
        if !value.is_null() {
            LLVMDeleteGlobal(value);
        }
    }
    let mut holes = BTreeMap::new();
    let mut functions = Vec::new();
    let mut value = LLVMGetFirstFunction(module.0);
    while !value.is_null() {
        functions.push(value);
        value = LLVMGetNextFunction(value);
    }
    for value in functions {
        if roots.contains(&(value as usize)) || LLVMGetIntrinsicID(value) != 0 {
            continue;
        }
        let original = value_name(value)?;
        if original.starts_with("BOA_JIT_CONST_") {
            continue;
        }
        let hole = format!("BOA_JIT_HOLE_{}", holes.len());
        let name = CString::new(hole.as_str()).unwrap();
        let declaration = LLVMAddFunction(module.0, name.as_ptr(), LLVMGlobalGetValueType(value));
        LLVMSetFunctionCallConv(declaration, LLVMGetFunctionCallConv(value));
        LLVMReplaceAllUsesWith(value, declaration);
        // Deleting the whole function also removes its CFG without dangling basic-block edges.
        LLVMDeleteFunction(value);
        holes.insert(hole, original);
    }
    let mut globals = Vec::new();
    let mut value = LLVMGetFirstGlobal(module.0);
    while !value.is_null() {
        globals.push(value);
        value = LLVMGetNextGlobal(value);
    }
    for value in globals {
        let original = value_name(value)?;
        if value == table || original == "BOA_JIT_TEMPLATE_CONFIG" {
            continue;
        }
        // Even unnamed_addr constants can escape (for example Rust vtables retained by GC).
        // Keep all IR globals in the runtime so their lifetime outlasts any JIT allocation.
        // Machine-code jump tables are emitted later and retain per-instance relocations.
        let hole = format!("BOA_JIT_HOLE_{}", holes.len());
        let name = CString::new(hole.as_str()).unwrap();
        let declaration = LLVMAddGlobal(module.0, LLVMGlobalGetValueType(value), name.as_ptr());
        // Preserve TLS so native relocation validation rejects a handler with inline TLS.
        LLVMSetThreadLocalMode(declaration, LLVMGetThreadLocalMode(value));
        LLVMReplaceAllUsesWith(value, declaration);
        LLVMDeleteGlobal(value);
        holes.insert(hole, original);
    }
    if LLVMVerifyModule(
        module.0,
        LLVMVerifierFailureAction::LLVMReturnStatusAction,
        &mut error,
    ) != 0
    {
        return Err(message(error));
    }
    if !error.is_null() {
        LLVMDisposeMessage(error);
    }
    let output = CString::new(output.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
    if LLVMWriteBitcodeToFile(module.0, output.as_ptr()) != 0 {
        return Err("could not write shallow module".into());
    }
    Ok(holes)
}

/// Emit absolute immediate relocations, not GOT loads or calls to a runtime resolver.
unsafe fn lower_constants(module: LLVMModuleRef, roots: &BTreeSet<usize>) -> Result<(), String> {
    let marker = LLVMGetNamedFunction(module, c"boa_jit_constant".as_ptr());
    if marker.is_null() {
        return Ok(());
    }
    let builder = LLVMCreateBuilderInContext(LLVMGetModuleContext(module));
    for &root in roots {
        let mut block = LLVMGetFirstBasicBlock(root as LLVMValueRef);
        while !block.is_null() {
            let mut instruction = LLVMGetFirstInstruction(block);
            while !instruction.is_null() {
                let next = LLVMGetNextInstruction(instruction);
                if !LLVMIsACallInst(instruction).is_null()
                    && LLVMGetCalledValue(instruction) == marker
                {
                    let slot = LLVMGetOperand(instruction, 0);
                    if LLVMIsAConstantInt(slot).is_null() {
                        return Err("nonconstant patch slot".into());
                    }
                    let slot = LLVMConstIntGetZExtValue(slot);
                    if slot > 6 {
                        return Err("invalid patch slot".into());
                    }
                    let asm = format!("movabsq $$BOA_JIT_CONST_{slot}, $0");
                    let ty = LLVMFunctionType(LLVMTypeOf(instruction), ptr::null_mut(), 0, 0);
                    let asm = LLVMGetInlineAsm(
                        ty,
                        asm.as_ptr().cast(),
                        asm.len(),
                        c"=r".as_ptr(),
                        2,
                        0,
                        0,
                        llvm_sys::LLVMInlineAsmDialect::LLVMInlineAsmDialectATT,
                        0,
                    );
                    LLVMPositionBuilderBefore(builder, instruction);
                    let value = LLVMBuildCall2(builder, ty, asm, ptr::null_mut(), 0, c"".as_ptr());
                    LLVMReplaceAllUsesWith(instruction, value);
                    LLVMInstructionEraseFromParent(instruction);
                }
                instruction = next;
            }
            block = LLVMGetNextBasicBlock(block);
        }
    }
    LLVMDisposeBuilder(builder);
    Ok(())
}

/// Turn the source marker into a stack-neutral transfer. Never discard cleanup code to force
/// a tail call: accept only an immediate return or a branch to a bare return block.
unsafe fn lower_tail_calls(module: LLVMModuleRef, roots: &BTreeSet<usize>) -> Result<(), String> {
    let marker = LLVMGetNamedFunction(module, c"boa_jit_tail".as_ptr());
    let next_marker = LLVMGetNamedFunction(module, c"boa_jit_next".as_ptr());
    if marker.is_null() {
        return Ok(());
    }
    let mut calls = Vec::new();
    for &root in roots {
        let function = root as LLVMValueRef;
        let mut block = LLVMGetFirstBasicBlock(function);
        while !block.is_null() {
            let mut instruction = LLVMGetFirstInstruction(block);
            while !instruction.is_null() {
                if !LLVMIsACallInst(instruction).is_null()
                    && (LLVMGetCalledValue(instruction) == marker
                        || LLVMGetCalledValue(instruction) == next_marker)
                {
                    let first_argument = u32::from(LLVMGetCalledValue(instruction) == marker);
                    let next = LLVMGetNextInstruction(instruction);
                    let return_block = if !next.is_null()
                        && !LLVMIsABranchInst(next).is_null()
                        && LLVMGetNumSuccessors(next) == 1
                    {
                        LLVMGetFirstInstruction(LLVMGetSuccessor(next, 0))
                    } else {
                        next
                    };
                    if return_block.is_null()
                        || LLVMIsAReturnInst(return_block).is_null()
                        || LLVMGetNumOperands(return_block) != 0
                    {
                        return Err(format!(
                            "{}: continuation has nontrivial cleanup after marker",
                            value_name(function)?
                        ));
                    }
                    if LLVMCountParams(function) != 4
                        || LLVMGetNumArgOperands(instruction) != 4 + first_argument
                    {
                        return Err("unexpected native chain ABI".into());
                    }
                    // These pointers must outlive the current native stack frame. Only forward
                    // the original parameters, never an address of a handler-local temporary.
                    for (argument, parameter) in [
                        (first_argument, 0),
                        (first_argument + 2, 2),
                        (first_argument + 3, 3),
                    ] {
                        if LLVMGetOperand(instruction, argument)
                            != LLVMGetParam(function, parameter)
                        {
                            return Err(
                                "continuation does not forward the original chain pointers".into(),
                            );
                        }
                    }
                    calls.push((function, instruction, next, first_argument));
                }
                instruction = LLVMGetNextInstruction(instruction);
            }
            block = LLVMGetNextBasicBlock(block);
        }
    }
    let builder = LLVMCreateBuilderInContext(LLVMGetModuleContext(module));
    for (function, old, terminator, first_argument) in calls {
        let target = if first_argument == 1 {
            LLVMGetOperand(old, 0)
        } else {
            let name = c"BOA_JIT_CONST_6";
            let mut target = LLVMGetNamedFunction(module, name.as_ptr());
            if target.is_null() {
                target = LLVMAddFunction(module, name.as_ptr(), LLVMGlobalGetValueType(function));
            }
            target
        };
        let mut arguments: Vec<_> = (first_argument..first_argument + 4)
            .map(|index| LLVMGetOperand(old, index))
            .collect();
        LLVMPositionBuilderBefore(builder, old);
        let call = LLVMBuildCall2(
            builder,
            LLVMGlobalGetValueType(function),
            target,
            arguments.as_mut_ptr(),
            4,
            c"".as_ptr(),
        );
        LLVMSetInstructionCallConv(call, LLVMGetFunctionCallConv(function));
        LLVMSetTailCallKind(call, llvm_sys::LLVMTailCallKind::LLVMTailCallKindMustTail);
        LLVMBuildRetVoid(builder);
        LLVMInstructionEraseFromParent(old);
        LLVMInstructionEraseFromParent(terminator);
    }
    LLVMDisposeBuilder(builder);
    Ok(())
}

/// Resolve the thread's address at execution time, never at stencil patching time.
unsafe fn outline_tls(module: LLVMModuleRef, roots: &BTreeSet<usize>) -> Result<(), String> {
    let mut accesses = Vec::new();
    for &root in roots {
        let mut block = LLVMGetFirstBasicBlock(root as LLVMValueRef);
        while !block.is_null() {
            let mut instruction = LLVMGetFirstInstruction(block);
            while !instruction.is_null() {
                if !LLVMIsACallInst(instruction).is_null() {
                    let callee = LLVMGetCalledValue(instruction);
                    if !LLVMIsAFunction(callee).is_null()
                        && value_name(callee)? == "llvm.threadlocal.address.p0"
                    {
                        let global = LLVMGetOperand(instruction, 0);
                        if !LLVMIsAGlobalVariable(global).is_null()
                            && LLVMIsThreadLocal(global) != 0
                        {
                            accesses.push((instruction, global));
                        }
                    }
                }
                instruction = LLVMGetNextInstruction(instruction);
            }
            block = LLVMGetNextBasicBlock(block);
        }
    }
    let context = LLVMGetModuleContext(module);
    let builder = LLVMCreateBuilderInContext(context);
    for (instruction, global) in accesses {
        let name = CString::new(format!("BOA_JIT_TLS_ADDR_{}", value_name(global)?)).unwrap();
        let ty = LLVMFunctionType(LLVMTypeOf(instruction), ptr::null_mut(), 0, 0);
        let mut helper = LLVMGetNamedFunction(module, name.as_ptr());
        if helper.is_null() {
            helper = LLVMAddFunction(module, name.as_ptr(), ty);
        }
        LLVMPositionBuilderBefore(builder, instruction);
        let address = LLVMBuildCall2(
            builder,
            ty,
            helper,
            ptr::null_mut(),
            0,
            c"tls_address".as_ptr(),
        );
        LLVMReplaceAllUsesWith(instruction, address);
        LLVMInstructionEraseFromParent(instruction);
    }
    LLVMDisposeBuilder(builder);
    Ok(())
}
