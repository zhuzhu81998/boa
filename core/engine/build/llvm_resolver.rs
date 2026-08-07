#![allow(unsafe_op_in_unsafe_fn)]

use llvm_sys::{
    LLVMLinkage,
    core::*,
    ir_reader::LLVMParseIRInContext2,
    prelude::{LLVMContextRef, LLVMMemoryBufferRef, LLVMModuleRef, LLVMTypeRef, LLVMValueRef},
    target::{LLVM_InitializeNativeAsmPrinter, LLVM_InitializeNativeTarget},
    target_machine::{
        LLVMCodeGenFileType, LLVMCodeGenOptLevel, LLVMCodeModel, LLVMCreateTargetMachine,
        LLVMDisposeTargetMachine, LLVMGetTargetFromTriple, LLVMRelocMode,
        LLVMTargetMachineEmitToFile,
    },
};
use std::{
    collections::BTreeSet,
    ffi::{CStr, CString},
    path::{Path, PathBuf},
    process::Command,
    ptr,
};

const TABLE_NAME: &str = "BOA_JIT_EXTERNAL_SYMBOLS";

pub(super) fn generate(ir: &Path, names: &[String], out: &Path) -> Result<PathBuf, String> {
    unsafe { generate_inner(ir, names, out) }
}

unsafe fn generate_inner(ir: &Path, names: &[String], out: &Path) -> Result<PathBuf, String> {
    let context = LLVMContextCreate();
    if context.is_null() {
        return Err("LLVMContextCreate failed".into());
    }
    let result = generate_in_context(context, ir, names, out);
    LLVMContextDispose(context);
    result
}

unsafe fn generate_in_context(
    context: LLVMContextRef,
    ir: &Path,
    names: &[String],
    out: &Path,
) -> Result<PathBuf, String> {
    let ir_path = path_cstring(ir)?;
    let mut buffer: LLVMMemoryBufferRef = ptr::null_mut();
    let mut message = ptr::null_mut();
    if LLVMCreateMemoryBufferWithContentsOfFile(ir_path.as_ptr(), &mut buffer, &mut message) != 0 {
        return Err(take_message(message));
    }
    let mut original: LLVMModuleRef = ptr::null_mut();
    let parsed = LLVMParseIRInContext2(context, buffer, &mut original, &mut message);
    LLVMDisposeMemoryBuffer(buffer);
    if parsed != 0 {
        return Err(take_message(message));
    }

    let module_name = CString::new("boa_jit_external_resolver").unwrap();
    let resolver = LLVMModuleCreateWithNameInContext(module_name.as_ptr(), context);
    LLVMSetTarget(resolver, LLVMGetTarget(original));
    LLVMSetDataLayout(resolver, LLVMGetDataLayoutStr(original));

    let pointer_type = LLVMPointerTypeInContext(context, 0);
    let mut entries = Vec::<LLVMValueRef>::with_capacity(names.len());
    for name in names {
        let c_name = CString::new(name.as_str())
            .map_err(|_| format!("external symbol contains NUL: {name:?}"))?;
        let source_function = LLVMGetNamedFunction(original, c_name.as_ptr());
        let declaration = if !source_function.is_null() {
            LLVMAddFunction(
                resolver,
                c_name.as_ptr(),
                LLVMGlobalGetValueType(source_function),
            )
        } else {
            let source_global = LLVMGetNamedGlobal(original, c_name.as_ptr());
            if !source_global.is_null() {
                LLVMAddGlobal(
                    resolver,
                    LLVMGlobalGetValueType(source_global),
                    c_name.as_ptr(),
                )
            } else if let Some(runtime) =
                add_compiler_runtime(context, resolver, name, c_name.as_ptr())
            {
                runtime
            } else {
                LLVMDisposeModule(resolver);
                LLVMDisposeModule(original);
                return Err(format!(
                    "LLVM module has no function or global named {name:?}"
                ));
            }
        };
        // The declaration itself is a typed LLVM function/global value. The cast only normalizes
        // opaque pointer address spaces for the homogeneous exported table.
        entries.push(LLVMConstPointerCast(declaration, pointer_type));
    }

    let initializer = LLVMConstArray2(pointer_type, entries.as_mut_ptr(), entries.len() as u64);
    let table_type = LLVMArrayType2(pointer_type, entries.len() as u64);
    let table_name = CString::new(TABLE_NAME).unwrap();
    let table = LLVMAddGlobal(resolver, table_type, table_name.as_ptr());
    LLVMSetInitializer(table, initializer);
    LLVMSetGlobalConstant(table, 1);
    LLVMSetLinkage(table, LLVMLinkage::LLVMExternalLinkage);

    let object = out.join("boa_jit_resolver.o");
    emit_object(resolver, &object)?;
    LLVMDisposeModule(resolver);
    LLVMDisposeModule(original);

    let archive = out.join("libboa_jit_resolver.a");
    let ar = std::env::var_os("LLVM_AR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/lib/llvm-22/bin/llvm-ar"));
    let status = Command::new(&ar)
        .arg("crs")
        .arg(&archive)
        .arg(&object)
        .status()
        .map_err(|e| format!("could not run {}: {e}", ar.display()))?;
    if !status.success() {
        return Err(format!("{} failed with {status}", ar.display()));
    }
    Ok(archive)
}

