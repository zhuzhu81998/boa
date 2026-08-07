//! Copy-and-patch baseline JIT support.
//!
//! Build-generated emitters copy actual opcode-handler text and record structural relocations.

use crate::{
    Context,
    vm::{
        CompletionRecord,
        opcode::{Bytecode, InstructionIterator, OPCODE_HANDLERS},
    },
};
use std::{fmt, mem::transmute, ops::ControlFlow, ptr};

type JitEntry = unsafe fn(&mut Context, usize) -> ControlFlow<CompletionRecord>;
pub(super) type Emitter = fn(&mut FunctionBuilder) -> Result<usize, JitError>;

#[derive(Clone, Copy)]
#[allow(dead_code)] // Some template objects do not contain every supported ELF kind.
pub(super) enum RelocationKind {
    Relative,
    PltRelative,
    GotRelative,
    Absolute,
}

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
        for relocation in relocations {
            let target = match relocation.target {
                RelocationTarget::External(index) => Target::Absolute(external_address(index)?),
                RelocationTarget::Internal(offset) => Target::Offset(offset),
            };
            let patch_target = match relocation.kind {
                RelocationKind::PltRelative if matches!(target, Target::Absolute(_)) => {
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
    memory: ExecutableMemory,
    entries: Box<[usize]>,
}

impl JitCode {
    pub(crate) fn compile(bytecode: &Bytecode) -> Option<Self> {
        let mut builder = FunctionBuilder::new().ok()?;
        let mut entries = vec![usize::MAX; bytecode.bytes.len()];
        for (pc, opcode, _) in InstructionIterator::new(bytecode) {
            entries[pc] = EMITTERS[opcode as usize](&mut builder).ok()?;
        }
        if builder.code.is_empty() {
            return None;
        }
        Some(Self {
            memory: builder.finish().ok()?,
            entries: entries.into_boxed_slice(),
        })
    }

    pub(crate) fn entry(&self, pc: usize) -> Option<JitEntry> {
        let offset = *self.entries.get(pc)?;
        (offset != usize::MAX).then(|| unsafe { transmute(self.memory.as_ptr().add(offset)) })
    }
}

pub(crate) fn execute(
    entry: JitEntry,
    context: &mut Context,
    pc: usize,
) -> ControlFlow<CompletionRecord> {
    unsafe { entry(context, pc) }
}

struct ExecutableMemory {
    pointer: ptr::NonNull<u8>,
    length: usize,
}

impl ExecutableMemory {
    fn allocate(length: usize) -> Result<Self, JitError> {
        let raw = unsafe {
            libc::mmap(
                ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
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
