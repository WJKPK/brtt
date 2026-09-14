use crate::cli::{LogDestination, Opts, SessionConfig};
use crate::defmt::DefmtData;
use crate::input::{InputAction, InteractiveInput, SessionCommand};
use crate::logger::Logger;
use crate::probe_handler::AttachedProbe;
use crate::renderer::Renderer;
use crate::target::{attach_initial_rtt, PollOutcome, TargetIo};
use anyhow::{Context, Result};
use brtt::rtt::{Rtt, RttDiscovery, ScanRegion};
use brtt::RttChannel;
use crossterm::event::{self, Event};
use probe_rs::Core;
use std::io::{stdout, BufWriter, Write};
use std::time::Duration;

/// Runs a target-dependent operation: attach to the probe session's core, find
/// the RTT control block, then either list channels or run the read loop.
pub(crate) fn start(
    attached: AttachedProbe,
    opts: Opts,
    defmt: Option<DefmtData>,
    elf_region: Option<ScanRegion>,
) -> Result<()> {
    let AttachedProbe {
        mut session,
        label,
        chip,
    } = attached;

    let discovery = resolve_scan_region(
        elf_region.as_ref(),
        opts.scan_region.as_ref(),
        &session.target().rtt_scan_regions,
    );

    let mut core = session.core(0).context("Error attaching to core #0")?;
    let mut rtt = attach_initial_rtt(&mut core, &discovery)?;

    if opts.list {
        println!("Up channels:");
        list_channels(rtt.up_channels());

        println!("Down channels:");
        list_channels(rtt.down_channels());

        return Ok(());
    }

    let config = SessionConfig::from_opts(opts, label, chip, defmt, discovery)?;
    run_loop(&mut core, rtt, config)
}

