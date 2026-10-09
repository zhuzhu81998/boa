use super::llvm_resolver;
use object::{
    Object, ObjectSection, ObjectSymbol, RelocationEncoding, RelocationKind, RelocationTarget,
    SectionKind, SymbolIndex,
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy)]
enum Kind {
    Relative,
    PltRelative,
    GotRelative,
    Absolute,
}

#[derive(Clone)]
enum Target {
    Internal(SymbolIndex),
    External(String),
}

#[derive(Clone)]
struct Reloc {
    offset: u32,
    addend: i64,
    size: u8,
    kind: Kind,
    target: Target,
}

struct Stencil {
    root: SymbolIndex,
    name: String,
    bytes: Vec<u8>,
    relocations: Vec<Reloc>,
    unsupported: Option<String>,
}

struct BundleSection {
    id: usize,
    address: u64,
    align: u64,
    blob_offset: usize,
    bytes: Vec<u8>,
    relocations: Vec<Reloc>,
}

struct StencilBundle {
    sections: Vec<BundleSection>,
    alignment: u64,
}

pub(super) struct Generated {
    pub(super) sections: usize,
    pub(super) bytes: usize,
    pub(super) relocations: usize,
    pub(super) externals: usize,
    pub(super) supported: usize,
}

pub(super) fn generate(
    file: &object::File<'_>,
    handler_table: &str,
    opcode_count: usize,
    llvm_ir: &Path,
    runtime_ir: &Path,
    holes: &BTreeMap<String, String>,
) -> Result<Generated, String> {
    let table = file
        .symbol_by_name(handler_table)
        .ok_or("handler table absent from template object")?;
    let module_symbols = llvm_resolver::module_symbols(llvm_ir)?;
    let roots = handler_roots(file, table.index(), opcode_count)?;
    let stencils = extract_stencils(file, &roots)?;
    let mut external_names = BTreeSet::new();
    let mut bundles = Vec::with_capacity(stencils.len());
    for (opcode, stencil) in stencils.iter().enumerate() {
        let mut opcode_externals = BTreeSet::new();
        let result = match &stencil.unsupported {
            Some(error) => Err(error.clone()),
            None => collect_bundle(file, stencil, &mut opcode_externals, &module_symbols),
        };
        match result {
            Ok(bundle) => {
                external_names.extend(opcode_externals);
                bundles.push(Some(bundle));
            }
            Err(error) => {
                println!(
                    "cargo::warning=JIT opcode {opcode} ({}) unsupported: {error}",
                    stencil.name
                );
                bundles.push(None);
            }
        }
    }
    let external: BTreeMap<String, usize> = external_names
        .into_iter()
        .enumerate()
        .map(|(index, name)| (name, index))
        .collect();
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is missing")?);
    let names: Vec<String> = external
        .keys()
        .map(|name| holes.get(name).unwrap_or(name).clone())
        .collect();
    llvm_resolver::generate(runtime_ir, &names, &out)?;
    fs::write(out.join("jit_runtime_holes.txt"), names.join("\n")).map_err(|e| e.to_string())?;
    emit_metadata(file, &stencils, &bundles, &external, &out)?;

    Ok(Generated {
        sections: bundles
            .iter()
            .flatten()
            .map(|bundle| bundle.sections.len())
            .sum(),
        bytes: bundles
            .iter()
            .flatten()
            .flat_map(|bundle| &bundle.sections)
            .map(|section| section.bytes.len())
            .sum(),
        relocations: bundles
            .iter()
            .flatten()
            .flat_map(|bundle| &bundle.sections)
            .map(|section| section.relocations.len())
            .sum(),
        externals: external.len(),
        supported: bundles.iter().flatten().count(),
    })
}

