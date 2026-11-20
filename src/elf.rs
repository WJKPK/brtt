use crate::channel::{ChannelEncoding, ChannelSpec};
use crate::defmt::DefmtData;
use crate::rtt::ScanRegion;
use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// One `--elf` value: either a bare path (assigned the lowest free core
/// index) or an explicit `INDEX=PATH` mapping. Any `=` requires the indexed
/// form; a bare path must not contain `=`.
#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct ElfSpec {
    pub(crate) index: Option<u32>,
    pub(crate) path: PathBuf,
}

/// Assigns each `--elf` value a core index. Explicit `INDEX=` mappings reserve
/// their slots first regardless of argument order, then bare paths fill the
/// lowest free slots in CLI order. Gaps are allowed so a single `--elf 1=…`
/// can target a non-zero core.
pub(crate) fn resolve_elf_specs(specs: &[ElfSpec]) -> Result<Vec<(u32, PathBuf)>> {
    let mut reserved = HashSet::new();
    for spec in specs {
        if let Some(index) = spec.index {
            if !reserved.insert(index) {
                bail!("--elf index {index} was specified more than once");
            }
        }
    }
    let mut next_free = 0u32;
    let mut resolved = Vec::with_capacity(specs.len());
    for spec in specs {
        let index = spec.index.unwrap_or_else(|| {
            while !reserved.insert(next_free) {
                next_free = next_free.checked_add(1).expect("u32 core indices exhausted");
            }
            next_free
        });
        resolved.push((index, spec.path.clone()));
    }
    Ok(resolved)
}

pub(crate) fn read_elf(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("failed to read ELF '{}'", path.display()))
}

fn rtt_region_from_elf(path: &Path, bytes: &[u8]) -> Result<Option<ScanRegion>> {
    let address = probe_rs::rtt::find_rtt_control_block_in_raw_file(bytes)
        .with_context(|| format!("failed to parse ELF '{}'", path.display()))?;
    Ok(address.map(ScanRegion::Exact))
}

/// RTT control-block region and optional defmt table parsed from a single ELF read.
pub(crate) struct ElfContents {
    pub(crate) region: Option<ScanRegion>,
    pub(crate) defmt: Option<DefmtData>,
}

impl ElfContents {
    fn load(path: &Path, with_defmt: bool) -> Result<Self> {
        let bytes = read_elf(path)?;
        let region = rtt_region_from_elf(path, &bytes)?;
        let defmt = with_defmt
            .then(|| DefmtData::load(path, &bytes))
            .transpose()?;
        Ok(Self { region, defmt })
    }

    /// Reads every `--elf` once, deriving each defmt table only when a defmt
    /// channel selects that core. A terminal-only core whose ELF lacks `.defmt`
    /// must not fail startup.
    pub(crate) fn load_all(
        specs: &[(u32, PathBuf)],
        up_specs: &[ChannelSpec],
    ) -> Result<Vec<(u32, Self)>> {
        specs
            .iter()
            .map(|(index, path)| {
                let needs_defmt = up_specs
                    .iter()
                    .any(|spec| spec.mode == ChannelEncoding::Defmt && spec.applies_to(*index));
                Self::load(path, needs_defmt).map(|contents| (*index, contents))
            })
            .collect()
    }
}
