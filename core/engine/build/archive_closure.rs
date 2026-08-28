use super::{
    llvm_resolver,
    template_archive::{ResolvedSymbol, SectionRef, SymbolRef, TemplateArchive},
};
use object::{
    Object, ObjectSection, ObjectSymbol, RelocationEncoding, RelocationKind, RelocationTarget,
    SectionKind,
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
    Internal(SymbolRef),
    External(String),
}

#[derive(Clone)]
enum Classification {
    Copy(SectionRef),
    External(String),
    Reject(String),
}

struct Reloc {
    offset: u32,
    addend: i64,
    size: u8,
    kind: Kind,
    target: Target,
}

struct Stencil {
    name: String,
    bytes: Vec<u8>,
    relocations: Vec<Reloc>,
    unsupported: Option<String>,
}

struct ClosureSection {
    id: SectionRef,
    address: u64,
    align: u64,
    blob_offset: usize,
    bytes: Vec<u8>,
    relocations: Vec<Reloc>,
}

pub(super) struct Generated {
    pub(super) members: usize,
    pub(super) sections: usize,
    pub(super) bytes: usize,
    pub(super) relocations: usize,
    pub(super) externals: usize,
    pub(super) supported: usize,
}

pub(super) fn generate(
    data: &[u8],
    handler_table: &str,
    opcode_count: usize,
    llvm_ir: &Path,
) -> Result<Generated, String> {
    if !llvm_ir.is_file() {
        return Err(format!("missing LLVM IR: {}", llvm_ir.display()));
    }
    let archive = TemplateArchive::parse(data)?;
    let table = archive
        .definition(handler_table)?
        .ok_or("handler table absent from archive")?;
    let module_symbols = llvm_resolver::module_symbols(llvm_ir)?;
    let boa_member = table.member;
    let boa_file = archive.file(boa_member)?;
    let roots = handler_roots(&archive, &boa_file, table, opcode_count)?;
    let stencils = extract_stencils(&archive, &boa_file, &roots)?;
    let mut externalized = BTreeSet::new();
    let mut external_names = BTreeSet::new();
    let mut closure_by_id = BTreeMap::new();
    let mut supported = BTreeSet::new();
    for (opcode, stencil) in stencils.iter().enumerate() {
        if let Some(error) = &stencil.unsupported {
            println!(
                "cargo::warning=JIT opcode {opcode} ({}) unsupported: {error}",
                stencil.name
            );
            continue;
        }
        let mut opcode_externalized = BTreeSet::new();
        let mut opcode_externals = BTreeSet::new();
        let mut classifications = BTreeMap::new();
        match collect_closure(
            &archive,
            std::slice::from_ref(stencil),
            &mut opcode_externalized,
            &mut opcode_externals,
            &mut classifications,
            boa_member,
            &module_symbols,
            &boa_file,
        ) {
            Ok(sections) => {
                supported.insert(opcode);
                externalized.extend(opcode_externalized);
                external_names.extend(opcode_externals);
                for section in sections {
                    closure_by_id.entry(section.id).or_insert(section);
                }
            }
            Err(error) => println!(
                "cargo::warning=JIT opcode {opcode} ({}) unsupported: {error}",
                stencil.name
            ),
        }
    }
    let mut closure: Vec<_> = closure_by_id.into_values().collect();
    layout_closure(&mut closure)?;

    for (opcode, stencil) in stencils.iter().enumerate() {
        if !supported.contains(&opcode) {
            continue;
        }
        for relocation in &stencil.relocations {
            if let Target::External(name) = &relocation.target {
                external_names.insert(name.clone());
            }
        }
    }
    for section in &closure {
        for relocation in &section.relocations {
            if let Target::External(name) = &relocation.target {
                external_names.insert(name.clone());
            }
        }
    }
    let external: BTreeMap<String, usize> = external_names
        .into_iter()
        .enumerate()
        .map(|(index, name)| (name, index))
        .collect();
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is missing")?);
    let names: Vec<String> = external.keys().cloned().collect();
    llvm_resolver::generate(llvm_ir, &names, &out)?;
    emit_metadata(
        &archive,
        &stencils,
        &closure,
        &externalized,
        &external,
        &supported,
        &out,
    )?;

    Ok(Generated {
        members: archive.member_count(),
        sections: closure.len(),
        bytes: closure.iter().map(|section| section.bytes.len()).sum(),
        relocations: stencils
            .iter()
            .map(|stencil| stencil.relocations.len())
            .sum::<usize>()
            + closure
                .iter()
                .map(|section| section.relocations.len())
                .sum::<usize>(),
        externals: external.len(),
        supported: supported.len(),
    })
}