fn handler_roots(
    file: &object::File<'_>,
    table: SymbolIndex,
    opcode_count: usize,
) -> Result<Vec<SymbolIndex>, String> {
    let symbol = file.symbol_by_index(table).map_err(|e| e.to_string())?;
    let section_index = symbol
        .section_index()
        .ok_or("handler table has no section")?;
    let section = file
        .section_by_index(section_index)
        .map_err(|e| e.to_string())?;
    let start = symbol.address() - section.address();
    let end = start + symbol.size();
    let mut roots = Vec::new();
    for (offset, relocation) in section.relocations() {
        if !(start..end).contains(&offset) {
            continue;
        }
        let RelocationTarget::Symbol(index) = relocation.target() else {
            return Err("non-symbol handler entry".into());
        };
        let handler = file.symbol_by_index(index).map_err(|e| e.to_string())?;
        if !handler.is_definition() {
            return Err(format!(
                "handler {:?} is undefined",
                handler.name().unwrap_or("<unnamed>")
            ));
        }
        roots.push((offset, index));
    }
    roots.sort_by_key(|(offset, _)| *offset);
    if roots.len() != opcode_count {
        return Err(format!(
            "handler table has {} entries, expected {opcode_count}",
            roots.len()
        ));
    }
    Ok(roots.into_iter().map(|(_, symbol)| symbol).collect())
}

fn extract_stencils(
    file: &object::File<'_>,
    roots: &[SymbolIndex],
) -> Result<Vec<Stencil>, String> {
    let mut stencils = Vec::with_capacity(roots.len());
    for &root in roots {
        let symbol = file
            .symbol_by_index(root)
            .map_err(|e| e.to_string())?;
        let name = symbol.name().map_err(|e| e.to_string())?.to_owned();
        let section_index = symbol
            .section_index()
            .ok_or_else(|| format!("handler {name:?} has no section"))?;
        let section = file
            .section_by_index(section_index)
            .map_err(|e| e.to_string())?;
        validate_section(&section)?;
        let section_data = section.data().map_err(|e| e.to_string())?;
        let start = (symbol.address() - section.address()) as usize;
        let end = start
            .checked_add(symbol.size() as usize)
            .ok_or("handler range overflow")?;
        let bytes = section_data
            .get(start..end)
            .ok_or_else(|| format!("invalid code range for {name:?}"))?
            .to_vec();
        let mut relocations = Vec::new();
        let mut unsupported = None;
        for (offset, relocation) in section.relocations() {
            let offset = offset as usize;
            if !(start..end).contains(&offset) {
                continue;
            }
            match read_relocation(file, offset - start, relocation, &name) {
                Ok(parsed) => relocations.push(parsed),
                Err(error) => {
                    unsupported = Some(error);
                    break;
                }
            }
        }
        stencils.push(Stencil {
            root,
            name,
            bytes,
            relocations,
            unsupported,
        });
    }
    Ok(stencils)
}

