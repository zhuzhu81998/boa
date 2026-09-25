//! Copy-and-patch baseline JIT support.
//!
//! Build-generated emitters copy actual opcode-handler text and record structural relocations.

use crate::{
    Context,
    vm::{
        CodeBlock, CompletionRecord,
        opcode::{
            Bytecode, InstructionIterator, JitChain, JitChainEntry, JitCodeView, OPCODE_HANDLERS,
        },
    },
};
use std::{fmt, mem::transmute, ops::ControlFlow, ptr};

/// Counts are local to one synchronous VM run; reentrant runs report separately.
pub(crate) struct RunDiagnostics {
    enabled: bool,
    chains: u64,
    native_opcodes: u64,
    frame_transfers: u64,
    interpreter_opcodes: u64,
}

impl RunDiagnostics {
    pub(crate) fn new() -> Self {
        Self {
            enabled: std::env::var_os("BOA_JIT_TRACE").is_some(),
            chains: 0,
            native_opcodes: 0,
            frame_transfers: 0,
            interpreter_opcodes: 0,
        }
    }

    pub(crate) fn interpreter(&mut self) {
        if self.enabled {
            self.interpreter_opcodes = self.interpreter_opcodes.saturating_add(1);
        }
    }
}

impl Drop for RunDiagnostics {
    fn drop(&mut self) {
        if self.enabled {
            eprintln!(
                "JIT run summary: native_opcodes={} native_chains={} native_frame_transfers={} interpreter_opcodes={} (nested runs reported separately)",
                self.native_opcodes, self.chains, self.frame_transfers, self.interpreter_opcodes
            );
        }
    }
}

/// A run retains every compiled caller/callee, including failed compilation attempts.
/// Owning a GC root prevents pointer reuse while a code block is used as a cache key.
#[derive(Default)]
pub(crate) struct JitCache {
    code: std::collections::HashMap<*const CodeBlock, (boa_gc::Gc<CodeBlock>, Option<JitCode>)>,
    views: Vec<JitCodeView>,
}

impl JitCache {
    pub(crate) fn entry(
        &mut self,
        block: &boa_gc::Gc<CodeBlock>,
        pc: usize,
        frame_depth: usize,
    ) -> Option<JitInvocation<'_>> {
        let (_, code) = self
            .code
            .entry(ptr::from_ref(&**block))
            .or_insert_with(|| (block.clone(), JitCode::compile(&block.bytecode)));
        let code = code.as_ref()?;
        let view = JitCodeView {
            code_block: ptr::from_ref(&**block),
            entries: code.entries.as_ptr(),
            length: code.entries.len(),
        };
        if let Err(index) = self
            .views
            .binary_search_by_key(&(view.code_block as usize), |view| view.code_block as usize)
        {
            self.views.insert(index, view);
        }
        Some(JitInvocation {
            _cache: std::marker::PhantomData,
            entry: code.entry(pc)?,
            chain: JitChain {
                active: std::cell::Cell::new(view),
                frame_depth: std::cell::Cell::new(frame_depth),
                cached: self.views.as_ptr(),
                cached_length: self.views.len(),
                executed: ptr::null_mut(),
                frame_transfers: ptr::null_mut(),
            },
        })
    }
}

pub(crate) struct JitInvocation<'cache> {
    _cache: std::marker::PhantomData<&'cache JitCache>,
    entry: JitChainEntry,
    chain: JitChain,
}
pub(super) type Emitter = fn(&mut FunctionBuilder) -> Result<usize, JitError>;

#[derive(Clone, Copy)]
#[allow(dead_code)] // Some template objects do not contain every supported ELF kind.
pub(super) enum RelocationKind {
    Relative,
    PltRelative,
    GotRelative,
    Absolute,
}

#[allow(dead_code)]
#[derive(Clone, Copy)]
pub(super) enum RelocationTarget {
    Internal(usize),
    External(usize),
}

#[derive(Clone, Copy)]
pub(super) struct StencilRelocation {
    offset: u32,
    addend: i64,
    size: u8,
    kind: RelocationKind,
    target: RelocationTarget,
}