fn handler_roots(
    archive: &TemplateArchive,
    file: &object::File<'_>,
    table: SymbolRef,
    opcode_count: usize,
) -> Result<Vec<SymbolRef>, String> {
    let symbol = file
        .symbol_by_index(table.symbol)
        .map_err(|e| e.to_string())?;
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
        match archive.resolve(table.member, index)? {
            ResolvedSymbol::Defined(symbol) => roots.push((offset, symbol)),
            ResolvedSymbol::External(name) => return Err(format!("handler {name:?} is undefined")),
        }
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
    archive: &TemplateArchive,
    boa_file: &object::File<'_>,
    roots: &[SymbolRef],
) -> Result<Vec<Stencil>, String> {
    let mut stencils = Vec::with_capacity(roots.len());
    for &root in roots {
        if root.member != roots[0].member {
            return Err("opcode handlers span multiple LLVM modules".into());
        }
        let file = boa_file;
        let symbol = file
            .symbol_by_index(root.symbol)
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
            match read_relocation(
                archive,
                &file,
                root.member,
                offset - start,
                relocation,
                &name,
            ) {
                Ok(parsed) => relocations.push(parsed),
                Err(error) => {
                    unsupported = Some(error);
                    break;
                }
            }
        }
        stencils.push(Stencil {
            name,
            bytes,
            relocations,
            unsupported,
        });
    }
    Ok(stencils)
}

fn collect_closure(
    archive: &TemplateArchive,
    stencils: &[Stencil],
    externalized: &mut BTreeSet<SymbolRef>,
    external_names: &mut BTreeSet<String>,
    classifications: &mut BTreeMap<SymbolRef, Classification>,
    boa_member: super::template_archive::MemberId,
    module_symbols: &llvm_resolver::ModuleSymbols,
    boa_file: &object::File<'_>,
) -> Result<Vec<ClosureSection>, String> {
    let mut pending = VecDeque::new();
    for stencil in stencils {
        for relocation in &stencil.relocations {
            queue_target(
                archive,
                &relocation.target,
                &mut pending,
                externalized,
                external_names,
                classifications,
                boa_member,
                module_symbols,
                boa_file,
            )?;
        }
    }
    let mut seen = BTreeSet::new();
    let mut closure = Vec::new();
    while let Some(id) = pending.pop_front() {
        if !seen.insert(id) {
            continue;
        }
        if id.member != boa_member {
            return Err("dependency section escaped LLVM module boundary".into());
        }
        let file = boa_file;
        let section = file
            .section_by_index(object::SectionIndex(id.section))
            .map_err(|e| e.to_string())?;
        validate_section(&section)?;
        let name = section.name().unwrap_or("<unnamed>");
        let bytes = section.data().map_err(|e| format!("{name}: {e}"))?.to_vec();
        let mut relocations = Vec::new();
        for (offset, relocation) in section.relocations() {
            let parsed =
                read_relocation(archive, &file, id.member, offset as usize, relocation, name)?;
            queue_target(
                archive,
                &parsed.target,
                &mut pending,
                externalized,
                external_names,
                classifications,
                boa_member,
                module_symbols,
                boa_file,
            )?;
            relocations.push(parsed);
        }
        closure.push(ClosureSection {
            id,
            address: section.address(),
            align: section.align().max(1),
            blob_offset: 0,
            bytes,
            relocations,
        });
    }
    closure.sort_by_key(|section| section.id);
    Ok(closure)
}

fn queue_target(
    archive: &TemplateArchive,
    target: &Target,
    pending: &mut VecDeque<SectionRef>,
    externalized: &mut BTreeSet<SymbolRef>,
    external_names: &mut BTreeSet<String>,
    classifications: &mut BTreeMap<SymbolRef, Classification>,
    boa_member: super::template_archive::MemberId,
    module_symbols: &llvm_resolver::ModuleSymbols,
    boa_file: &object::File<'_>,
) -> Result<(), String> {
    let Target::Internal(target) = target else {
        let Target::External(name) = target else {
            unreachable!()
        };
        if !(module_symbols.is_declaration(name) || module_symbols.is_external_abi(name)) {
            return Err(format!(
                "external symbol {name:?} has no typed declaration in boa_engine.ll"
            ));
        }
        external_names.insert(name.clone());
        return Ok(());
    };
    let classification = classify_symbol(
        archive,
        *target,
        classifications,
        &mut BTreeSet::new(),
        boa_member,
        module_symbols,
        boa_file,
    )?;
    match classification {
        Classification::Copy(section) => pending.push_back(section),
        Classification::External(name) => {
            externalized.insert(*target);
            external_names.insert(name);
        }
        Classification::Reject(error) => return Err(error),
    }
    Ok(())
}

