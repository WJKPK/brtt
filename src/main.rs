mod cli;
mod defmt;
mod logger;
mod session;

use brtt::rtt::{attach_region_incremental, Rtt};
use brtt::RttChannel;

use anyhow::{bail, Context, Result};
use clap::Parser;
use cli::{configured_up_specs, ChannelEncoding, Opts, ProbeInfo};
use probe_rs::{config::TargetSelector, probe::list::Lister, probe::DebugProbeInfo, Permissions};
use session::{run_session, SessionConfig};
use std::io::{self, IsTerminal, Write};
use std::time::Duration;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("brtt=warn"))
        .format(|buffer, record| writeln!(buffer, "[brtt {}] {}", record.level(), record.args()))
        .init();
    let opts = Opts::parse();

    let up_specs = configured_up_specs(&opts.up);
    opts.validate(&up_specs)?;
    let defmt_filters = opts
        .defmt_filter
        .as_deref()
        .map(defmt::parse_filter_spec)
        .transpose()?;
    let defmt_data = defmt::require_elf(
        opts.elf.as_deref(),
        opts.debug_defmt_table
            || up_specs
                .iter()
                .any(|spec| spec.mode == ChannelEncoding::Defmt),
    )?;
    if opts.debug_defmt_table {
        let data = defmt_data.as_ref().ok_or_else(|| {
            anyhow::anyhow!("--debug-defmt-table requires --elf with a defmt table")
        })?;
        data.debug_summary(&mut std::io::stdout())?;
        return Ok(());
    }

    let elf_region = opts
        .elf
        .as_deref()
        .map(defmt::rtt_region_from_elf)
        .transpose()?;

    let lister = Lister::new();
    let probes = lister.list_all();

    if matches!(opts.probe, Some(ProbeInfo::List)) {
        list_probes(std::io::stdout(), &probes);
        return Ok(());
    }

    if probes.is_empty() {
        bail!(
            "No debug probes available. Make sure your probe is plugged in, supported and up-to-date."
        );
    }

    let probe_number = select_probe(&probes, opts.probe.as_ref())?;

    if probe_number >= probes.len() {
        list_probes(std::io::stderr(), &probes);
        bail!("Probe {probe_number} does not exist.");
    }

    let probe_label = format!(
        "{} {}",
        probes[probe_number].identifier,
        probes[probe_number]
            .serial_number
            .as_deref()
            .unwrap_or("(no serial number)")
    );
    let probe = match probes[probe_number].open() {
        Ok(probe) => probe,
        Err(err) => {
            bail!("Error opening probe: {err}");
        }
    };

    let target_selector = TargetSelector::from(opts.chip.as_deref());
    let chip = opts.chip.as_deref().unwrap_or("auto").to_string();

    let mut session = match probe.attach(target_selector, Permissions::default()) {
        Ok(session) => session,
        Err(err) => {
            let mut err_str = format!("Error creating debug session: {err}");

            if opts.chip.is_none() {
                if let probe_rs::Error::ChipNotFound(_) = err {
                    err_str
                        .push_str("\nHint: Use '--chip' to specify the target chip type manually");
                }
            }

            bail!("{err_str}");
        }
    };

    let automatic_scan = elf_region.is_none() && opts.scan_region.is_none();
    let scan_region = match (elf_region, opts.scan_region) {
        (Some(region), Some(_)) => {
            eprintln!("Ignoring --scan-region because --elf provides _SEGGER_RTT.");
            region
        }
        (Some(region), None) => region,
        (None, Some(region)) => region,
        (None, None) => session.target().rtt_scan_regions.clone(),
    };
    let mut core = session.core(0).context("Error attaching to core #0")?;

    ensure_supported_target(core.is_64_bit())?;

    eprintln!("Attaching to RTT...");

    let mut rtt = if automatic_scan {
        attach_region_incremental(&mut core, &scan_region)
    } else {
        Rtt::attach_region(&mut core, &scan_region)
    }
    .context("Error attaching to RTT")?;
    eprintln!("Found control block at {:#010x}", rtt.ptr());

    if opts.list {
        println!("Up channels:");
        list_channels(rtt.up_channels());

        println!("Down channels:");
        list_channels(rtt.down_channels());

        return Ok(());
    }

    let down_channel = opts
        .down
        .unwrap_or(0)
        .try_into()
        .context("down channel index cannot be represented on this host")?;

    run_session(
        &mut core,
        rtt,
        SessionConfig {
            probe: probe_label,
            chip,
            up_specs,
            down_channel,
            down_configured: !opts.no_down,
            poll_interval: Duration::from_millis(opts.poll_interval),
            reset: opts.reset,
            timestamps: opts.timestamps,
            defmt: defmt_data,
            defmt_filters,
            color: opts.color,
            log: opts.log,
            log_per_channel: opts.log_per_channel,
            log_format: opts.log_format.unwrap_or(cli::LogFormat::Decoded),
            scan_region,
            automatic_scan,
        },
    )
}