#[derive(Debug)]
pub(super) enum JitError {
    MissingExternal(&'static str),
    InvalidRelocation { offset: usize, size: u8 },
    RelocationOverflow { offset: usize, size: u8 },
    Allocation,
}

impl fmt::Display for JitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingExternal(name) => write!(f, "unresolved stencil external {name}"),
            Self::InvalidRelocation { offset, size } => {
                write!(f, "invalid {size}-bit stencil relocation at {offset:#x}")
            }
            Self::RelocationOverflow { offset, size } => {
                write!(f, "{size}-bit stencil relocation at {offset:#x} overflowed")
            }
            Self::Allocation => f.write_str("could not allocate executable JIT memory"),
        }
    }
}

struct PendingRelocation {
    field: usize,
    addend: i64,
    size: u8,
    kind: RelocationKind,
    target: RelocationTarget,
}

pub(super) struct FunctionBuilder {
    code: Vec<u8>,
    relocations: Vec<PendingRelocation>,
}

impl FunctionBuilder {
    fn new() -> Result<Self, JitError> {
        let mut builder = Self {
            code: Vec::new(),
            relocations: Vec::new(),
        };
        let closure = builder.append_stencil(INTERNAL_CLOSURE_BLOB, INTERNAL_CLOSURE_RELOCS)?;
        debug_assert_eq!(closure, 0);
        Ok(builder)
    }

    pub(super) fn append_stencil(
        &mut self,
        stencil: &[u8],
        relocations: &[StencilRelocation],
    ) -> Result<usize, JitError> {
        self.code.resize(self.code.len().next_multiple_of(16), 0x90);
        let entry = self.code.len();
        self.code.extend_from_slice(stencil);
        for relocation in relocations {
            let field = entry + relocation.offset as usize;
            let bytes = usize::from(relocation.size / 8);
            if relocation.size % 8 != 0
                || !matches!(bytes, 1 | 2 | 4 | 8)
                || field
                    .checked_add(bytes)
                    .is_none_or(|end| end > self.code.len())
            {
                return Err(JitError::InvalidRelocation {
                    offset: field,
                    size: relocation.size,
                });
            }
            self.relocations.push(PendingRelocation {
                field,
                addend: relocation.addend,
                size: relocation.size,
                kind: relocation.kind,
                target: relocation.target,
            });
        }
        Ok(entry)
    }

    fn finish(mut self) -> Result<ExecutableMemory, JitError> {
        let relocations = std::mem::take(&mut self.relocations);
        let mut resolved = Vec::with_capacity(relocations.len());
        let mut direct_calls = Vec::new();
        for relocation in relocations {
            let target = match relocation.target {
                RelocationTarget::External(index) => Target::Absolute(external_address(index)?),
                RelocationTarget::Internal(offset) => Target::Offset(offset),
            };
            let patch_target = match relocation.kind {
                RelocationKind::PltRelative if matches!(target, Target::Absolute(_)) => {
                    if relocation.size == 32
                        && let Target::Absolute(address) = target
                    {
                        direct_calls.push((relocation.field, relocation.addend, address));
                    }
                    let (island, pointer) = self.append_pointer_cell(true);
                    resolved.push((pointer, 0, 64, RelocationKind::Absolute, target));
                    Target::Offset(island)
                }
                RelocationKind::GotRelative => {
                    let (cell, pointer) = self.append_pointer_cell(false);
                    debug_assert_eq!(cell, pointer);
                    resolved.push((pointer, 0, 64, RelocationKind::Absolute, target));
                    Target::Offset(cell)
                }
                RelocationKind::Relative
                | RelocationKind::PltRelative
                | RelocationKind::Absolute => target,
            };
            resolved.push((
                relocation.field,
                relocation.addend,
                relocation.size,
                relocation.kind,
                patch_target,
            ));
        }
        let mut memory = ExecutableMemory::allocate(self.code.len())?;
        let base = memory.as_ptr() as usize;
        for (field, addend, size, kind, target) in resolved {
            let target = match target {
                Target::Absolute(value) => value,
                Target::Offset(offset) => base + offset,
            };
            let value = match kind {
                RelocationKind::Relative
                | RelocationKind::PltRelative
                | RelocationKind::GotRelative => {
                    target as i128 + addend as i128 - (base + field) as i128
                }
                RelocationKind::Absolute => target as i128 + addend as i128,
            };
            write_relocation(&mut self.code, field, size, value)?;
        }
        // Keep the reserved island as the fallback, but bypass it whenever the actual helper
        // is reachable. GOT/data references remain pointer cells, not branch trampolines.
        for (field, addend, target) in direct_calls {
            let displacement = target as i128 + addend as i128 - (base + field) as i128;
            if i32::try_from(displacement).is_ok() {
                write_relocation(&mut self.code, field, 32, displacement)?;
            }
        }
        memory.write_and_make_executable(&self.code)?;
        Ok(memory)
    }