fn classify_symbol(
    archive: &TemplateArchive,
    target: SymbolRef,
    classifications: &mut BTreeMap<SymbolRef, Classification>,
    visiting: &mut BTreeSet<SymbolRef>,
    boa_member: super::template_archive::MemberId,
    module_symbols: &llvm_resolver::ModuleSymbols,
    boa_file: &object::File<'_>,
) -> Result<Classification, String> {
    if let Some(classification) = classifications.get(&target) {
        return Ok(classification.clone());
    }
    if target.member != boa_member {
        let file = archive.file(target.member)?;
        let symbol = file
            .symbol_by_index(target.symbol)
            .map_err(|e| e.to_string())?;
        let name = symbol.name().map_err(|e| e.to_string())?;
        if name.is_empty() {
            return Err("cross-module target has no linker name".into());
        }
        if !(module_symbols.is_declaration(name) || module_symbols.is_external_abi(name)) {
            return Err(format!(
                "cross-module symbol {name:?} has no typed declaration in boa_engine.ll"
            ));
        }
        let classification = Classification::External(name.to_owned());
        classifications.insert(target, classification.clone());
        return Ok(classification);
    }
    let file = boa_file;
    let symbol = file
        .symbol_by_index(target.symbol)
        .map_err(|e| e.to_string())?;
    let section_index = symbol
        .section_index()
        .ok_or("defined target without section")?;
    let section = file
        .section_by_index(section_index)
        .map_err(|e| e.to_string())?;
    let id = SectionRef {
        member: target.member,
        section: section_index.0,
    };
    if !visiting.insert(target) {
        return Ok(Classification::Copy(id));
    }
    let result: Result<Classification, String> = (|| {
        validate_section(&section)?;
        for (_, relocation) in section.relocations() {
            validate_relocation(&relocation)?;
            let RelocationTarget::Symbol(index) = relocation.target() else {
                return Err("non-symbol relocation in closure".into());
            };
            if let ResolvedSymbol::Defined(dependency) =
                archive.resolve_in(target.member, &file, index)?
            {
                if let Classification::Reject(error) = classify_symbol(
                    archive,
                    dependency,
                    classifications,
                    visiting,
                    boa_member,
                    module_symbols,
                    boa_file,
                )? {
                    return Err(error);
                }
            }
        }
        Ok(Classification::Copy(id))
    })();
    visiting.remove(&target);
    let classification = match result {
        Ok(classification) => classification,
        Err(error) => {
            let name = symbol.name().map_err(|e| e.to_string())?;
            if section.kind() != SectionKind::Text
                || !symbol.is_global()
                || name.is_empty()
                || !(module_symbols.is_linkable_definition(name)
                    || module_symbols.is_declaration(name))
            {
                Classification::Reject(format!(
                    "cannot externalize non-global target {name:?} in section {:?} ({:?}): {error}",
                    section.name().unwrap_or("<unnamed>"),
                    section.kind()
                ))
            } else {
                Classification::External(name.to_owned())
            }
        }
    };
    classifications.insert(target, classification.clone());
    Ok(classification)
}

