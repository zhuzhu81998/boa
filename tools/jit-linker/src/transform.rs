//! Resolve the generated table against values in the actual runtime module, before LTO.
#![allow(unsafe_op_in_unsafe_fn)]

use llvm_sys::{
    LLVMLinkage,
    analysis::{LLVMVerifierFailureAction, LLVMVerifyModule},
    bit_writer::LLVMWriteBitcodeToMemoryBuffer,
    core::*,
    ir_reader::LLVMParseIRInContext2,
    prelude::*,
    target::{LLVMABISizeOfType, LLVMCreateTargetData, LLVMDisposeTargetData},
};
use std::{
    ffi::{CStr, CString},
    ptr,
};

const TABLE: &CStr = c"BOA_JIT_EXTERNAL_SYMBOLS";

struct Module(LLVMModuleRef);
impl Drop for Module {
    fn drop(&mut self) {
        unsafe { LLVMDisposeModule(self.0) }
    }
}

unsafe fn parse(context: LLVMContextRef, bytes: &[u8]) -> Result<Module, String> {
    let buffer = LLVMCreateMemoryBufferWithMemoryRangeCopy(
        bytes.as_ptr().cast(),
        bytes.len(),
        c"jit-link-input".as_ptr(),
    );
    let mut module = ptr::null_mut();
    let mut message = ptr::null_mut();
    let failed = LLVMParseIRInContext2(context, buffer, &mut module, &mut message);
    LLVMDisposeMemoryBuffer(buffer);
    if failed != 0 {
        return Err(take_message(message));
    }
    Ok(Module(module))
}

unsafe fn take_message(message: *mut i8) -> String {
    if message.is_null() {
        return "LLVM failed without a diagnostic".into();
    }
    let result = CStr::from_ptr(message).to_string_lossy().into_owned();
    LLVMDisposeMessage(message);
    result
}

/// `request` is the existing generated resolver bitcode. Its array order is the hole-ID order.
/// Return None for modules that do not own the opcode handler table.
pub fn inject(runtime: &[u8], request: &[u8]) -> Result<Option<Vec<u8>>, String> {
    unsafe {
        let context = LLVMContextCreate();
        let result = inject_inner(context, runtime, request);
        LLVMContextDispose(context);
        result
    }
}

