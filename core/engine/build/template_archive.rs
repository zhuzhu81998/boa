use object::{
    Object, ObjectSection, ObjectSymbol, SymbolIndex, SymbolSection, read::archive::ArchiveFile,
};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct MemberId(pub(super) usize);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct SectionRef {
    pub(super) member: MemberId,
    pub(super) section: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SymbolRef {
    pub(super) member: MemberId,
    pub(super) symbol: SymbolIndex,
}

impl Ord for SymbolRef {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.member, self.symbol.0).cmp(&(other.member, other.symbol.0))
    }
}

impl PartialOrd for SymbolRef {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ResolvedSymbol {
    Defined(SymbolRef),
    External(String),
}

/// A rustc staticlib viewed as independent relocatable objects. Members are copied solely to meet
/// the alignment required by object’s zero-copy ELF reader; no native linking is performed.
pub(super) struct TemplateArchive {
    members: Vec<Vec<u8>>,
    definitions: BTreeMap<String, SymbolRef>,
}

impl TemplateArchive {
    pub(super) fn parse(data: &[u8]) -> Result<Self, String> {
        let archive = ArchiveFile::parse(data).map_err(|e| e.to_string())?;
        if archive.is_thin() {
            return Err("thin template archives are unsupported".into());
        }
        let mut members = Vec::new();
        for member in archive.members() {
            let member = member.map_err(|e| e.to_string())?;
            let bytes = member.data(data).map_err(|e| e.to_string())?.to_vec();
            if object::File::parse(bytes.as_slice()).is_ok() {
                members.push(bytes);
            }
        }
        let mut definitions = BTreeMap::new();
        for (ordinal, bytes) in members.iter().enumerate() {
            let file = object::File::parse(bytes.as_slice()).map_err(|e| e.to_string())?;
            for symbol in file.symbols() {
                if matches!(symbol.section(), SymbolSection::Undefined) || !symbol.is_global() {
                    continue;
                }
                let Ok(name) = symbol.name() else {
                    continue;
                };
                if !name.is_empty() {
                    definitions.entry(name.to_owned()).or_insert(SymbolRef {
                        member: MemberId(ordinal),
                        symbol: symbol.index(),
                    });
                }
            }
        }
        if definitions.is_empty() {
            return Err("template archive has no global definitions".into());
        }
        Ok(Self {
            members,
            definitions,
        })
    }

    pub(super) fn member_count(&self) -> usize {
        self.members.len()
    }

    pub(super) fn definition(&self, name: &str) -> Result<Option<SymbolRef>, String> {
        Ok(self.definitions.get(name).copied())
    }

    pub(super) fn resolve(
        &self,
        origin: MemberId,
        index: SymbolIndex,
    ) -> Result<ResolvedSymbol, String> {
        let file = self.file(origin)?;
        self.resolve_in(origin, &file, index)
    }

    pub(super) fn resolve_in(
        &self,
        origin: MemberId,
        file: &object::File<'_>,
        index: SymbolIndex,
    ) -> Result<ResolvedSymbol, String> {
        let symbol = file.symbol_by_index(index).map_err(|e| e.to_string())?;
        if !matches!(symbol.section(), SymbolSection::Undefined) {
            return Ok(ResolvedSymbol::Defined(SymbolRef {
                member: origin,
                symbol: index,
            }));
        }
        let name = symbol.name().map_err(|e| e.to_string())?;
        if name.is_empty() {
            return Err("unnamed archive external".into());
        }
        Ok(match self.definition(name)? {
            Some(definition) => ResolvedSymbol::Defined(definition),
            None => ResolvedSymbol::External(name.to_owned()),
        })
    }

    pub(super) fn file(&self, member: MemberId) -> Result<object::File<'_>, String> {
        let bytes = self
            .members
            .get(member.0)
            .ok_or("invalid archive member id")?;
        object::File::parse(bytes.as_slice()).map_err(|e| e.to_string())
    }
}

pub(super) fn read_u64_symbol(data: &[u8], name: &str) -> Result<u64, String> {
    let archive = TemplateArchive::parse(data)?;
    let reference = archive
        .definition(name)?
        .ok_or_else(|| format!("template archive does not define {name}"))?;
    let file = archive.file(reference.member)?;
    let symbol = file
        .symbol_by_index(reference.symbol)
        .map_err(|error| error.to_string())?;
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
