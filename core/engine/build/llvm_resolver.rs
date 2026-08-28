#![allow(unsafe_op_in_unsafe_fn)]

use llvm_sys::{
    LLVMLinkage,
    bit_writer::LLVMWriteBitcodeToFile,
    core::*,
    ir_reader::LLVMParseIRInContext2,
    prelude::{LLVMContextRef, LLVMMemoryBufferRef, LLVMModuleRef, LLVMTypeRef, LLVMValueRef},
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

    let object = out.join("boa_jit_resolver.bc");
    let filename = path_cstring(&object)?;
    if LLVMWriteBitcodeToFile(resolver, filename.as_ptr()) != 0 {
        LLVMDisposeModule(resolver);
        LLVMDisposeModule(original);
        return Err(format!("could not write {}", object.display()));
    }
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
    let (result, parameters) = compiler_runtime_signature(rust_name)?;
    let result = result.llvm_type(context);
    let mut parameters: Vec<_> = parameters
        .iter()
        .map(|parameter| parameter.llvm_type(context))
        .collect();
    let function = LLVMFunctionType(result, parameters.as_mut_ptr(), parameters.len() as u32, 0);
    Some(LLVMAddFunction(module, name, function))
}

#[derive(Clone, Copy)]
enum AbiType {
    Pointer,
    I32,
    I64,
    F64,
}

impl AbiType {
    unsafe fn llvm_type(self, context: LLVMContextRef) -> LLVMTypeRef {
        match self {
            Self::Pointer => LLVMPointerTypeInContext(context, 0),
            Self::I32 => LLVMInt32TypeInContext(context),
            Self::I64 => LLVMInt64TypeInContext(context),
            Self::F64 => LLVMDoubleTypeInContext(context),
        }
    }
}

fn compiler_runtime_signature(name: &str) -> Option<(AbiType, &'static [AbiType])> {
    use AbiType::{F64, I32, I64, Pointer};

    let signature: (AbiType, &'static [AbiType]) = match name {
        "memcpy" | "memmove" => (Pointer, &[Pointer, Pointer, I64]),
        "memset" => (Pointer, &[Pointer, I32, I64]),
        "__powidf2" => (F64, &[F64, I32]),
        "floor" | "ceil" | "trunc" | "round" | "sqrt" | "sin" | "cos" | "tan" | "exp" | "log" => {
            (F64, &[F64])
        }
        "pow" | "fmod" | "copysign" => (F64, &[F64, F64]),
        "fma" => (F64, &[F64, F64, F64]),
        "ldexp" => (F64, &[F64, I32]),
        "memcmp" | "bcmp" => (I32, &[Pointer, Pointer, I64]),
        _ => return None,
    };
    Some(signature)
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

#[allow(dead_code)]
pub(super) struct ModuleSymbols {
    definitions: BTreeSet<String>,
    linkable_definitions: BTreeSet<String>,
    declarations: BTreeSet<String>,
}

#[allow(dead_code)]
impl ModuleSymbols {
    pub(super) fn is_definition(&self, name: &str) -> bool {
        self.definitions.contains(name)
    }

    pub(super) fn is_linkable_definition(&self, name: &str) -> bool {
        self.linkable_definitions.contains(name)
    }

    pub(super) fn is_declaration(&self, name: &str) -> bool {
        self.declarations.contains(name)
    }

    pub(super) fn is_external_abi(&self, name: &str) -> bool {
        compiler_runtime_signature(name).is_some()
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
    let mut linkable_definitions = BTreeSet::new();
    let mut declarations = BTreeSet::new();
    let mut function = LLVMGetFirstFunction(module);
    while !function.is_null() {
        insert_symbol(
            function,
            &mut definitions,
            &mut linkable_definitions,
            &mut declarations,
        );
        function = LLVMGetNextFunction(function);
    }
    let mut global = LLVMGetFirstGlobal(module);
    while !global.is_null() {
        insert_symbol(
            global,
            &mut definitions,
            &mut linkable_definitions,
            &mut declarations,
        );
        global = LLVMGetNextGlobal(global);
    }
    LLVMDisposeModule(module);
    Ok(ModuleSymbols {
        definitions,
        linkable_definitions,
        declarations,
    })
}

unsafe fn insert_symbol(
    value: LLVMValueRef,
    definitions: &mut BTreeSet<String>,
    linkable_definitions: &mut BTreeSet<String>,
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
        let linkage = LLVMGetLinkage(value);
        if matches!(
            linkage,
            LLVMLinkage::LLVMExternalLinkage
                | LLVMLinkage::LLVMLinkOnceAnyLinkage
                | LLVMLinkage::LLVMLinkOnceODRLinkage
                | LLVMLinkage::LLVMWeakAnyLinkage
                | LLVMLinkage::LLVMWeakODRLinkage
        ) {
            linkable_definitions.insert(name.clone());
        }
        definitions.insert(name);
    }
}