/// Precedence: ELF-provided `_SEGGER_RTT` > explicit `--scan-region` > target default.
/// Automatic scanning is only used when neither the ELF nor the user pins a region.
fn resolve_scan_region(
    elf_region: Option<&ScanRegion>,
    requested: Option<&ScanRegion>,
    target_default: &ScanRegion,
) -> RttDiscovery {
    match (elf_region, requested) {
        (Some(region), Some(_)) => {
            eprintln!("Ignoring --scan-region because --elf provides _SEGGER_RTT.");
            RttDiscovery::Fixed(region.clone())
        }
        (Some(region), None) | (None, Some(region)) => RttDiscovery::Fixed(region.clone()),
        (None, None) => RttDiscovery::Incremental(target_default.clone()),
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

fn dispatch_command<W: Write>(
    command: SessionCommand,
    config: &SessionConfig,
    target: &mut TargetIo<'_, '_, '_>,
    input: &mut Option<InteractiveInput>,
    renderer: &mut Renderer<'_, W>,
) -> Result<bool> {
    if command == SessionCommand::Quit {
        return Ok(true);
    }
    let restore_foreground = command != SessionCommand::ResetTarget;
    let suspended = renderer.suspend_foreground()?;
    match command {
        SessionCommand::Quit => unreachable!("quit handled before rendering command output"),
        SessionCommand::Help => renderer.show_help()?,
        SessionCommand::ShowConfig => renderer.show_config(config)?,
        SessionCommand::ClearScreen => renderer.clear_screen()?,
        SessionCommand::ToggleTimestamps => renderer.toggle_timestamps()?,
        SessionCommand::ToggleLocalEcho => renderer.toggle_local_echo()?,
        SessionCommand::ResetTarget => {
            target.reset_and_reattach()?;
            target.reset_epoch(config)?;
            renderer.reset_target_epoch()?;
            if let Some(input) = input.as_mut() {
                input.clear_queued_bytes();
            }
            renderer.notice_target_reset()?;
        }
    }

    if restore_foreground {
        if let Some(saved) = suspended {
            renderer.restore_foreground(saved)?;
        }
    }

    Ok(false)
}

/// Reattaches after the target restarted and RTT state changed under us.
fn reattach_target<W: Write>(
    target: &mut TargetIo<'_, '_, '_>,
    renderer: &mut Renderer<'_, W>,
    config: &SessionConfig,
    input: &mut Option<InteractiveInput>,
) -> Result<()> {
    target.reattach()?;
    target.reset_epoch(config)?;
    renderer.reset_target_epoch()?;
    if let Some(input) = input.as_mut() {
        input.clear_queued_bytes();
    }
    renderer.notice_reattached()?;
    Ok(())
}

fn run_loop(core: &mut Core, rtt: Rtt, config: SessionConfig) -> Result<()> {
    let mut target = TargetIo::new(core, rtt, &config)?;
    if config.reset {
        target.reset_and_reattach()?;
    }
    target.validate_channels(&config)?;

    let include_channel = config.up_specs.len() > 1;
    let logger = match config.log.as_ref() {
        Some(log) => {
            let (path, per_channel) = match &log.destination {
                LogDestination::Merged(path) => (path.as_path(), false),
                LogDestination::PerChannel(path) => (path.as_path(), true),
            };
            Logger::new(Some(path), per_channel, log.format, include_channel)?
        }
        None => None,
    };

    let down_channel_present = config
        .down_channel
        .is_some_and(|channel| target.has_down_channel(channel));
    let mut input = InteractiveInput::new(config.down_channel, down_channel_present)?;

    let output = BufWriter::new(stdout().lock());
    let mut renderer =
        Renderer::for_session(output, logger, config.defmt_filters.as_deref(), &config);
    renderer.show_banner()?;

    let result = 'read_loop: loop {
        let stats = match target.poll(&mut renderer) {
            Ok(PollOutcome::Data(stats)) => stats,
            Ok(PollOutcome::Reattach) => {
                if let Err(err) = reattach_target(&mut target, &mut renderer, &config, &mut input) {
                    break 'read_loop Err(err);
                }
                continue 'read_loop;
            }
            Err(err) => break 'read_loop Err(err),
        };

        let had_data = stats.bytes > 0 || stats.messages > 0;
        if had_data {
            if let Err(err) = renderer.flush_data() {
                break 'read_loop Err(err);
            }
        }

        if let Some(interactive) = input.as_mut() {
            let timeout = if had_data {
                Duration::ZERO
            } else {
                config.poll_interval
            };
            let input_ready = match event::poll(timeout) {
                Ok(ready) => ready,
                Err(err) => break 'read_loop Err(err.into()),
            };
            if input_ready {
                let event = match event::read() {
                    Ok(event) => event,
                    Err(err) => break 'read_loop Err(err.into()),
                };
                if let Event::Key(key_event) = event {
                    let (next_state, action) = interactive.escape_state.handle_key(key_event);
                    interactive.escape_state = next_state;

                    match action {
                        InputAction::Send(bytes) => {
                            if renderer.local_echo_enabled() {
                                if let Err(err) = renderer.render_local_echo(&bytes) {
                                    break 'read_loop Err(anyhow::anyhow!(
                                        "Error writing local echo: {err}"
                                    ));
                                }
                                if let Err(err) = renderer.flush_output() {
                                    break 'read_loop Err(anyhow::anyhow!(
                                        "Error writing to stdout: {err}"
                                    ));
                                }
                            }
                            interactive.queue(&bytes);
                        }
                        InputAction::Command(command) => {
                            match dispatch_command(
                                command,
                                &config,
                                &mut target,
                                &mut input,
                                &mut renderer,
                            ) {
                                Ok(true) => break 'read_loop Ok(()),
                                Ok(false) => {
                                    if let Err(err) = renderer.flush_output() {
                                        break 'read_loop Err(anyhow::anyhow!(
                                            "Error writing to stdout: {err}"
                                        ));
                                    }
                                }
                                Err(err) => break 'read_loop Err(err),
                            }
                        }
                        InputAction::Ignore => {}
                    }
                }
            }
        } else if !had_data {
            std::thread::sleep(config.poll_interval);
        }

        if let Some(interactive) = input.as_mut() {
            if interactive.has_pending() {
                let written = target.write_down(interactive.pending_bytes());
                match written {
                    Ok(count) => interactive.consume_sent(count),
                    Err(err) => break 'read_loop Err(err),
                }
            }
        }
    };

    let cleanup = renderer.finish_session();
    match (result, cleanup) {
        (Err(primary), Err(cleanup)) => {
            log::error!("session cleanup also failed: {cleanup:#}");
            Err(primary)
        }
        (Err(primary), Ok(())) => Err(primary),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Ok(()), Ok(())) => Ok(()),
    }
}

#[cfg(test)]
#[path = "../tests/session.rs"]
mod tests;
