mod cli;
mod session;

use brtt::channel::RttChannel;
use brtt::rtt::Rtt;

use anyhow::{bail, Context, Result};
use clap::Parser;
use cli::{configured_up_specs, selected_channel, Opts, ProbeInfo};
use probe_rs::{config::TargetSelector, probe::list::Lister, probe::DebugProbeInfo, Permissions};
use session::{run_session, SessionConfig};
use std::time::Duration;

fn main() -> Result<()> {
    env_logger::init();
    let opts = Opts::parse();

    let lister = Lister::new();
    let probes = lister.list_all();

    if probes.is_empty() {
        bail!(
            "No debug probes available. Make sure your probe is plugged in, supported and up-to-date."
        );
    }

    let probe_number = match opts.probe {
        ProbeInfo::List => {
            list_probes(std::io::stdout(), &probes);
            return Ok(());
        }
        ProbeInfo::Number(i) => i,
    };

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

    let mut core = session.core(0).context("Error attaching to core # 0")?;

    eprintln!("Attaching to RTT...");

    let mut rtt =
        Rtt::attach_region(&mut core, &opts.scan_region).context("Error attaching to RTT")?;
    eprintln!("Found control block at {:#010x}", rtt.ptr());

    if opts.list {
        println!("Up channels:");
        list_channels(rtt.up_channels());

        println!("Down channels:");
        list_channels(rtt.down_channels());

        return Ok(());
    }

    let up_specs = configured_up_specs(&opts.up);
    let down_channel = selected_channel(&opts.down, "down")?;

    run_session(
        &mut core,
        rtt,
        SessionConfig {
            probe: probe_label,
            chip,
            up_specs,
            up_configured: !opts.up.is_empty(),
            down_channel,
            down_configured: !opts.down.is_empty(),
            poll_interval: Duration::from_millis(opts.poll_interval),
            reset: opts.reset,
        },
    )
}

fn list_probes(mut stream: impl std::io::Write, probes: &[DebugProbeInfo]) {
    writeln!(stream, "Available probes:").unwrap();

    for (i, probe) in probes.iter().enumerate() {
        writeln!(
            stream,
            "  {}: {} {}",
            i,
            probe.identifier,
            probe
                .serial_number
                .as_deref()
                .unwrap_or("(no serial number)")
        )
        .unwrap();
    }
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
