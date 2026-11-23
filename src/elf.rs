use crate::channel::CoreId;
use crate::defmt::DefmtData;
use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// One `--elf` value: either a bare path (assigned the lowest free core
/// index) or an explicit `INDEX=PATH` mapping. Any `=` requires the indexed
/// form; a bare path must not contain `=`.
#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct ElfSpec {
    pub(crate) core: Option<CoreId>,
    pub(crate) path: PathBuf,
}

/// An ELF file resolved to a specific target core.
#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct CoreElf {
    pub(crate) core: CoreId,
    pub(crate) path: PathBuf,
}

impl CoreElf {
    pub(crate) fn load_rtt_symbol(&self) -> Result<Option<u64>> {
        let bytes = read_elf(&self.path)?;
        rtt_symbol_from_elf(&self.path, &bytes)
    }

    pub(crate) fn load_rtt_only(&self) -> Result<ElfContents> {
        Ok(ElfContents {
            rtt_symbol: self.load_rtt_symbol()?,
            defmt: None,
        })
    }

    pub(crate) fn load_with_defmt(&self) -> Result<ElfContents> {
        let bytes = read_elf(&self.path)?;
        let rtt_symbol = rtt_symbol_from_elf(&self.path, &bytes)?;
        let defmt = Some(DefmtData::load(&self.path, &bytes)?);
        Ok(ElfContents { rtt_symbol, defmt })
    }
}

/// Assigns each `--elf` value a core index. Explicit `INDEX=` mappings reserve
/// their slots first regardless of argument order, then bare paths fill the
/// lowest free slots in CLI order. Gaps are allowed so a single `--elf 1=…`
/// can target a non-zero core.
pub(crate) fn resolve_elf_specs(specs: &[ElfSpec]) -> Result<Vec<CoreElf>> {
    let mut reserved = HashSet::new();
    for spec in specs {
        if let Some(core) = spec.core {
            if !reserved.insert(core) {
                bail!("--elf index {core} was specified more than once");
            }
        }
    }
    let mut next_free = CoreId::new(0);
    let mut resolved = Vec::with_capacity(specs.len());
    for spec in specs {
        let core = spec.core.unwrap_or_else(|| {
            while !reserved.insert(next_free) {
                next_free = next_free.checked_next().expect("u32 core indices exhausted");
            }
            next_free
        });
        resolved.push(CoreElf {
            core,
            path: spec.path.clone(),
        });
    }
    Ok(resolved)
}

pub(crate) fn read_elf(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("failed to read ELF '{}'", path.display()))
}

fn rtt_symbol_from_elf(path: &Path, bytes: &[u8]) -> Result<Option<u64>> {
    probe_rs::rtt::find_rtt_control_block_in_raw_file(bytes)
        .with_context(|| format!("failed to parse ELF '{}'", path.display()))
}

/// Parsed `_SEGGER_RTT` symbol address and optional defmt table from a single ELF read.
pub(crate) struct ElfContents {
    pub(crate) rtt_symbol: Option<u64>,
    pub(crate) defmt: Option<DefmtData>,
}