/// Keep native data and references back into the handler in this instruction's bundle.
/// Runtime helpers have already become LLVM holes and must never be copied here.
fn collect_bundle(
    file: &object::File<'_>,
    stencil: &Stencil,
    external_names: &mut BTreeSet<String>,
    module_symbols: &llvm_resolver::ModuleSymbols,
) -> Result<StencilBundle, String> {
    let root = file
        .symbol_by_index(stencil.root)
        .map_err(|e| e.to_string())?;
    let root_section = root.section_index().ok_or("handler without section")?;
    let mut sections = vec![BundleSection {
        id: root_section.0,
        address: root.address(),
        align: 16,
        blob_offset: 0,
        bytes: stencil.bytes.clone(),
        relocations: stencil.relocations.clone(),
    }];
    let mut pending = VecDeque::new();
    let mut seen = BTreeSet::from([root_section.0]);
    for relocation in &stencil.relocations {
        queue_target(
            file,
            &relocation.target,
            &mut pending,
            external_names,
            root_section,
            module_symbols,
        )?;
    }
    while let Some(id) = pending.pop_front() {
        if !seen.insert(id) {
            continue;
        }
        let section = file
            .section_by_index(object::SectionIndex(id))
            .map_err(|e| e.to_string())?;
        validate_section(&section)?;
        let name = section.name().unwrap_or("<unnamed>");
        let bytes = section.data().map_err(|e| format!("{name}: {e}"))?.to_vec();
        let mut relocations = Vec::new();
        for (offset, relocation) in section.relocations() {
            let parsed = read_relocation(file, offset as usize, relocation, name)?;
            queue_target(
                file,
                &parsed.target,
                &mut pending,
                external_names,
                root_section,
                module_symbols,
            )?;
            relocations.push(parsed);
        }
        sections.push(BundleSection {
            id,
            address: section.address(),
            align: section.align().max(1),
            blob_offset: 0,
            bytes,
            relocations,
        });
    }
    layout_bundle(&mut sections)?;
    let alignment = sections
        .iter()
        .map(|section| section.align)
        .max()
        .unwrap_or(16);
    if alignment > 4096 {
        return Err("stencil alignment exceeds executable page alignment".into());
    }
    // Validate internal offsets before accepting this opcode, rather than failing metadata
    // generation for the entire table after the supported set has already been chosen.
    for section in &sections {
        for relocation in &section.relocations {
            if let Target::Internal(symbol) = relocation.target {
                internal_offset(file, &sections, symbol)?;
            }
        }
    }
    Ok(StencilBundle {
        sections,
        alignment,
    })
}

fn queue_target(
    file: &object::File<'_>,
    target: &Target,
    pending: &mut VecDeque<usize>,
    external_names: &mut BTreeSet<String>,
    root_section: object::SectionIndex,
    module_symbols: &llvm_resolver::ModuleSymbols,
) -> Result<(), String> {
    let symbol = match target {
        Target::External(name) => {
            if let Some(slot) = name.strip_prefix("BOA_JIT_CONST_") {
                let slot: usize = slot.parse().map_err(|_| "invalid constant slot")?;
                if slot > 6 {
                    return Err("invalid constant slot".into());
                }
                return Ok(());
            }
            if !(module_symbols.is_declaration(name) || module_symbols.is_external_abi(name)) {
                return Err(format!(
                    "external symbol {name:?} has no typed declaration in boa_engine.ll"
                ));
            }
            external_names.insert(name.clone());
            return Ok(());
        }
        Target::Internal(symbol) => file.symbol_by_index(*symbol).map_err(|e| e.to_string())?,
    };
    let section_index = symbol
        .section_index()
        .ok_or("defined target without section")?;
    let section = file
        .section_by_index(section_index)
        .map_err(|e| e.to_string())?;
    if section.kind() == SectionKind::Text && section_index != root_section {
        return Err(format!(
            "unexpected code dependency {:?}; helper references must be LLVM holes",
            symbol.name().unwrap_or("<unnamed>")
        ));
    }
    validate_section(&section)?;
    pending.push_back(section_index.0);
    Ok(())
}

fn read_relocation(
    file: &object::File<'_>,
    offset: usize,
    relocation: object::Relocation,
    owner: &str,
) -> Result<Reloc, String> {
    validate_relocation(&relocation).map_err(|error| format!("{owner}: {error}"))?;
    let RelocationTarget::Symbol(index) = relocation.target() else {
        return Err(format!("{owner}: non-symbol relocation at {offset:#x}"));
    };
    let symbol = file.symbol_by_index(index).map_err(|e| e.to_string())?;
    let target = if matches!(symbol.section(), object::SymbolSection::Undefined) {
        let name = symbol.name().map_err(|e| e.to_string())?;
        if name.is_empty() {
            return Err(format!("{owner}: unnamed template external"));
        }
        Target::External(name.to_owned())
    } else {
        Target::Internal(index)
    };
    let kind = match relocation.kind() {
        RelocationKind::Relative => Kind::Relative,
        RelocationKind::PltRelative => Kind::PltRelative,
        RelocationKind::GotRelative => Kind::GotRelative,
        RelocationKind::Absolute => Kind::Absolute,
        _ => unreachable!("validated relocation kind"),
    };
    Ok(Reloc {
        offset: offset
            .try_into()
            .map_err(|_| format!("{owner}: relocation offset overflow"))?,
        addend: relocation.addend(),
        size: relocation.size(),
        kind,
        target,
    })
}