unsafe fn add_compiler_runtime(
    context: LLVMContextRef,
    module: LLVMModuleRef,
    rust_name: &str,
    name: *const i8,
) -> Option<LLVMValueRef> {
    let pointer = LLVMPointerTypeInContext(context, 0);
    let i32_type = LLVMInt32TypeInContext(context);
    let i64_type = LLVMInt64TypeInContext(context);
    let f64_type = LLVMDoubleTypeInContext(context);
    let (result, mut parameters): (LLVMTypeRef, Vec<LLVMTypeRef>) = match rust_name {
        "memcpy" | "memmove" => (pointer, vec![pointer, pointer, i64_type]),
        "memset" => (pointer, vec![pointer, i32_type, i64_type]),
        "__powidf2" => (f64_type, vec![f64_type, i32_type]),
        "floor" | "ceil" | "trunc" | "round" | "sqrt" | "sin" | "cos" | "tan" | "exp" | "log" => {
            (f64_type, vec![f64_type])
        }
        "pow" | "fmod" | "copysign" => (f64_type, vec![f64_type, f64_type]),
        "fma" => (f64_type, vec![f64_type, f64_type, f64_type]),
        "ldexp" => (f64_type, vec![f64_type, i32_type]),
        "memcmp" | "bcmp" => (i32_type, vec![pointer, pointer, i64_type]),
        _ => return None,
    };
    let function = LLVMFunctionType(result, parameters.as_mut_ptr(), parameters.len() as u32, 0);
    Some(LLVMAddFunction(module, name, function))
}

unsafe fn emit_object(module: LLVMModuleRef, path: &Path) -> Result<(), String> {
    if LLVM_InitializeNativeTarget() != 0 || LLVM_InitializeNativeAsmPrinter() != 0 {
        return Err("LLVM native target initialization failed".into());
    }
    let triple = LLVMGetTarget(module);
    let mut target = ptr::null_mut();
    let mut message = ptr::null_mut();
    if LLVMGetTargetFromTriple(triple, &mut target, &mut message) != 0 {
        return Err(take_message(message));
    }
    let empty = CString::new("").unwrap();
    let machine = LLVMCreateTargetMachine(
        target,
        triple,
        empty.as_ptr(),
        empty.as_ptr(),
        LLVMCodeGenOptLevel::LLVMCodeGenLevelNone,
        LLVMRelocMode::LLVMRelocPIC,
        LLVMCodeModel::LLVMCodeModelDefault,
    );
    if machine.is_null() {
        return Err("LLVMCreateTargetMachine failed".into());
    }
    let filename = path_cstring(path)?;
    let failed = LLVMTargetMachineEmitToFile(
        machine,
        module,
        filename.as_ptr().cast_mut(),
        LLVMCodeGenFileType::LLVMObjectFile,
        &mut message,
    );
    LLVMDisposeTargetMachine(machine);
    if failed != 0 {
        return Err(take_message(message));
    }
    Ok(())
}

