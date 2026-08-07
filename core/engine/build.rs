//! Builds the actual opcode-handler stencil library for the baseline JIT.
//!
//! The template artifacts are produced from this exact crate with
//! `boa_jit_template_export`, one codegen unit, no optimization, and panic=abort.  This build
//! script uses the exported handler table to associate opcodes with real handler functions, then
//! extracts their code and complete relocation records.  It never manufactures forwarding stubs.

#[path = "build/archive_closure.rs"]
mod archive_closure;
#[path = "build/llvm_resolver.rs"]
mod llvm_resolver;
#[path = "build/template_archive.rs"]
mod template_archive;

use object::{
    Object, ObjectSection, ObjectSymbol, RelocationEncoding, RelocationKind, RelocationTarget,
    SectionKind, SymbolIndex, SymbolSection,
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env, fs,
    path::{Path, PathBuf},
};

const OPCODE_COUNT: usize = 256;
const HANDLER_TABLE: &str = "BOA_JIT_TEMPLATE_HANDLERS";

fn main() {
    println!("cargo::rerun-if-changed=src/vm/opcode/mod.rs");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(boa_jit_stencils)");
    println!("cargo::rerun-if-env-changed=BOA_JIT_BUILD_TEMPLATE");
    println!("cargo::rerun-if-env-changed=BOA_JIT_TEMPLATE_ARCHIVE");

    if env::var_os("CARGO_FEATURE_JIT").is_none() {
        return;
    }
    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("Cargo must set target arch");
    let family = env::var("CARGO_CFG_TARGET_FAMILY").expect("Cargo must set target family");
    if arch != "x86_64" || family != "unix" {
        println!("cargo::warning=the baseline JIT is currently disabled outside x86-64 Unix");
        return;
    }
    if env::var_os("BOA_JIT_BUILD_TEMPLATE").is_some() {
        return;
    }

    if let Some(archive_path) = env::var_os("BOA_JIT_TEMPLATE_ARCHIVE").map(PathBuf::from) {
        println!("cargo::rerun-if-changed={}", archive_path.display());
        let archive_data = fs::read(&archive_path)
            .unwrap_or_else(|error| panic!("cannot read JIT template archive: {error}"));
        let llvm_ir = env::var_os("BOA_JIT_TEMPLATE_LLVM_IR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                panic!(
                    "jit requires BOA_JIT_TEMPLATE_LLVM_IR pointing to the paired typed LLVM module"
                )
            });
        println!("cargo::rerun-if-changed={}", llvm_ir.display());
        let generated =
            archive_closure::generate(&archive_data, HANDLER_TABLE, OPCODE_COUNT, &llvm_ir)
                .unwrap_or_else(|error| panic!("invalid archive closure: {error}"));
        println!(
            "cargo::warning=JIT archive generator: {} members, {} closure sections, {} bytes, {} relocations, {} externals",
            generated.members,
            generated.sections,
            generated.bytes,
            generated.relocations,
            generated.externals
        );
        let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is missing"));
        println!("cargo::rustc-link-search=native={}", out.display());
        println!("cargo::rustc-link-lib=static=boa_jit_resolver");
        println!("cargo::rustc-cfg=boa_jit_stencils");
        return;
    }

    let object = env::var_os("BOA_JIT_TEMPLATE_OBJECT").map(PathBuf::from).unwrap_or_else(|| {
        panic!("jit requires BOA_JIT_TEMPLATE_OBJECT pointing to the actual boa_engine template object; see core/engine/src/jit/README.md")
    });
    let llvm_ir = env::var_os("BOA_JIT_TEMPLATE_LLVM_IR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            panic!("jit requires BOA_JIT_TEMPLATE_LLVM_IR pointing to the paired typed LLVM module")
        });
    println!("cargo::rerun-if-changed={}", object.display());
    println!("cargo::rerun-if-changed={}", llvm_ir.display());
    generate(&object, &llvm_ir)
        .unwrap_or_else(|error| panic!("invalid JIT template artifacts: {error}"));
    println!("cargo::rustc-cfg=boa_jit_stencils");
}

#[derive(Clone, Copy)]
enum RelocKind {
    Relative,
    PltRelative,
    GotRelative,
    Absolute,
}