fn validate_section(section: &object::Section<'_, '_>) -> Result<(), String> {
    let name = section.name().unwrap_or("<unnamed>");
    if matches!(
        section.kind(),
        SectionKind::Text | SectionKind::ReadOnlyData | SectionKind::ReadOnlyString
    ) || (section.kind() == SectionKind::Data && name.starts_with(".data.rel.ro"))
    {
        Ok(())
    } else {
        Err(format!(
            "unsupported stencil section {name:?} ({:?})",
            section.kind()
        ))
    }
}

fn validate_relocation(relocation: &object::Relocation) -> Result<(), String> {
    if !matches!(
        relocation.kind(),
        RelocationKind::Relative
            | RelocationKind::PltRelative
            | RelocationKind::GotRelative
            | RelocationKind::Absolute
    ) {
        return Err(format!(
            "unsupported stencil relocation kind {:?} ({:?})",
            relocation.kind(),
            relocation.flags()
        ));
    }
    if !matches!(
        relocation.encoding(),
        RelocationEncoding::Generic | RelocationEncoding::X86RipRelative
    ) {
        return Err(format!(
            "unsupported stencil relocation encoding {:?}",
            relocation.encoding()
        ));
    }
    Ok(())
}

fn layout_bundle(sections: &mut [BundleSection]) -> Result<(), String> {
    let mut offset = 0usize;
    for section in sections {
        let align: usize = section
            .align
            .try_into()
            .map_err(|_| "bundle alignment overflow")?;
        if !align.is_power_of_two() {
            return Err(format!(
                "section {:?} has invalid alignment {align}",
                section.id
            ));
        }
        offset = offset
            .checked_add(align - 1)
            .ok_or("bundle layout overflow")?
            & !(align - 1);
        section.blob_offset = offset;
        offset = offset
            .checked_add(section.bytes.len())
            .ok_or("bundle layout overflow")?;
    }
    Ok(())
}

fn internal_offset(
    file: &object::File<'_>,
    sections: &[BundleSection],
    symbol: SymbolIndex,
) -> Result<usize, String> {
    let target = file
        .symbol_by_index(symbol)
        .map_err(|e| e.to_string())?;
    let id = target
        .section_index()
        .ok_or("internal target without section")?
        .0;
    let section = sections
        .iter()
        .find(|entry| entry.id == id)
        .ok_or_else(|| format!("internal target section {id:?} missing from bundle"))?;
    let within = target
        .address()
        .checked_sub(section.address)
        .ok_or("internal symbol precedes section")? as usize;
    if within > section.bytes.len() {
        return Err(format!("internal symbol lies outside section {id:?}"));
    }
    section
        .blob_offset
        .checked_add(within)
        .ok_or_else(|| "internal target offset overflow".into())
}

fn target_expr(
    file: &object::File<'_>,
    sections: &[BundleSection],
    external: &BTreeMap<String, usize>,
    target: &Target,
) -> Result<String, String> {
    let name = match target {
        Target::External(name) => name.as_str(),
        Target::Internal(symbol) => {
            return Ok(format!(
                "RelocationTarget::Internal({})",
                internal_offset(file, sections, *symbol)?
            ));
        }
    };
    if let Some(slot) = name.strip_prefix("BOA_JIT_CONST_") {
        let slot: usize = slot.parse().map_err(|_| "invalid constant slot")?;
        if slot > 6 {
            return Err("invalid constant slot".into());
        }
        return Ok(format!("RelocationTarget::Constant({slot})"));
    }
    let index = external
        .get(name)
        .ok_or_else(|| format!("external symbol {name:?} missing from resolver table"))?;
    Ok(format!("RelocationTarget::External({index})"))
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Relative => "Relative",
        Kind::PltRelative => "PltRelative",
        Kind::GotRelative => "GotRelative",
        Kind::Absolute => "Absolute",
    }
}