unsafe fn inject_inner(
    context: LLVMContextRef,
    runtime: &[u8],
    request: &[u8],
) -> Result<Option<Vec<u8>>, String> {
    let runtime = parse(context, runtime)?;
    let marker = LLVMGetNamedGlobal(runtime.0, c"BOA_JIT_TEMPLATE_HANDLERS".as_ptr());
    if marker.is_null() || LLVMIsDeclaration(marker) != 0 {
        return Ok(None);
    }
    let request = parse(context, request)?;
    if CStr::from_ptr(LLVMGetTarget(runtime.0)) != CStr::from_ptr(LLVMGetTarget(request.0))
        || CStr::from_ptr(LLVMGetDataLayoutStr(runtime.0))
            != CStr::from_ptr(LLVMGetDataLayoutStr(request.0))
    {
        return Err("resolver and runtime target/data layout differ".into());
    }
    let table = LLVMGetNamedGlobal(request.0, TABLE.as_ptr());
    if table.is_null() || LLVMIsDeclaration(table) != 0 {
        return Err("resolver has no address table".into());
    }
    let initializer = LLVMGetInitializer(table);
    let count = LLVMGetArrayLength2(LLVMGlobalGetValueType(table));
    let mut entries = Vec::new();
    for index in 0..count {
        let source = LLVMGetOperand(initializer, index.try_into().map_err(|_| "too many holes")?);
        let mut length = 0;
        let name = LLVMGetValueName2(source, &mut length);
        if length == 0 {
            return Err(format!("hole {index} is not a named function/global"));
        }
        let name = CString::new(std::slice::from_raw_parts(name.cast::<u8>(), length)).unwrap();
        let is_function = !LLVMIsAFunction(source).is_null();
        let target = if let Some(tls_name) = name.to_bytes().strip_prefix(b"BOA_JIT_TLS_ADDR_") {
            if !is_function {
                return Err("TLS accessor request must be a function".into());
            }
            tls_accessor(runtime.0, &name, tls_name)?
        } else if anonymous_constant(source) {
            let mut candidate = LLVMGetFirstGlobal(runtime.0);
            while !candidate.is_null() {
                if anonymous_constant(candidate)
                    && same_constant(source, candidate, &mut std::collections::BTreeSet::new())
                {
                    break;
                }
                candidate = LLVMGetNextGlobal(candidate);
            }
            candidate
        } else if is_function {
            LLVMGetNamedFunction(runtime.0, name.as_ptr())
        } else {
            LLVMGetNamedGlobal(runtime.0, name.as_ptr())
        };
        // Compiler-introduced libc/libm declarations may be absent from pre-codegen runtime IR.
        // Never invent a missing Rust definition: this would hide a stale template/hash mismatch.
        let target = if target.is_null() {
            if !is_function || !compiler_runtime(name.to_bytes()) {
                return Err(format!(
                    "runtime is missing hole {index}: {} (rebuild the matching template)",
                    name.to_string_lossy()
                ));
            }
            LLVMAddFunction(runtime.0, name.as_ptr(), LLVMGlobalGetValueType(source))
        } else {
            target
        };
        if !same_type(
            LLVMGlobalGetValueType(source),
            LLVMGlobalGetValueType(target),
        ) {
            return Err(format!("ABI type mismatch for {}", name.to_string_lossy()));
        }
        if is_function && LLVMGetFunctionCallConv(source) != LLVMGetFunctionCallConv(target) {
            return Err(format!(
                "calling convention mismatch for {}",
                name.to_string_lossy()
            ));
        }
        if !is_function && LLVMIsThreadLocal(target) != 0 {
            return Err(format!(
                "TLS target {} needs a runtime helper",
                name.to_string_lossy()
            ));
        }
        entries.push(target);
    }
    let old = LLVMGetNamedGlobal(runtime.0, TABLE.as_ptr());
    if !old.is_null() && LLVMIsDeclaration(old) == 0 {
        return Err("runtime already defines the address table".into());
    }
    let pointer_type = LLVMPointerTypeInContext(context, 0);
    let ty = LLVMArrayType2(pointer_type, count);
    if !old.is_null() {
        // rustc can represent an extern static as a byte array rather than a pointer array.
        let layout = LLVMCreateTargetData(LLVMGetDataLayoutStr(runtime.0));
        let matches =
            LLVMABISizeOfType(layout, LLVMGlobalGetValueType(old)) == LLVMABISizeOfType(layout, ty);
        LLVMDisposeTargetData(layout);
        if !matches {
            return Err("runtime and resolver address-table sizes differ".into());
        }
    }
    let new = LLVMAddGlobal(runtime.0, ty, c"boa_jit_pending_table".as_ptr());
    LLVMSetInitializer(
        new,
        LLVMConstArray2(pointer_type, entries.as_mut_ptr(), count),
    );
    LLVMSetGlobalConstant(new, 1);
    LLVMSetLinkage(new, LLVMLinkage::LLVMExternalLinkage);
    if !old.is_null() {
        LLVMReplaceAllUsesWith(old, new);
        LLVMDeleteGlobal(old);
    }
    LLVMSetValueName2(new, TABLE.as_ptr(), TABLE.to_bytes().len());
    let mut message = ptr::null_mut();
    if LLVMVerifyModule(
        runtime.0,
        LLVMVerifierFailureAction::LLVMReturnStatusAction,
        &mut message,
    ) != 0
    {
        return Err(take_message(message));
    }
    if !message.is_null() {
        LLVMDisposeMessage(message);
    }
    let output = LLVMWriteBitcodeToMemoryBuffer(runtime.0);
    let bytes = std::slice::from_raw_parts(
        LLVMGetBufferStart(output).cast::<u8>(),
        LLVMGetBufferSize(output),
    )
    .to_vec();
    LLVMDisposeMemoryBuffer(output);
    Ok(Some(bytes))
}

unsafe fn anonymous_constant(value: LLVMValueRef) -> bool {
    !LLVMIsAGlobalVariable(value).is_null()
        && LLVMIsGlobalConstant(value) != 0
        && LLVMIsDeclaration(value) == 0
        && LLVMGetUnnamedAddress(value) == llvm_sys::LLVMUnnamedAddr::LLVMGlobalUnnamedAddr
}