    fn append_pointer_cell(&mut self, branch_island: bool) -> (usize, usize) {
        self.code.resize(self.code.len().next_multiple_of(8), 0);
        let entry = self.code.len();
        if branch_island {
            self.code.extend_from_slice(&[0xff, 0x25, 0, 0, 0, 0]);
        }
        let pointer = self.code.len();
        self.code.extend_from_slice(&[0; size_of::<usize>()]);
        (entry, pointer)
    }
}

#[derive(Clone, Copy)]
enum Target {
    Absolute(usize),
    Offset(usize),
}

fn write_relocation(code: &mut [u8], offset: usize, size: u8, value: i128) -> Result<(), JitError> {
    let width = usize::from(size / 8);
    let fits = if size == 64 {
        value >= 0 && value <= u64::MAX as i128
    } else {
        let shift = 128 - u32::from(size);
        (value << shift >> shift) == value
    };
    if !fits {
        return Err(JitError::RelocationOverflow { offset, size });
    }
    let bytes = (value as u64).to_le_bytes();
    code[offset..offset + width].copy_from_slice(&bytes[..width]);
    Ok(())
}

static STENCIL_BLOB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/jit_stencils.bin"));
static INTERNAL_CLOSURE_BLOB: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/jit_internal_closure.bin"));
include!(concat!(env!("OUT_DIR"), "/jit_stencils_generated.rs"));

const _: [(); 256] = [(); OPCODE_HANDLERS.len()];

fn external_address(index: usize) -> Result<usize, JitError> {
    // The typed LLVM resolver-object stage will provide this exact-name table. Never use dlsym:
    // the native linker must retain and resolve every external dependency before Boa starts.
    let name = EXTERNAL_NAMES
        .get(index)
        .copied()
        .ok_or(JitError::MissingExternal("<invalid external index>"))?;
    let address = unsafe { BOA_JIT_EXTERNAL_SYMBOLS[index] };
    if address == 0 {
        return Err(JitError::MissingExternal(name));
    }
    Ok(address)
}

pub(crate) struct JitCode {
    // Entries point into this mapping; keep ownership even though lookup uses absolute addresses.
    _memory: ExecutableMemory,
    entries: Box<[usize]>,
}

impl JitCode {
    pub(crate) fn compile(bytecode: &Bytecode) -> Option<Self> {
        match Self::try_compile(bytecode) {
            Ok(code) => {
                if std::env::var_os("BOA_JIT_TRACE").is_some() {
                    let entries = code.as_ref().map_or(0, |code| {
                        code.entries.iter().filter(|&&entry| entry != 0).count()
                    });
                    eprintln!("JIT compiled {entries} bytecode entries");
                }
                code
            }
            Err(error) => {
                if std::env::var_os("BOA_JIT_TRACE").is_some() {
                    eprintln!("JIT compilation failed: {error}");
                }
                None
            }
        }
    }

