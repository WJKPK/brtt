use anyhow::{Context, Result};
use probe_rs::{
    config::{Registry, Target, TargetSelector},
    probe::{list::Lister, DebugProbeInfo},
    Permissions, Session,
};
use std::io::{self, Write};

/// A supported chip name, resolved without discovering or opening hardware.
#[derive(Debug, Clone)]
pub(crate) struct Chip(Target);

impl std::str::FromStr for Chip {
    type Err = anyhow::Error;

    fn from_str(name: &str) -> Result<Self> {
        Registry::from_builtin_families()
            .get_target_by_name(name)
            .map(Self)
            .with_context(|| format!("Invalid --chip '{name}'"))
    }
}

pub(crate) enum TargetRequest {
    AutoDetect,
    Named(Chip),
}

/// One discovery snapshot. Selection never discovers a second list of devices.
#[derive(Debug)]
pub(crate) struct ProbeCatalog {
    entries: Vec<DebugProbeInfo>,
}

#[derive(Debug)]
pub(crate) struct SelectedProbe {
    info: DebugProbeInfo,
}

#[derive(Debug)]
pub(crate) enum SelectionOutcome {
    Selected(SelectedProbe),
    NeedsChoice(ProbeCatalog),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectionError {
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

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// An invalid choice leaves the catalog unchanged so the caller can retry.
    /// A successful extraction changes indices; the picker finishes at that point.
    pub(crate) fn take(&mut self, index: usize) -> Result<SelectedProbe, SelectionError> {
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

    /// Selects the sole candidate or returns the owned snapshot for a choice.
    pub(crate) fn resolve(mut self) -> Result<SelectionOutcome, SelectionError> {
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

/// Metadata obtained from the selected probe and the successfully attached target.
#[derive(Debug)]
pub(crate) struct ProbeIdentity {
    pub(crate) label: String,
    pub(crate) chip: String,
}

pub(crate) struct AttachedTarget {
    session: Session,
    identity: ProbeIdentity,
}

impl AttachedTarget {
    pub(crate) fn into_parts(self) -> (Session, ProbeIdentity) {
        (self.session, self.identity)
    }
}

impl SelectedProbe {
    pub(crate) fn attach(self, target: TargetRequest) -> Result<AttachedTarget> {
        let auto_detect = matches!(target, TargetRequest::AutoDetect);
        let target = match target {
            TargetRequest::AutoDetect => TargetSelector::Auto,
            TargetRequest::Named(chip) => TargetSelector::Specified(chip.0),
        };
        let probe = self
            .info
            .open()
            .map_err(|error| anyhow::anyhow!("Error opening probe: {error}"))?;
        let session = probe
            .attach(target, Permissions::default())
            .map_err(|error| {
                let mut message = format!("Error creating debug session: {error}");
                if auto_detect && matches!(error, probe_rs::Error::ChipNotFound(_)) {
                    message
                        .push_str("\nHint: Specify the target chip type to disable auto-detection");
                }
                anyhow::anyhow!(message)
            })?;
        let identity = ProbeIdentity {
            label: format!(
                "{} {}",
                self.info.identifier,
                self.info
                    .serial_number
                    .as_deref()
                    .unwrap_or("(no serial number)")
            ),
            chip: session.target().name.clone(),
        };
        Ok(AttachedTarget { session, identity })
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
