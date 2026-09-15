use crate::cli::{Opts, SessionConfig};
use crate::defmt::DefmtData;
use crate::input::{InputAction, InteractiveInput, SessionCommand};
use crate::logger::Logger;
use crate::probe_handler::AttachedProbe;
use crate::renderer::Renderer;
use crate::target::{attach_initial_rtt, PollOutcome, TargetIo};
use anyhow::{anyhow, Context, Result};
use brtt::rtt::{Rtt, RttDiscovery, ScanRegion};
use brtt::RttChannel;
use crossterm::event::KeyEvent;
use probe_rs::{Core, Session as ProbeSession};
use std::io::{stdout, BufWriter, Write};

/// `--list`: attach to the target, find the RTT control block and print the
/// channels, then return.
pub(crate) fn list_channels(
    attached: AttachedProbe,
    opts: &Opts,
    elf_region: Option<&ScanRegion>,
) -> Result<()> {
    let AttachedProbe { mut session, .. } = attached;
    let (_core, mut rtt, _) = open_rtt(&mut session, elf_region, opts.scan_region.as_ref())?;

    println!("Up channels:");
    print_channels(rtt.up_channels());

    println!("Down channels:");
    print_channels(rtt.down_channels());

    Ok(())
}

/// Runs the interactive session: attach, find the RTT control block, then
/// read and render until the user quits.
pub(crate) fn run(
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

    let (core, rtt, discovery) =
        open_rtt(&mut session, elf_region.as_ref(), opts.scan_region.as_ref())?;

    let config = SessionConfig::from_opts(opts, label, chip, defmt, discovery)?;
    run_loop(core, rtt, config)
}

/// Attaches to core 0, resolves the scan region and finds the RTT control
/// block. Shared by channel listing and the session so they cannot drift.
fn open_rtt<'probe>(
    session: &'probe mut ProbeSession,
    elf_region: Option<&ScanRegion>,
    requested: Option<&ScanRegion>,
) -> Result<(Core<'probe>, Rtt, RttDiscovery)> {
    let discovery = resolve_scan_region(elf_region, requested, &session.target().rtt_scan_regions);
    let mut core = session.core(0).context("Error attaching to core #0")?;
    let rtt = attach_initial_rtt(&mut core, &discovery)?;
    Ok((core, rtt, discovery))
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
            log::info!("ignoring --scan-region because --elf provides _SEGGER_RTT.");
            RttDiscovery::Fixed(region.clone())
        }
        (Some(region), None) | (None, Some(region)) => RttDiscovery::Fixed(region.clone()),
        (None, None) => RttDiscovery::Incremental(target_default.clone()),
    }
}

fn print_channels(channels: &[impl RttChannel]) {
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
            Some(log) => Logger::new(Some(&log.destination), log.format, include_channel)?,
            None => None,
        };

        // validate_channels() above already bailed if the configured down
        // channel is missing, so presence here is just "was one configured".
        let down_channel_present = config.down_channel.is_some();
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
        let primary = loop {
            match self.tick() {
                Ok(Signal::Continue) => {}
                Ok(Signal::Quit) => break Ok(()),
                Err(err) => break Err(err),
            }
        };

        let cleanup = self.renderer.finish_session();
        digest_result(primary, cleanup)
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

        if let Some(key_event) = self.wait_for_key(had_data)? {
            if let Signal::Quit = self.handle_key(key_event)? {
                return Ok(Signal::Quit);
            }
        }

        self.flush_pending_input()?;
        Ok(Signal::Continue)
    }

    fn wait_for_key(&self, had_data: bool) -> Result<Option<KeyEvent>> {
        // Do not delay the next RTT read when data was available. When idle,
        // this wait also provides the keyboard poll and CPU backoff.
        let timeout = if !had_data {
            self.config.poll_interval
        } else {
            Default::default()
        };

        match self.input {
            Some(_) => InteractiveInput::poll_key(timeout),
            None => {
                std::thread::sleep(timeout);
                Ok(None)
            }
        }
    }

    fn handle_key(&mut self, key_event: KeyEvent) -> Result<Signal> {
        let action = {
            let Some(interactive) = self.input.as_mut() else {
                return Ok(Signal::Continue);
            };
            let (next_state, action) = interactive.escape_state.handle_key(key_event);
            interactive.escape_state = next_state;
            action
        };

        match action {
            InputAction::Send(bytes) => {
                if let Some(interactive) = self.input.as_mut() {
                    interactive.queue(&bytes);
                }
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

/// Simplify error, prefer primary
fn digest_result(primary: Result<()>, cleanup: Result<()>) -> Result<()> {
    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Err(primary), Ok(())) => Err(primary),
        (Err(primary), Err(cleanup)) => Err(anyhow!(
            "session failed: {primary:#}; cleanup also failed: {cleanup:#}"
        )),
    }
}

fn run_loop(core: Core<'_>, rtt: Rtt, config: SessionConfig) -> Result<()> {
    let output = BufWriter::new(stdout().lock());
    let mut session = Session::new(core, rtt, &config, output)?;
    session.run()
}
