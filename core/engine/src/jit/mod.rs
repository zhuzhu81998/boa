//! Minimal copy-and-patch baseline JIT.
//!
//! A compiled function is an executable allocation containing one generic stencil for every
//! bytecode instruction. Stencils carry no bytecode operands or runtime values. Their sole
//! relocation is patched to the corresponding, already-linked interpreter opcode handler.

use crate::{
    Context,
    vm::{
        CompletionRecord,
        opcode::{Bytecode, InstructionIterator, OPCODE_HANDLERS},
    },
};
use std::{mem::transmute, ops::ControlFlow, ptr};

pub(super) type Emitter = fn(&mut Vec<u8>, &mut Vec<PendingRelocation>) -> usize;

pub(super) struct PendingRelocation {
    offset: usize,
    addend: i64,
    handler: usize,
}

fn emit_stencil(
    code: &mut Vec<u8>,
    relocations: &mut Vec<PendingRelocation>,
    stencil: &[u8],
    relocation_offset: usize,
    addend: i64,
    handler: usize,
) -> usize {
    let padding = code.len().next_multiple_of(16) - code.len();
    code.resize(code.len() + padding, 0x90);
    let entry = code.len();
    code.extend_from_slice(stencil);
    relocations.push(PendingRelocation {
        offset: entry + relocation_offset,
        addend,
        handler,
    });
    entry
}

include!(concat!(env!("OUT_DIR"), "/jit_stencils_generated.rs"));

// The opcode macro deliberately fills the complete byte namespace. This keeps the generated
// emitter library and the current checkout's handler table in lockstep.
const _: [(); 256] = [(); OPCODE_HANDLERS.len()];

type JitEntry = unsafe fn(&mut Context, usize) -> ControlFlow<CompletionRecord>;

/// Executable code for one bytecode array. `entries` is indexed by bytecode PC.
pub(crate) struct JitCode {
    memory: ExecutableMemory,
    entries: Box<[usize]>,
}

impl JitCode {
    pub(crate) fn compile(bytecode: &Bytecode) -> Option<Self> {
        let mut code = Vec::new();
        let mut relocations = Vec::new();
        let mut entries = vec![usize::MAX; bytecode.bytes.len()];
        for (pc, opcode, _) in InstructionIterator::new(bytecode) {
            entries[pc] = EMITTERS[opcode as usize](&mut code, &mut relocations);
        }
        if code.is_empty() {
            return None;
        }
        for relocation in relocations {
            // The typed table reference keeps every handler linked and gives us its post-loader
            // address. No symbol lookup or runtime semantic dispatch remains in the stencil.
            let handler = OPCODE_HANDLERS[relocation.handler] as *const () as usize;
            let value = handler
                .wrapping_add_signed(relocation.addend as isize)
                .to_ne_bytes();
            code[relocation.offset..relocation.offset + value.len()].copy_from_slice(&value);
        }
        Some(Self {
            memory: ExecutableMemory::new(&code)?,
            entries: entries.into_boxed_slice(),
        })
    }

    pub(crate) fn entry(&self, pc: usize) -> Option<JitEntry> {
        let offset = *self.entries.get(pc)?;
        if offset == usize::MAX {
            return None;
        }
        // SAFETY: the entry is a tail-jump stencil to a function with exactly `JitEntry`'s ABI.
        Some(unsafe { transmute(self.memory.as_ptr().add(offset)) })
    }
}

pub(crate) fn execute(
    entry: JitEntry,
    context: &mut Context,
    pc: usize,
) -> ControlFlow<CompletionRecord> {
    // SAFETY: `entry` belongs to the live `JitCode` held by the VM loop.
    unsafe { entry(context, pc) }
}

struct ExecutableMemory {
    pointer: ptr::NonNull<u8>,
    length: usize,
}

impl ExecutableMemory {
    fn new(code: &[u8]) -> Option<Self> {
        let length = code.len();
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
            return None;
        }
        unsafe { ptr::copy_nonoverlapping(code.as_ptr(), raw.cast::<u8>(), length) };
        if unsafe { libc::mprotect(raw, length, libc::PROT_READ | libc::PROT_EXEC) } != 0 {
            unsafe { libc::munmap(raw, length) };
            return None;
        }
        Some(Self {
            pointer: ptr::NonNull::new(raw.cast())?,
            length,
        })
    }

    fn as_ptr(&self) -> *mut u8 {
        self.pointer.as_ptr()
    }
}

impl Drop for ExecutableMemory {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.pointer.as_ptr().cast(), self.length) };
    }
}
