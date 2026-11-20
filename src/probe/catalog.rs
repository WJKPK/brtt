use super::connection::SelectedProbe;
use probe_rs::probe::{list::Lister, DebugProbeInfo};
use std::io::{self, Write};

#[derive(Debug)]
pub(crate) struct ProbeCatalog {
    entries: Vec<DebugProbeInfo>,
}

#[derive(Debug)]
pub(super) enum SelectionOutcome {
    Selected(SelectedProbe),
    NeedsChoice(ProbeCatalog),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SelectionError {
    NoProbes,
    InvalidIndex(usize),
}

impl std::fmt::Display for SelectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoProbes => formatter.write_str(
                "No debug probes available. Make sure your probe is plugged in, supported and up-to-date.",
            ),
            Self::InvalidIndex(index) => write!(formatter, "Probe {index} does not exist."),
        }
    }
}

impl std::error::Error for SelectionError {}

impl ProbeCatalog {
    pub(crate) fn discover() -> Self {
        Self {
            entries: Lister::new().list_all(),
        }
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    /// An invalid choice leaves the catalog unchanged so the caller can retry.
    /// A successful extraction changes indices; the picker finishes at that point.
    pub(super) fn take(&mut self, index: usize) -> Result<SelectedProbe, SelectionError> {
        if self.entries.is_empty() {
            return Err(SelectionError::NoProbes);
        }
        if index >= self.entries.len() {
            return Err(SelectionError::InvalidIndex(index));
        }
        Ok(SelectedProbe {
            info: self.entries.swap_remove(index),
        })
    }

    pub(super) fn resolve(mut self) -> Result<SelectionOutcome, SelectionError> {
        match self.entries.len() {
            0 => Err(SelectionError::NoProbes),
            1 => self.take(0).map(SelectionOutcome::Selected),
            _ => Ok(SelectionOutcome::NeedsChoice(self)),
        }
    }

    pub(crate) fn write_to(&self, mut output: impl Write) -> io::Result<()> {
        writeln!(output, "Available probes:")?;
        write_probe_list(&mut output, &self.entries)
    }
}

fn write_probe_list(mut stream: impl Write, probes: &[DebugProbeInfo]) -> io::Result<()> {
    for (index, probe) in probes.iter().enumerate() {
        writeln!(stream, "  [{index}] {}", probe.identifier)?;
        writeln!(stream, "      Type: {}", probe.probe_type())?;
        writeln!(
            stream,
            "      Serial: {}",
            probe.serial_number.as_deref().unwrap_or("(none)")
        )?;
        writeln!(
            stream,
            "      USB: {:04x}:{:04x}",
            probe.vendor_id, probe.product_id
        )?;
        if let Some(interface) = probe.interface {
            writeln!(stream, "      Interface: {interface}")?;
        }
        if index + 1 < probes.len() {
            writeln!(stream)?;
        }
    }
    Ok(())
}
