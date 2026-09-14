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
    run_loop(core, rtt, config)
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

enum Signal {
    Continue,
    Quit,
}

struct Session<'probe, 'config, W: Write> {
    target: TargetIo<'probe, 'config>,
    renderer: Renderer<W>,
    input: Option<InteractiveInput>,
    config: &'config SessionConfig,
}

impl<'probe, 'config, W: Write> Session<'probe, 'config, W> {
    fn new(
        core: Core<'probe>,
        rtt: Rtt,
        config: &'config SessionConfig,
        output: W,
    ) -> Result<Self> {
        let mut target = TargetIo::new(core, rtt, config)?;
        if config.reset {
            target.reset_and_reattach()?;
        }
        target.validate_channels()?;

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
        let input = InteractiveInput::new(config.down_channel, down_channel_present)?;

        let mut renderer = Renderer::for_session(output, logger, config);
        renderer.show_banner()?;

        Ok(Self {
            target,
            renderer,
            input,
            config,
        })
    }

    fn run(&mut self) -> Result<()> {
        let result = loop {
            match self.tick() {
                Ok(Signal::Continue) => {}
                Ok(Signal::Quit) => break Ok(()),
                Err(err) => break Err(err),
            }
        };

        let cleanup = self.renderer.finish_session();
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

    fn tick(&mut self) -> Result<Signal> {
        let stats = match self.target.poll(&mut self.renderer)? {
            PollOutcome::Data(stats) => stats,
            PollOutcome::Reattach => {
                self.reattach()?;
                return Ok(Signal::Continue);
            }
        };

        let had_data = stats.bytes > 0 || stats.messages > 0;
        if had_data {
            self.renderer.flush_data()?;
        }

        if self.input.is_some() {
            let timeout = if had_data {
                Duration::ZERO
            } else {
                self.config.poll_interval
            };
            if event::poll(timeout)? {
                if let Event::Key(key_event) = event::read()? {
                    if let Signal::Quit = self.handle_key(key_event)? {
                        return Ok(Signal::Quit);
                    }
                }
            }
        } else if !had_data {
            std::thread::sleep(self.config.poll_interval);
        }

        self.flush_pending_input()?;
        Ok(Signal::Continue)
    }

    fn handle_key(&mut self, key_event: crossterm::event::KeyEvent) -> Result<Signal> {
        let action = {
            let interactive = self
                .input
                .as_mut()
                .expect("input is Some when polling keys");
            let (next_state, action) = interactive.escape_state.handle_key(key_event);
            interactive.escape_state = next_state;
            action
        };

        match action {
            InputAction::Send(bytes) => {
                if self.renderer.local_echo_enabled() {
                    self.renderer
                        .render_local_echo(&bytes)
                        .context("Error writing local echo")?;
                    self.renderer
                        .flush_output()
                        .context("Error writing to stdout")?;
                }
                self.input
                    .as_mut()
                    .expect("input is Some when polling keys")
                    .queue(&bytes);
                Ok(Signal::Continue)
            }
            InputAction::Command(command) => {
                let quit = self.dispatch_command(command)?;
                if !quit {
                    self.renderer
                        .flush_output()
                        .context("Error writing to stdout")?;
                }
                Ok(if quit { Signal::Quit } else { Signal::Continue })
            }
            InputAction::Ignore => Ok(Signal::Continue),
        }
    }

    fn dispatch_command(&mut self, command: SessionCommand) -> Result<bool> {
        if command == SessionCommand::Quit {
            return Ok(true);
        }
        let restore_foreground = command != SessionCommand::ResetTarget;
        let suspended = self.renderer.suspend_foreground()?;

        match command {
            SessionCommand::Quit => unreachable!("quit handled above"),
            SessionCommand::Help => self.renderer.show_help()?,
            SessionCommand::ShowConfig => self.renderer.show_config(self.config)?,
            SessionCommand::ClearScreen => self.renderer.clear_screen()?,
            SessionCommand::ToggleTimestamps => self.renderer.toggle_timestamps()?,
            SessionCommand::ToggleLocalEcho => self.renderer.toggle_local_echo()?,
            SessionCommand::ResetTarget => {
                self.target.reset_and_reattach()?;
                self.target.reset_epoch()?;
                self.renderer.reset_target_epoch()?;
                if let Some(input) = self.input.as_mut() {
                    input.clear_queued_bytes();
                }
                self.renderer.notice_target_reset()?;
            }
        }

        if restore_foreground {
            if let Some(saved) = suspended {
                self.renderer.restore_foreground(saved)?;
            }
        }
        Ok(false)
    }

    fn reattach(&mut self) -> Result<()> {
        self.target.reattach()?;
        self.target.reset_epoch()?;
        self.renderer.reset_target_epoch()?;
        if let Some(input) = self.input.as_mut() {
            input.clear_queued_bytes();
        }
        Ok(self.renderer.notice_reattached()?)
    }

    fn flush_pending_input(&mut self) -> Result<()> {
        let Some(interactive) = self.input.as_mut() else {
            return Ok(());
        };
        if interactive.has_pending() {
            let count = self.target.write_down(interactive.pending_bytes())?;
            interactive.consume_sent(count);
        }
        Ok(())
    }
}

fn run_loop(core: Core<'_>, rtt: Rtt, config: SessionConfig) -> Result<()> {
    let output = BufWriter::new(stdout().lock());
    let mut session = Session::new(core, rtt, &config, output)?;
    session.run()
}

#[cfg(test)]
#[path = "../tests/session.rs"]
mod tests;
