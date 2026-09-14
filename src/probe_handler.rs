use anyhow::{bail, Result};
use probe_rs::{
    config::TargetSelector,
    probe::{list::Lister, DebugProbeInfo},
    Permissions, Session,
};
use std::io::{self, IsTerminal, Write};

use crate::cli::ProbeInfo;

pub(crate) struct AttachedProbe {
    pub(crate) session: Session,
    pub(crate) label: String,
    pub(crate) chip: String,
}

pub(crate) fn list() -> Vec<DebugProbeInfo> {
    Lister::new().list_all()
}

pub(crate) fn attach(
    probes: &[DebugProbeInfo],
    requested: Option<&ProbeInfo>,
    chip: Option<&str>,
) -> Result<AttachedProbe> {
    if probes.is_empty() {
        bail!("No debug probes available. Make sure your probe is plugged in, supported and up-to-date.");
    }

    let number = select(probes, requested)?;
    if number >= probes.len() {
        list_probes(io::stderr(), probes);
        bail!("Probe {number} does not exist.");
    }

    let info = &probes[number];
    let label = format!(
        "{} {}",
        info.identifier,
        info.serial_number
            .as_deref()
            .unwrap_or("(no serial number)")
    );
    let probe = info
        .open()
        .map_err(|error| anyhow::anyhow!("Error opening probe: {error}"))?;
    let target_selector = TargetSelector::from(chip);
    let chip_name = chip.unwrap_or("auto").to_string();
    let session = probe
        .attach(target_selector, Permissions::default())
        .map_err(|error| {
            let mut message = format!("Error creating debug session: {error}");
            if chip.is_none() && matches!(error, probe_rs::Error::ChipNotFound(_)) {
                message.push_str("\nHint: Use '--chip' to specify the target chip type manually");
            }
            anyhow::anyhow!(message)
        })?;

    Ok(AttachedProbe {
        session,
        label,
        chip: chip_name,
    })
}

fn select(probes: &[DebugProbeInfo], requested: Option<&ProbeInfo>) -> Result<usize> {
    if let Some(index) = automatic_selection(probes.len(), requested)? {
        return Ok(index);
    }
    if !io::stdin().is_terminal() {
        bail!("Multiple debug probes found; specify one with '--probe INDEX' when stdin is not interactive.");
    }

    let stdout = io::stdout();
    let mut output = stdout.lock();
    writeln!(output, "Multiple debug probes found:")?;
    writeln!(output)?;
    write_probe_list(&mut output, probes)?;
    loop {
        write!(output, "Select probe [0-{}]: ", probes.len() - 1)?;
        output.flush()?;
        let mut input = String::new();
        if io::stdin().read_line(&mut input)? == 0 {
            bail!("Probe selection cancelled.");
        }
        match input.trim().parse::<usize>() {
            Ok(index) if index < probes.len() => return Ok(index),
            _ => writeln!(output, "Invalid probe selection.")?,
        }
    }
}

fn automatic_selection(count: usize, requested: Option<&ProbeInfo>) -> Result<Option<usize>> {
    match requested {
        Some(ProbeInfo::Number(index)) => Ok(Some(*index)),
        Some(ProbeInfo::List) => bail!("probe list must be handled before selecting a probe"),
        None if count == 1 => Ok(Some(0)),
        None => Ok(None),
    }
}

pub(crate) fn list_probes(mut stream: impl Write, probes: &[DebugProbeInfo]) {
    writeln!(stream, "Available probes:").unwrap();
    write_probe_list(&mut stream, probes).unwrap();
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

#[cfg(test)]
#[path = "../tests/probe_handler.rs"]
mod tests;