fn emit_metadata(
    file: &object::File<'_>,
    stencils: &[Stencil],
    bundles: &[Option<StencilBundle>],
    external: &BTreeMap<String, usize>,
    out: &Path,
) -> Result<(), String> {
    let mut stencil_blob = Vec::new();
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
    for (opcode, (stencil, bundle)) in stencils.iter().zip(bundles).enumerate() {
        let Some(bundle) = bundle else {
            generated.push_str(&format!(
                "// opcode {opcode}: {} (unsupported)\n",
                stencil.name
            ));
            continue;
        };
        let alignment = bundle.alignment;
        let start = stencil_blob.len();
        for section in &bundle.sections {
            stencil_blob.resize(start + section.blob_offset, 0);
            stencil_blob.extend_from_slice(&section.bytes);
        }
        generated.push_str(&format!("// opcode {opcode}: {}\n", stencil.name));
        generated.push_str(&format!(
            "static RELOCS_{opcode:03}: &[StencilRelocation] = &[\n"
        ));
        for section in &bundle.sections {
            for relocation in &section.relocations {
                let target = target_expr(file, &bundle.sections, external, &relocation.target)?;
                generated.push_str(&format!(
                "    StencilRelocation {{ offset: {}, addend: {}, size: {}, kind: RelocationKind::{}, target: {target} }},\n",
                section.blob_offset + relocation.offset as usize,
                relocation.addend,
                relocation.size,
                kind_name(relocation.kind)
            ));
            }
        }
        generated.push_str("];\n");
        generated.push_str(&format!(
            "fn emit_{opcode:03}(out: &mut FunctionBuilder) -> Result<usize, JitError> {{ out.append_stencil(&STENCIL_BLOB[{start}..{}], RELOCS_{opcode:03}, {alignment}) }}\n\n",
            stencil_blob.len()
        ));
    }
    generated.push_str(&format!(
        "pub(super) static EMITTERS: [Option<Emitter>; {0}] = [\n",
        stencils.len()
    ));
    for (opcode, bundle) in bundles.iter().enumerate() {
        if bundle.is_some() {
            generated.push_str(&format!("    Some(emit_{opcode:03}),\n"));
        } else {
            generated.push_str("    None,\n");
        }
    }
    generated.push_str("];\n");
    fs::write(out.join("jit_stencils.bin"), stencil_blob).map_err(|e| e.to_string())?;
    fs::write(out.join("jit_stencils_generated.rs"), generated).map_err(|e| e.to_string())
}

pub(super) fn read_u64_symbol(file: &object::File<'_>, name: &str) -> Result<u64, String> {
    let symbol = file
        .symbol_by_name(name)
        .ok_or_else(|| format!("template object does not define {name}"))?;
    if symbol.size() != 8 {
        return Err(format!("{name} has size {}, expected 8", symbol.size()));
    }
    let section_index = symbol
        .section_index()
        .ok_or_else(|| format!("{name} has no section"))?;
    let section = file
        .section_by_index(section_index)
        .map_err(|error| error.to_string())?;
    let offset = symbol
        .address()
        .checked_sub(section.address())
        .ok_or_else(|| format!("invalid {name} address"))? as usize;
    let bytes: [u8; 8] = section
        .data()
        .map_err(|error| error.to_string())?
        .get(offset..offset + 8)
        .ok_or_else(|| format!("invalid {name} data range"))?
        .try_into()
        .map_err(|_| format!("invalid {name} value"))?;
    Ok(if file.is_little_endian() {
        u64::from_le_bytes(bytes)
    } else {
        u64::from_be_bytes(bytes)
    })
}