fn path_cstring(path: &Path) -> Result<CString, String> {
    CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| format!("path contains NUL: {}", path.display()))
}

unsafe fn take_message(message: *mut i8) -> String {
    if message.is_null() {
        return "LLVM operation failed without a diagnostic".into();
    }
    let text = CStr::from_ptr(message).to_string_lossy().into_owned();
    LLVMDisposeMessage(message);
    text
}

pub(super) struct ModuleSymbols {
    definitions: BTreeSet<String>,
    declarations: BTreeSet<String>,
}

impl ModuleSymbols {
    pub(super) fn is_definition(&self, name: &str) -> bool {
        self.definitions.contains(name)
    }

    pub(super) fn is_declaration(&self, name: &str) -> bool {
        self.declarations.contains(name) || is_compiler_runtime(name)
    }
}

pub(super) fn module_symbols(ir: &Path) -> Result<ModuleSymbols, String> {
    unsafe {
        let context = LLVMContextCreate();
        if context.is_null() {
            return Err("LLVMContextCreate failed".into());
        }
        let result = module_symbols_in_context(context, ir);
        LLVMContextDispose(context);
        result
    }
}

unsafe fn module_symbols_in_context(
    context: LLVMContextRef,
    ir: &Path,
) -> Result<ModuleSymbols, String> {
    let ir_path = path_cstring(ir)?;
    let mut buffer: LLVMMemoryBufferRef = ptr::null_mut();
    let mut message = ptr::null_mut();
    if LLVMCreateMemoryBufferWithContentsOfFile(ir_path.as_ptr(), &mut buffer, &mut message) != 0 {
        return Err(take_message(message));
    }
    let mut module: LLVMModuleRef = ptr::null_mut();
    let parsed = LLVMParseIRInContext2(context, buffer, &mut module, &mut message);
    LLVMDisposeMemoryBuffer(buffer);
    if parsed != 0 {
        return Err(take_message(message));
    }
    let mut definitions = BTreeSet::new();
    let mut declarations = BTreeSet::new();
    let mut function = LLVMGetFirstFunction(module);
    while !function.is_null() {
        insert_symbol(function, &mut definitions, &mut declarations);
        function = LLVMGetNextFunction(function);
    }
    let mut global = LLVMGetFirstGlobal(module);
    while !global.is_null() {
        insert_symbol(global, &mut definitions, &mut declarations);
        global = LLVMGetNextGlobal(global);
    }
    LLVMDisposeModule(module);
    Ok(ModuleSymbols {
        definitions,
        declarations,
    })
}

unsafe fn insert_symbol(
    value: LLVMValueRef,
    definitions: &mut BTreeSet<String>,
    declarations: &mut BTreeSet<String>,
) {
    let mut length = 0usize;
    let name = LLVMGetValueName2(value, &mut length);
    if name.is_null() || length == 0 {
        return;
    }
    let name =
        String::from_utf8_lossy(std::slice::from_raw_parts(name.cast::<u8>(), length)).into_owned();
    if LLVMIsDeclaration(value) != 0 {
        declarations.insert(name);
    } else {
        definitions.insert(name);
    }
}

fn is_compiler_runtime(name: &str) -> bool {
    matches!(
        name,
        "memcpy"
            | "memmove"
            | "memset"
            | "__powidf2"
            | "floor"
            | "ceil"
            | "trunc"
            | "round"
            | "sqrt"
            | "sin"
            | "cos"
            | "tan"
            | "exp"
            | "log"
            | "pow"
            | "fmod"
            | "copysign"
            | "fma"
            | "ldexp"
            | "memcmp"
            | "bcmp"
    )
}