fn ensure_supported_target(is_64_bit: bool) -> Result<()> {
    if is_64_bit {
        bail!(
            "64-bit targets are not supported until probe-rs fixes 32-bit RTT offset writes on 64-bit targets"
        );
    }
    Ok(())
}

fn select_probe(probes: &[DebugProbeInfo], requested: Option<&ProbeInfo>) -> Result<usize> {
    if let Some(index) = automatic_probe_selection(probes.len(), requested)? {
        return Ok(index);
    }

    if !io::stdin().is_terminal() {
        bail!(
            "Multiple debug probes found; specify one with '--probe INDEX' when stdin is not interactive."
        );
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
            _ => {
                writeln!(output, "Invalid probe selection.")?;
            }
        }
    }
}

fn automatic_probe_selection(
    probe_count: usize,
    requested: Option<&ProbeInfo>,
) -> Result<Option<usize>> {
    match requested {
        Some(ProbeInfo::Number(index)) => Ok(Some(*index)),
        Some(ProbeInfo::List) => bail!("probe list must be handled before selecting a probe"),
        None if probe_count == 1 => Ok(Some(0)),
        None => Ok(None),
    }
}

fn list_probes(mut stream: impl std::io::Write, probes: &[DebugProbeInfo]) {
    writeln!(stream, "Available probes:").unwrap();

    write_probe_list(&mut stream, probes).unwrap();
}

fn write_probe_list(mut stream: impl std::io::Write, probes: &[DebugProbeInfo]) -> io::Result<()> {
    for (i, probe) in probes.iter().enumerate() {
        writeln!(stream, "  [{i}] {}", probe.identifier)?;
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
        if i + 1 < probes.len() {
            writeln!(stream)?;
        }
    }

    Ok(())
}

fn list_channels(channels: &[impl RttChannel]) {
    if channels.is_empty() {
        println!("  (none)");
        return;
    }

    for chan in channels.iter() {
        println!(
            "  {}: {} (buffer size {})",
            chan.number(),
            chan.name().unwrap_or("(no name)"),
            chan.buffer_size(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_selection_defaults_only_when_single_probe_exists() {
        assert_eq!(automatic_probe_selection(1, None).unwrap(), Some(0));
        assert_eq!(automatic_probe_selection(2, None).unwrap(), None);
    }

    #[test]
    fn explicit_probe_zero_is_not_treated_as_missing() {
        assert_eq!(
            automatic_probe_selection(2, Some(&ProbeInfo::Number(0))).unwrap(),
            Some(0)
        );
    }

    #[test]
    fn rejects_64_bit_targets_until_probe_rs_rtt_writes_are_fixed() {
        assert!(ensure_supported_target(false).is_ok());
        assert!(ensure_supported_target(true).is_err());
    }
}
