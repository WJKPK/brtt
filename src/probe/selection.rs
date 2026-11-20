use super::catalog::{SelectionError, SelectionOutcome};
use super::{ProbeCatalog, SelectedProbe};
use anyhow::{bail, Result};
use std::io::{self, BufRead, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeSelection {
    Index(usize),
    OnlyAvailable,
    Prompt,
}

impl ProbeSelection {
    pub(crate) fn select(self, mut catalog: ProbeCatalog) -> Result<SelectedProbe> {
        if let Self::Index(index) = self {
            return match catalog.take(index) {
                Ok(selected) => Ok(selected),
                Err(error) => {
                    if matches!(error, SelectionError::InvalidIndex(_)) {
                        catalog.write_to(io::stderr().lock())?;
                    }
                    Err(error.into())
                }
            };
        }
        match catalog.resolve()? {
            SelectionOutcome::Selected(selected) => Ok(selected),
            SelectionOutcome::NeedsChoice(catalog) if self == Self::Prompt => {
                prompt_for_probe(catalog, &mut io::stdin().lock(), &mut io::stderr().lock())
            }
            SelectionOutcome::NeedsChoice(_) => {
                bail!("Multiple debug probes found; specify one with '--probe INDEX' when stdin is not interactive.");
            }
        }
    }
}

fn prompt_for_probe(
    mut catalog: ProbeCatalog,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<SelectedProbe> {
    let last_index = catalog
        .len()
        .checked_sub(1)
        .ok_or(SelectionError::NoProbes)?;
    catalog.write_to(&mut *output)?;
    loop {
        write!(output, "Select probe [0-{last_index}]: ")?;
        output.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            bail!("Probe selection cancelled.");
        }
        if let Ok(index) = line.trim().parse::<usize>() {
            match catalog.take(index) {
                Ok(selected) => return Ok(selected),
                Err(SelectionError::InvalidIndex(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
        writeln!(output, "Invalid probe selection.")?;
    }
}