unsafe fn tls_accessor(
    module: LLVMModuleRef,
    name: &CStr,
    tls_name: &[u8],
) -> Result<LLVMValueRef, String> {
    let tls_name = CString::new(tls_name).unwrap();
    let global = LLVMGetNamedGlobal(module, tls_name.as_ptr());
    if global.is_null() || LLVMIsThreadLocal(global) == 0 {
        return Err(format!("missing TLS global for {}", name.to_string_lossy()));
    }
    if !LLVMGetNamedFunction(module, name.as_ptr()).is_null() {
        return Err(format!(
            "TLS accessor name collision: {}",
            name.to_string_lossy()
        ));
    }
    let context = LLVMGetModuleContext(module);
    let pointer = LLVMPointerTypeInContext(context, 0);
    let ty = LLVMFunctionType(pointer, ptr::null_mut(), 0, 0);
    let helper = LLVMAddFunction(module, name.as_ptr(), ty);
    LLVMSetLinkage(helper, LLVMLinkage::LLVMInternalLinkage);
    let noinline = LLVMGetEnumAttributeKindForName(c"noinline".as_ptr(), 8);
    LLVMAddAttributeAtIndex(
        helper,
        llvm_sys::LLVMAttributeFunctionIndex,
        LLVMCreateEnumAttribute(context, noinline, 0),
    );
    let block = LLVMAppendBasicBlockInContext(context, helper, c"entry".as_ptr());
    let builder = LLVMCreateBuilderInContext(context);
    LLVMPositionBuilderAtEnd(builder, block);
    let mut argument_types = [pointer];
    let intrinsic_type = LLVMFunctionType(pointer, argument_types.as_mut_ptr(), 1, 0);
    let mut intrinsic = LLVMGetNamedFunction(module, c"llvm.threadlocal.address.p0".as_ptr());
    if intrinsic.is_null() {
        intrinsic = LLVMAddFunction(
            module,
            c"llvm.threadlocal.address.p0".as_ptr(),
            intrinsic_type,
        );
    }
    let mut arguments = [global];
    let address = LLVMBuildCall2(
        builder,
        intrinsic_type,
        intrinsic,
        arguments.as_mut_ptr(),
        1,
        c"address".as_ptr(),
    );
    LLVMBuildRet(builder, address);
    LLVMDisposeBuilder(builder);
    Ok(helper)
}

// Compare anonymous constant graphs, not their unstable alloc_* labels. Named references retain
// exact identity. Cycles are compared coinductively; no generated-memory pointers are introduced.
unsafe fn same_constant(
    left: LLVMValueRef,
    right: LLVMValueRef,
    visited: &mut std::collections::BTreeSet<(usize, usize)>,
) -> bool {
    if left == right {
        return true;
    }
    if LLVMGetValueKind(left) != LLVMGetValueKind(right)
        || !same_type(LLVMTypeOf(left), LLVMTypeOf(right))
    {
        return false;
    }
    if !visited.insert((left as usize, right as usize)) {
        return true;
    }
    if !LLVMIsAGlobalValue(left).is_null() {
        if !same_type(LLVMGlobalGetValueType(left), LLVMGlobalGetValueType(right)) {
            return false;
        }
        if !LLVMIsAFunction(left).is_null()
            && LLVMGetFunctionCallConv(left) != LLVMGetFunctionCallConv(right)
        {
            return false;
        }
        if anonymous_constant(left) && anonymous_constant(right) {
            return same_type(LLVMGlobalGetValueType(left), LLVMGlobalGetValueType(right))
                && same_constant(LLVMGetInitializer(left), LLVMGetInitializer(right), visited);
        }
        let mut a_len = 0;
        let mut b_len = 0;
        let a = LLVMGetValueName2(left, &mut a_len);
        let b = LLVMGetValueName2(right, &mut b_len);
        return a_len != 0
            && a_len == b_len
            && std::slice::from_raw_parts(a.cast::<u8>(), a_len)
                == std::slice::from_raw_parts(b.cast::<u8>(), b_len);
    }
    if !LLVMIsAConstantExpr(left).is_null() {
        if LLVMGetConstOpcode(left) != LLVMGetConstOpcode(right) {
            return false;
        }
        if LLVMGetConstOpcode(left) == llvm_sys::LLVMOpcode::LLVMGetElementPtr
            && !same_type(
                LLVMGetGEPSourceElementType(left),
                LLVMGetGEPSourceElementType(right),
            )
        {
            return false;
        }
    }
    if !LLVMIsABlockAddress(left).is_null() {
        return false;
    }
    let count = LLVMGetNumOperands(left);
    if count != LLVMGetNumOperands(right) {
        return false;
    }
    if count != 0 {
        return (0..count).all(|i| {
            same_constant(
                LLVMGetOperand(left, i as u32),
                LLVMGetOperand(right, i as u32),
                visited,
            )
        });
    }
    let a = LLVMPrintValueToString(left);
    let b = LLVMPrintValueToString(right);
    let equal = CStr::from_ptr(a) == CStr::from_ptr(b);
    LLVMDisposeMessage(a);
    LLVMDisposeMessage(b);
    equal
}

