use anyhow::{Context, Result};
use probe_rs::{
    config::{Registry, Target, TargetSelector},
    probe::DebugProbeInfo,
    Permissions, Session,
};

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

#[derive(Debug)]
pub(crate) struct SelectedProbe {
    pub(super) info: DebugProbeInfo,
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