    fn try_compile(bytecode: &Bytecode) -> Result<Option<Self>, JitError> {
        let mut builder = FunctionBuilder::new()?;
        let mut entries = vec![usize::MAX; bytecode.bytes.len()];
        for (pc, opcode, _) in InstructionIterator::new(bytecode) {
            if let Some(emitter) = EMITTERS[opcode as usize] {
                entries[pc] = emitter(&mut builder)?;
            }
        }
        if entries.iter().all(|&entry| entry == usize::MAX) {
            return Ok(None);
        }
        let memory = builder.finish()?;
        for entry in &mut entries {
            *entry = if *entry == usize::MAX {
                0
            } else {
                memory.as_ptr() as usize + *entry
            };
        }
        Ok(Some(Self {
            _memory: memory,
            entries: entries.into_boxed_slice(),
        }))
    }

    pub(crate) fn entry(&self, pc: usize) -> Option<JitChainEntry> {
        let address = *self.entries.get(pc)?;
        (address != 0).then(|| unsafe { transmute(address) })
    }
}

pub(crate) fn execute(
    mut invocation: JitInvocation<'_>,
    context: &mut Context,
    pc: usize,
    diagnostics: &mut RunDiagnostics,
) -> ControlFlow<CompletionRecord> {
    let mut executed = 0_u64;
    let mut frame_transfers = 0_u64;
    if diagnostics.enabled {
        invocation.chain.executed = &mut executed;
        invocation.chain.frame_transfers = &mut frame_transfers;
        eprintln!(
            "JIT entering native chain at pc {pc}, entry={:p}",
            invocation.entry as *const ()
        );
    }
    let mut output = std::mem::MaybeUninit::uninit();
    // Every chain exit writes exactly one result. The cache outlives this synchronous call,
    // including any reentrant VM runs, and keeps its executable mappings and GC roots alive.
    let result = unsafe {
        (invocation.entry)(context, pc, &invocation.chain, output.as_mut_ptr());
        output.assume_init()
    };
    if diagnostics.enabled {
        diagnostics.chains = diagnostics.chains.saturating_add(1);
        diagnostics.native_opcodes = diagnostics.native_opcodes.saturating_add(executed);
        diagnostics.frame_transfers = diagnostics.frame_transfers.saturating_add(frame_transfers);
        eprintln!(
            "JIT returned from native chain: native_opcodes={executed} native_frame_transfers={frame_transfers}"
        );
    }
    result
}

struct ExecutableMemory {
    pointer: ptr::NonNull<u8>,
    length: usize,
}

impl ExecutableMemory {
    fn allocate(length: usize) -> Result<Self, JitError> {
        // A non-fixed hint cannot overwrite an existing mapping. The OS may place this
        // elsewhere; out-of-range helper branches still have their reserved trampolines.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page_size = usize::try_from(page_size)
            .ok()
            .filter(|&size| size != 0)
            .unwrap_or(4096);
        let anchor = OPCODE_HANDLERS[0] as *const () as usize;
        let hint = (anchor / page_size * page_size).saturating_add(64 * 1024 * 1024);
        let mut raw = unsafe {
            libc::mmap(
                hint as *mut libc::c_void,
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if raw == libc::MAP_FAILED {
            raw = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    length,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
        }
        if raw == libc::MAP_FAILED {
            return Err(JitError::Allocation);
        }
        Ok(Self {
            pointer: ptr::NonNull::new(raw.cast()).ok_or(JitError::Allocation)?,
            length,
        })
    }

    fn write_and_make_executable(&mut self, code: &[u8]) -> Result<(), JitError> {
        unsafe { ptr::copy_nonoverlapping(code.as_ptr(), self.pointer.as_ptr(), code.len()) };
        if unsafe {
            libc::mprotect(
                self.pointer.as_ptr().cast(),
                self.length,
                libc::PROT_READ | libc::PROT_EXEC,
            )
        } != 0
        {
            return Err(JitError::Allocation);
        }
        Ok(())
    }

    fn as_ptr(&self) -> *mut u8 {
        self.pointer.as_ptr()
    }
}

impl Drop for ExecutableMemory {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.pointer.as_ptr().cast(), self.length);
        }
    }
}