// Parsing two modules into one context gives their identified structs distinct identities.
// Compare ABI structure, not LLVMTypeRef identity or the compiler's temporary type names.
unsafe fn same_type(left: LLVMTypeRef, right: LLVMTypeRef) -> bool {
    use llvm_sys::LLVMTypeKind::*;
    if left == right {
        return true;
    }
    if LLVMGetTypeKind(left) != LLVMGetTypeKind(right) {
        return false;
    }
    match LLVMGetTypeKind(left) {
        LLVMIntegerTypeKind => LLVMGetIntTypeWidth(left) == LLVMGetIntTypeWidth(right),
        LLVMPointerTypeKind => {
            LLVMGetPointerAddressSpace(left) == LLVMGetPointerAddressSpace(right)
        }
        LLVMArrayTypeKind => {
            LLVMGetArrayLength2(left) == LLVMGetArrayLength2(right)
                && same_type(LLVMGetElementType(left), LLVMGetElementType(right))
        }
        LLVMStructTypeKind => {
            if LLVMIsOpaqueStruct(left) != 0
                || LLVMIsOpaqueStruct(right) != 0
                || LLVMIsPackedStruct(left) != LLVMIsPackedStruct(right)
                || LLVMCountStructElementTypes(left) != LLVMCountStructElementTypes(right)
            {
                return false;
            }
            let count = LLVMCountStructElementTypes(left) as usize;
            let mut a = vec![ptr::null_mut(); count];
            let mut b = vec![ptr::null_mut(); count];
            LLVMGetStructElementTypes(left, a.as_mut_ptr());
            LLVMGetStructElementTypes(right, b.as_mut_ptr());
            a.into_iter().zip(b).all(|(a, b)| same_type(a, b))
        }
        LLVMFunctionTypeKind => {
            if LLVMIsFunctionVarArg(left) != LLVMIsFunctionVarArg(right)
                || LLVMCountParamTypes(left) != LLVMCountParamTypes(right)
                || !same_type(LLVMGetReturnType(left), LLVMGetReturnType(right))
            {
                return false;
            }
            let count = LLVMCountParamTypes(left) as usize;
            let mut a = vec![ptr::null_mut(); count];
            let mut b = vec![ptr::null_mut(); count];
            LLVMGetParamTypes(left, a.as_mut_ptr());
            LLVMGetParamTypes(right, b.as_mut_ptr());
            a.into_iter().zip(b).all(|(a, b)| same_type(a, b))
        }
        LLVMVectorTypeKind | LLVMScalableVectorTypeKind => {
            LLVMGetVectorSize(left) == LLVMGetVectorSize(right)
                && same_type(LLVMGetElementType(left), LLVMGetElementType(right))
        }
        LLVMVoidTypeKind
        | LLVMHalfTypeKind
        | LLVMBFloatTypeKind
        | LLVMFloatTypeKind
        | LLVMDoubleTypeKind
        | LLVMX86_FP80TypeKind
        | LLVMFP128TypeKind
        | LLVMPPC_FP128TypeKind => true,
        _ => false,
    }
}

fn compiler_runtime(name: &[u8]) -> bool {
    matches!(
        name,
        b"memcpy"
            | b"memmove"
            | b"memset"
            | b"memcmp"
            | b"bcmp"
            | b"__powidf2"
            | b"floor"
            | b"ceil"
            | b"trunc"
            | b"round"
            | b"sqrt"
            | b"sin"
            | b"cos"
            | b"tan"
            | b"exp"
            | b"log"
            | b"pow"
            | b"fmod"
            | b"copysign"
            | b"fma"
            | b"ldexp"
    )
}

#[cfg(test)]
mod tests {
    use super::inject;

    fn constant_request(
        runtime_constant: &str,
        requested_constant: &str,
    ) -> Result<Option<Vec<u8>>, String> {
        let runtime = format!(
            "@BOA_JIT_TEMPLATE_HANDLERS = constant [1 x ptr] [ptr @handler]\ndefine void @handler() {{ ret void }}\n{runtime_constant}"
        );
        let request = format!(
            "{requested_constant}\n@BOA_JIT_EXTERNAL_SYMBOLS = constant [1 x ptr] [ptr @requested]"
        );
        inject(runtime.as_bytes(), request.as_bytes())
    }

    #[test]
    fn same_label_does_not_hide_a_different_initializer() {
        let error = constant_request(
            "@requested = private unnamed_addr constant i64 41",
            "@requested = private unnamed_addr constant i64 42",
        )
        .unwrap_err();
        assert!(error.contains("runtime is missing hole 0"), "{error}");
    }

    #[test]
    fn local_address_insignificance_does_not_allow_cross_module_matching() {
        let error = constant_request(
            "@runtime = private local_unnamed_addr constant i64 42",
            "@requested = private local_unnamed_addr constant i64 42",
        )
        .unwrap_err();
        assert!(error.contains("runtime is missing hole 0"), "{error}");
    }

    #[test]
    fn constant_graph_does_not_guess_private_function_names() {
        let error = constant_request(
            "define internal i32 @runtime_helper() { ret i32 42 }\n@runtime = private unnamed_addr constant ptr @runtime_helper",
            "declare i32 @different_helper()\n@requested = private unnamed_addr constant ptr @different_helper",
        ).unwrap_err();
        assert!(error.contains("runtime is missing hole 0"), "{error}");
    }
}