struct Reloc {
    offset: u32,
    addend: i64,
    size: u8,
    kind: RelocKind,
    target: SymbolIndex,
}

struct Stencil {
    name: String,
    bytes: Vec<u8>,
    relocations: Vec<Reloc>,
}

struct ClosureSection {
    index: usize,
    address: u64,
    align: u64,
    blob_offset: usize,
    bytes: Vec<u8>,
    relocations: Vec<Reloc>,
}

fn generate(object_path: &Path, llvm_ir: &Path) -> Result<(), String> {
    if !llvm_ir.is_file() {
        return Err(format!("missing LLVM IR: {}", llvm_ir.display()));
    }
    let data = fs::read(object_path).map_err(|e| e.to_string())?;
    let file = object::File::parse(&*data).map_err(|e| e.to_string())?;
    let handlers = handler_symbols(&file)?;
    let mut stencils = Vec::with_capacity(OPCODE_COUNT);
    let mut external_symbols = BTreeMap::<String, usize>::new();

    for handler in handlers {
        let symbol = file.symbol_by_index(handler).map_err(|e| e.to_string())?;
        let name = symbol.name().map_err(|e| e.to_string())?.to_owned();
        let section_index = symbol
            .section_index()
            .ok_or_else(|| format!("handler {name} is undefined"))?;
        let section = file
            .section_by_index(section_index)
            .map_err(|e| e.to_string())?;
        let section_data = section.data().map_err(|e| e.to_string())?;
        let start = (symbol.address() - section.address()) as usize;
        let size = symbol.size() as usize;
        let bytes = section_data
            .get(start..start + size)
            .ok_or_else(|| format!("invalid code range for {name}"))?
            .to_vec();
        let mut relocations = Vec::new();
        for (offset, relocation) in section.relocations() {
            let offset = offset as usize;
            if !(start..start + size).contains(&offset) {
                continue;
            }
            let RelocationTarget::Symbol(target) = relocation.target() else {
                return Err(format!("{name}: non-symbol relocation at {offset:#x}"));
            };
            let target_symbol = file.symbol_by_index(target).map_err(|e| e.to_string())?;
            if matches!(target_symbol.section(), SymbolSection::Undefined) {
                let target_name = target_symbol.name().map_err(|e| e.to_string())?;
                if target_name.is_empty() {
                    return Err(format!("{name}: unnamed external relocation"));
                }
                external_symbols.entry(target_name.to_owned()).or_insert(0);
            }
            let kind = match relocation.kind() {
                RelocationKind::Relative => RelocKind::Relative,
                RelocationKind::PltRelative => RelocKind::PltRelative,
                RelocationKind::GotRelative => RelocKind::GotRelative,
                RelocationKind::Absolute => RelocKind::Absolute,
                other => return Err(format!("{name}: unsupported relocation kind {other:?}")),
            };
            if relocation.encoding() != RelocationEncoding::Generic
                && relocation.encoding() != RelocationEncoding::X86RipRelative
            {
                return Err(format!(
                    "{name}: unsupported relocation encoding {:?}",
                    relocation.encoding()
                ));
            }
            relocations.push(Reloc {
                offset: (offset - start)
                    .try_into()
                    .map_err(|_| "relocation offset overflow")?,
                addend: relocation.addend(),
                size: relocation.size(),
                kind,
                target,
            });
        }
        stencils.push(Stencil {
            name,
            bytes,
            relocations,
        });
    }

    let (mut closure, externalized) = collect_closure(&file, &stencils, &mut external_symbols)?;
    layout_closure(&mut closure)?;

    for (next, value) in external_symbols.values_mut().enumerate() {
        *value = next;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is missing")?);
    let names: Vec<String> = external_symbols.keys().cloned().collect();
    llvm_resolver::generate(llvm_ir, &names, &out)?;
    println!("cargo::rustc-link-search=native={}", out.display());
    println!("cargo::rustc-link-lib=static=boa_jit_resolver");
    emit_metadata(&file, &stencils, &closure, &externalized, &external_symbols)
}

fn queue_target(
    file: &object::File<'_>,
    symbol_index: SymbolIndex,
    pending: &mut VecDeque<usize>,
    external: &mut BTreeMap<String, usize>,
    externalized: &mut BTreeSet<usize>,
) -> Result<(), String> {
    let symbol = file
        .symbol_by_index(symbol_index)
        .map_err(|e| e.to_string())?;
    if matches!(symbol.section(), SymbolSection::Undefined) {
        let name = symbol.name().map_err(|e| e.to_string())?;
        if name.is_empty() {
            return Err("unnamed external relocation".into());
        }
        external.entry(name.to_owned()).or_insert(0);
        return Ok(());
    }
    let section_index = symbol
        .section_index()
        .ok_or("defined target without section")?;
    let section = file
        .section_by_index(section_index)
        .map_err(|e| e.to_string())?;
    let section_name = section.name().unwrap_or("<unnamed>");
    let kind_ok = matches!(
        section.kind(),
        SectionKind::Text | SectionKind::ReadOnlyData | SectionKind::ReadOnlyString
    ) || (section.kind() == SectionKind::Data
        && section_name.starts_with(".data.rel.ro"));
    let relocations_ok = section.relocations().all(|(_, relocation)| {
        matches!(
            relocation.kind(),
            RelocationKind::Relative
                | RelocationKind::PltRelative
                | RelocationKind::GotRelative
                | RelocationKind::Absolute
        ) && matches!(
            relocation.encoding(),
            RelocationEncoding::Generic | RelocationEncoding::X86RipRelative
        )
    });
    if kind_ok && relocations_ok {
        pending.push_back(section_index.0);
        return Ok(());
    }
    if section.kind() != SectionKind::Text {
        return Err(format!(
            "internal closure reaches unsupported section {section_name:?} ({:?})",
            section.kind()
        ));
    }
    let name = symbol.name().map_err(|e| e.to_string())?;
    if name.is_empty() {
        return Err(format!(
            "cannot externalize unnamed helper in {section_name:?}"
        ));
    }
    external.entry(name.to_owned()).or_insert(0);
    externalized.insert(symbol_index.0);
    Ok(())
}

fn collect_closure(
    file: &object::File<'_>,
    stencils: &[Stencil],
    external: &mut BTreeMap<String, usize>,
) -> Result<(Vec<ClosureSection>, BTreeSet<usize>), String> {
    let mut pending = VecDeque::new();
    let mut externalized = BTreeSet::new();
    for stencil in stencils {
        for relocation in &stencil.relocations {
            queue_target(
                file,
                relocation.target,
                &mut pending,
                external,
                &mut externalized,
            )?;
        }
    }

    let mut seen = BTreeSet::new();
    let mut closure = Vec::new();
    while let Some(raw_index) = pending.pop_front() {
        if !seen.insert(raw_index) {
            continue;
        }
        let index = object::SectionIndex(raw_index);
        let section = file.section_by_index(index).map_err(|e| e.to_string())?;
        let name = section.name().unwrap_or("<unnamed>");
        let permitted = matches!(
            section.kind(),
            SectionKind::Text | SectionKind::ReadOnlyData | SectionKind::ReadOnlyString
        ) || (section.kind() == SectionKind::Data
            && name.starts_with(".data.rel.ro"));
        if !permitted {
            return Err(format!(
                "internal closure reaches unsupported section {name:?} ({:?})",
                section.kind()
            ));
        }
        let bytes = section.data().map_err(|e| format!("{name}: {e}"))?.to_vec();
        let mut relocations = Vec::new();
        for (offset, relocation) in section.relocations() {
            let RelocationTarget::Symbol(target_index) = relocation.target() else {
                return Err(format!("{name}: non-symbol relocation at {offset:#x}"));
            };
            queue_target(
                file,
                target_index,
                &mut pending,
                external,
                &mut externalized,
            )?;
            let kind = match relocation.kind() {
                RelocationKind::Relative => RelocKind::Relative,
                RelocationKind::PltRelative => RelocKind::PltRelative,
                RelocationKind::GotRelative => RelocKind::GotRelative,
                RelocationKind::Absolute => RelocKind::Absolute,
                other => {
                    return Err(format!(
                        "{name}: unsupported relocation kind {other:?} at {offset:#x}"
                    ));
                }
            };
            if relocation.encoding() != RelocationEncoding::Generic
                && relocation.encoding() != RelocationEncoding::X86RipRelative
            {
                return Err(format!(
                    "{name}: unsupported relocation encoding {:?} at {offset:#x}",
                    relocation.encoding()
                ));
            }
            relocations.push(Reloc {
                offset: offset
                    .try_into()
                    .map_err(|_| format!("{name}: relocation offset overflow"))?,
                addend: relocation.addend(),
                size: relocation.size(),
                kind,
                target: target_index,
            });
        }
        closure.push(ClosureSection {
            index: raw_index,
            address: section.address(),
            align: section.align().max(1),
            blob_offset: 0,
            bytes,
            relocations,
        });
    }
    closure.sort_by_key(|section| section.index);
    Ok((closure, externalized))
}

fn layout_closure(closure: &mut [ClosureSection]) -> Result<(), String> {
    let mut offset = 0usize;
    for section in closure {
        let align: usize = section
            .align
            .try_into()
            .map_err(|_| "closure alignment overflow")?;
        if !align.is_power_of_two() {
            return Err(format!(
                "section {} has invalid alignment {align}",
                section.index
            ));
        }
        offset = offset
            .checked_add(align - 1)
            .ok_or("closure layout overflow")?
            & !(align - 1);
        section.blob_offset = offset;
        offset = offset
            .checked_add(section.bytes.len())
            .ok_or("closure layout overflow")?;
    }
    Ok(())
}

fn internal_offset(
    file: &object::File<'_>,
    closure: &[ClosureSection],
    symbol: SymbolIndex,
) -> Result<usize, String> {
    let target = file.symbol_by_index(symbol).map_err(|e| e.to_string())?;
    let section_index = target
        .section_index()
        .ok_or("internal target without section")?;
    let section = closure
        .iter()
        .find(|entry| entry.index == section_index.0)
        .ok_or_else(|| {
            format!(
                "internal target section {} missing from closure",
                section_index.0
            )
        })?;
    let within = target
        .address()
        .checked_sub(section.address)
        .ok_or("internal symbol precedes section")? as usize;
    if within > section.bytes.len() {
        return Err(format!(
            "internal symbol lies outside section {}",
            section.index
        ));
    }
    section
        .blob_offset
        .checked_add(within)
        .ok_or_else(|| "internal target offset overflow".into())
}
fn handler_symbols(file: &object::File<'_>) -> Result<Vec<SymbolIndex>, String> {
    let table = file
        .symbols()
        .find(|s| s.name().ok() == Some(HANDLER_TABLE))
        .ok_or_else(|| {
            format!("missing {HANDLER_TABLE}; compile with --cfg boa_jit_template_export")
        })?;
    if table.size() != (OPCODE_COUNT * size_of::<usize>()) as u64 {
        return Err(format!(
            "{HANDLER_TABLE} has size {}, expected {}",
            table.size(),
            OPCODE_COUNT * size_of::<usize>()
        ));
    }
    let section_index = table
        .section_index()
        .ok_or("handler table has no section")?;
    let section = file
        .section_by_index(section_index)
        .map_err(|e| e.to_string())?;
    let start = table.address() - section.address();
    let end = start + table.size();
    let mut entries = Vec::new();
    for (offset, relocation) in section.relocations() {
        if !(start..end).contains(&offset) {
            continue;
        }
        let RelocationTarget::Symbol(symbol) = relocation.target() else {
            return Err("handler table contains a non-symbol relocation".into());
        };
        entries.push((offset, symbol));
    }
    entries.sort_by_key(|(offset, _)| *offset);
    if entries.len() != OPCODE_COUNT {
        return Err(format!(
            "handler table has {} relocations, expected {OPCODE_COUNT}",
            entries.len()
        ));
    }
    Ok(entries.into_iter().map(|(_, symbol)| symbol).collect())
}

fn target_expr(
    file: &object::File<'_>,
    closure: &[ClosureSection],
    externalized: &BTreeSet<usize>,
    external: &BTreeMap<String, usize>,
    symbol: SymbolIndex,
) -> Result<String, String> {
    let target = file.symbol_by_index(symbol).map_err(|e| e.to_string())?;
    if matches!(target.section(), SymbolSection::Undefined) || externalized.contains(&symbol.0) {
        let name = target.name().map_err(|e| e.to_string())?;
        let index = external
            .get(name)
            .ok_or_else(|| format!("external symbol {name:?} missing from resolver table"))?;
        Ok(format!("RelocationTarget::External({index})"))
    } else {
        Ok(format!(
            "RelocationTarget::Internal({})",
            internal_offset(file, closure, symbol)?
        ))
    }
}

fn reloc_kind_name(kind: RelocKind) -> &'static str {
    match kind {
        RelocKind::Relative => "Relative",
        RelocKind::PltRelative => "PltRelative",
        RelocKind::GotRelative => "GotRelative",
        RelocKind::Absolute => "Absolute",
    }
}
fn emit_metadata(
    file: &object::File<'_>,
    stencils: &[Stencil],
    closure: &[ClosureSection],
    externalized: &BTreeSet<usize>,
    external: &BTreeMap<String, usize>,
) -> Result<(), String> {
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is missing")?);
    let mut stencil_blob = Vec::new();
    let closure_len = closure
        .iter()
        .map(|section| section.blob_offset + section.bytes.len())
        .max()
        .unwrap_or(0);
    let mut closure_blob = vec![0u8; closure_len];
    let mut generated = String::from("// @generated from actual boa_engine opcode handlers.\n");
    generated.push_str("pub(super) static EXTERNAL_NAMES: &[&str] = &[\n");
    for name in external.keys() {
        generated.push_str(&format!("    {name:?},\n"));
    }
    generated.push_str("];\n\n");
    generated.push_str(&format!(
        "unsafe extern \"C\" {{ static BOA_JIT_EXTERNAL_SYMBOLS: [usize; {}]; }}\n\n",
        external.len()
    ));

    generated.push_str("static INTERNAL_CLOSURE_RELOCS: &[StencilRelocation] = &[\n");
    for section in closure {
        closure_blob[section.blob_offset..section.blob_offset + section.bytes.len()]
            .copy_from_slice(&section.bytes);
        for relocation in &section.relocations {
            let offset = section
                .blob_offset
                .checked_add(relocation.offset as usize)
                .ok_or("closure relocation offset overflow")?;
            let target = target_expr(file, closure, externalized, external, relocation.target)?;
            let kind = reloc_kind_name(relocation.kind);
            generated.push_str(&format!("    StencilRelocation {{ offset: {offset}, addend: {}, size: {}, kind: RelocationKind::{kind}, target: {target} }},\n", relocation.addend, relocation.size));
        }
    }
    generated.push_str("];\n\n");

    for (opcode, stencil) in stencils.iter().enumerate() {
        let blob_start = stencil_blob.len();
        stencil_blob.extend_from_slice(&stencil.bytes);
        generated.push_str(&format!("// opcode {opcode}: {}\n", stencil.name));
        generated.push_str(&format!(
            "static RELOCS_{opcode:03}: &[StencilRelocation] = &[\n"
        ));
        for relocation in &stencil.relocations {
            let target = target_expr(file, closure, externalized, external, relocation.target)?;
            let kind = reloc_kind_name(relocation.kind);
            generated.push_str(&format!("    StencilRelocation {{ offset: {}, addend: {}, size: {}, kind: RelocationKind::{kind}, target: {target} }},\n", relocation.offset, relocation.addend, relocation.size));
        }
        generated.push_str("];\n");
        generated.push_str(&format!("fn emit_{opcode:03}(out: &mut FunctionBuilder) -> Result<usize, JitError> {{ out.append_stencil(&STENCIL_BLOB[{blob_start}..{}], RELOCS_{opcode:03}) }}\n\n", stencil_blob.len()));
    }
    generated.push_str("pub(super) static EMITTERS: [Emitter; 256] = [\n");
    for opcode in 0..OPCODE_COUNT {
        generated.push_str(&format!("    emit_{opcode:03},\n"));
    }
    generated.push_str("];\n");
    fs::write(out.join("jit_stencils.bin"), stencil_blob).map_err(|e| e.to_string())?;
    fs::write(out.join("jit_internal_closure.bin"), closure_blob).map_err(|e| e.to_string())?;
    fs::write(out.join("jit_stencils_generated.rs"), generated).map_err(|e| e.to_string())
}