fn read_relocation(
    archive: &TemplateArchive,
    file: &object::File<'_>,
    origin: super::template_archive::MemberId,
    offset: usize,
    relocation: object::Relocation,
    owner: &str,
) -> Result<Reloc, String> {
    validate_relocation(&relocation).map_err(|error| format!("{owner}: {error}"))?;
    let RelocationTarget::Symbol(index) = relocation.target() else {
        return Err(format!("{owner}: non-symbol relocation at {offset:#x}"));
    };
    let target = match archive.resolve_in(origin, file, index)? {
        ResolvedSymbol::Defined(symbol) => Target::Internal(symbol),
        ResolvedSymbol::External(name) => Target::External(name),
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
            "unsupported archive closure section {name:?} ({:?})",
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
            "unsupported archive relocation kind {:?} ({:?})",
            relocation.kind(),
            relocation.flags()
        ));
    }
    if !matches!(
        relocation.encoding(),
        RelocationEncoding::Generic | RelocationEncoding::X86RipRelative
    ) {
        return Err(format!(
            "unsupported archive relocation encoding {:?}",
            relocation.encoding()
        ));
    }
    Ok(())
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
                "section {:?} has invalid alignment {align}",
                section.id
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
    archive: &TemplateArchive,
    closure: &[ClosureSection],
    symbol: SymbolRef,
) -> Result<usize, String> {
    let file = archive.file(symbol.member)?;
    let target = file
        .symbol_by_index(symbol.symbol)
        .map_err(|e| e.to_string())?;
    let id = SectionRef {
        member: symbol.member,
        section: target
            .section_index()
            .ok_or("internal target without section")?
            .0,
    };
    let section = closure
        .iter()
        .find(|entry| entry.id == id)
        .ok_or_else(|| format!("internal target section {id:?} missing from closure"))?;
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
    archive: &TemplateArchive,
    closure: &[ClosureSection],
    externalized: &BTreeSet<SymbolRef>,
    external: &BTreeMap<String, usize>,
    target: &Target,
) -> Result<String, String> {
    let name = match target {
        Target::External(name) => Some(name.as_str()),
        Target::Internal(symbol) if externalized.contains(symbol) => {
            let file = archive.file(symbol.member)?;
            Some(
                file.symbol_by_index(symbol.symbol)
                    .map_err(|e| e.to_string())?
                    .name()
                    .map_err(|e| e.to_string())?,
            )
        }
        Target::Internal(symbol) => {
            return Ok(format!(
                "RelocationTarget::Internal({})",
                internal_offset(archive, closure, *symbol)?
            ));
        }
    };
    let name = name.expect("external target has a name");
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
    archive: &TemplateArchive,
    stencils: &[Stencil],
    closure: &[ClosureSection],
    externalized: &BTreeSet<SymbolRef>,
    external: &BTreeMap<String, usize>,
    supported: &BTreeSet<usize>,
    out: &Path,
) -> Result<(), String> {
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
            let target = target_expr(archive, closure, externalized, external, &relocation.target)?;
            generated.push_str(&format!(
                "    StencilRelocation {{ offset: {offset}, addend: {}, size: {}, kind: RelocationKind::{}, target: {target} }},\n",
                relocation.addend,
                relocation.size,
                kind_name(relocation.kind)
            ));
        }
    }
    generated.push_str("];\n\n");
    for (opcode, stencil) in stencils.iter().enumerate() {
        if !supported.contains(&opcode) {
            generated.push_str(&format!(
                "// opcode {opcode}: {} (unsupported)\n",
                stencil.name
            ));
            continue;
        }
        let start = stencil_blob.len();
        stencil_blob.extend_from_slice(&stencil.bytes);
        generated.push_str(&format!("// opcode {opcode}: {}\n", stencil.name));
        generated.push_str(&format!(
            "static RELOCS_{opcode:03}: &[StencilRelocation] = &[\n"
        ));
        for relocation in &stencil.relocations {
            let target = target_expr(archive, closure, externalized, external, &relocation.target)?;
            generated.push_str(&format!(
                "    StencilRelocation {{ offset: {}, addend: {}, size: {}, kind: RelocationKind::{}, target: {target} }},\n",
                relocation.offset,
                relocation.addend,
                relocation.size,
                kind_name(relocation.kind)
            ));
        }
        generated.push_str("];\n");
        generated.push_str(&format!(
            "fn emit_{opcode:03}(out: &mut FunctionBuilder) -> Result<usize, JitError> {{ out.append_stencil(&STENCIL_BLOB[{start}..{}], RELOCS_{opcode:03}) }}\n\n",
            stencil_blob.len()
        ));
    }
    generated.push_str(&format!(
        "pub(super) static EMITTERS: [Option<Emitter>; {0}] = [\n",
        stencils.len()
    ));
    for opcode in 0..stencils.len() {
        if supported.contains(&opcode) {
            generated.push_str(&format!("    Some(emit_{opcode:03}),\n"));
        } else {
            generated.push_str("    None,\n");
        }
    }
    generated.push_str("];\n");
    fs::write(out.join("jit_stencils.bin"), stencil_blob).map_err(|e| e.to_string())?;
    fs::write(out.join("jit_internal_closure.bin"), closure_blob).map_err(|e| e.to_string())?;
    fs::write(out.join("jit_stencils_generated.rs"), generated).map_err(|e| e.to_string())
}
