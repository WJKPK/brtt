use crate::channel::CoreId;
use crate::cli::{expand_sources, ChannelSpec, Opts, SessionPolicy};
use crate::defmt::{DefmtData, ElfContents};
use crate::input::{DownRouting, InputAction, SessionCommand};
use crate::logger::Logger;
use crate::probe_handler::AttachedProbe;
use crate::renderer::Renderer;
use crate::target::{
    attach_rtt_classified, AttachOutcome, CoreEvent, CoreSetup, CoreSlots, RTT_TIMEOUT,
};
use anyhow::{anyhow, bail, Context, Result};
use brtt::rtt::{RttDiscovery, ScanRegion};
use brtt::RttChannel;
use crossterm::event::KeyEvent;
use probe_rs::Session as ProbeSession;
use std::io::{stdout, Write};
use std::time::{Duration, Instant};

/// `--list`: attach to each configured core, find its RTT control block and
/// print the channels, then return. Retryable absences are reported on stderr
/// and skipped; permanent failures abort; at least one core must list.
pub(crate) fn list_channels(
    attached: AttachedProbe,
    opts: &Opts,
    elves: Vec<(u32, ElfContents)>,
) -> Result<()> {
    let AttachedProbe { mut session, .. } = attached;
    let inputs = core_inputs(elves);
    let show_headers = inputs.len() > 1 || inputs.iter().any(|input| input.id != CoreId::new(0));
    let mut listed = 0;
    let mut reasons = Vec::new();

    for input in inputs {
        let discovery = resolve_scan_region(
            input.elf_region.as_ref(),
            opts.scan_region.as_ref(),
            &session.target().rtt_scan_regions,
        );
        match attach_rtt_classified(&mut session, input.id, &discovery, RTT_TIMEOUT)? {
            AttachOutcome::Attached(mut rtt) => {
                if show_headers {
                    println!("Core {}:", input.id);
                }
                println!("Up channels:");
                print_channels(rtt.up_channels());
                println!("Down channels:");
                print_channels(rtt.down_channels());
                listed += 1;
            }
            AttachOutcome::Retryable(reason) => {
                log::warn!("{reason}; skipping");
                reasons.push(reason);
            }
            AttachOutcome::Fatal(error) => return Err(error),
        }
    }
    if listed == 0 {
        bail!(
            "no RTT targets could be listed ({}); ensure the cores are running and the ELF addresses match the firmware",
            reasons.join("; ")
        );
    }
    Ok(())
}

/// Runs the interactive session over every configured core: attach, find each
/// RTT control block, then read and render until the user quits. Per-core order
/// follows the numeric index, never the CLI order.
pub(crate) fn run_multi(
    attached: AttachedProbe,
    opts: Opts,
    elves: Vec<(u32, ElfContents)>,
    up_specs: &[ChannelSpec],
) -> Result<()> {
    let AttachedProbe {
        mut session,
        label,
        chip,
    } = attached;
    let policy = SessionPolicy::from_opts(&opts, label, chip, up_specs)?;
    let setups = core_inputs(elves)
        .into_iter()
        .map(|input| {
            let discovery = resolve_scan_region(
                input.elf_region.as_ref(),
                opts.scan_region.as_ref(),
                &session.target().rtt_scan_regions,
            );
            (
                input.id,
                CoreSetup {
                    discovery,
                    defmt: input.defmt,
                },
            )
        })
        .collect::<Vec<_>>();
    let mut slots = CoreSlots::build(&setups, &policy)?;

    // Reset before the first RTT attach so --reset can also recover a target
    // whose RTT block is not initialized yet.
    if opts.reset {
        slots.chip_reset(&mut session)?;
    }
    let startup = slots.attach_all(&mut session, &policy, RTT_TIMEOUT)?;
    if startup.attached_count() == 0 {
        return Err(startup.error());
    }
    if startup.has_pending() {
        log::info!(
            "continuing with {} core(s); pending cores retry in the background",
            startup.attached_count()
        );
    }

    let output = stdout().lock();
    let mut runner = Session::new(session, slots, policy, output)?;
    runner.run()
}

/// Precedence: ELF-provided `_SEGGER_RTT` > explicit `--scan-region` > target default.
/// Automatic scanning is only used when neither the ELF nor the user pins a region.
pub(crate) fn resolve_scan_region(
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
    for chan in channels {
        println!(
            "  {}: {} (buffer size {})",
            chan.number(),
            chan.name().unwrap_or("(no name)"),
            chan.buffer_size(),
        );
    }
}

struct CoreInput {
    id: CoreId,
    elf_region: Option<ScanRegion>,
    defmt: Option<DefmtData>,
}

/// Moves ELF-derived data exactly once and establishes numeric core order for
/// both listing and session modes.
fn core_inputs(elves: Vec<(u32, ElfContents)>) -> Vec<CoreInput> {
    let mut inputs = if elves.is_empty() {
        vec![CoreInput {
            id: CoreId::new(0),
            elf_region: None,
            defmt: None,
        }]
    } else {
        elves
            .into_iter()
            .map(|(id, contents)| CoreInput {
                id: CoreId::new(id),
                elf_region: Some(contents.region),
                defmt: contents.defmt,
            })
            .collect()
    };
    inputs.sort_by_key(|input| input.id);
    inputs
}

enum Signal {
    Continue,
    Quit,
}

struct Session<'setup, W: Write> {
    session: ProbeSession,
    slots: CoreSlots<'setup>,
    policy: SessionPolicy,
    renderer: Renderer<W>,
    routing: DownRouting,
}

impl<'setup, W: Write> Session<'setup, W> {
    fn new(
        session: ProbeSession,
        slots: CoreSlots<'setup>,
        policy: SessionPolicy,
        output: W,
    ) -> Result<Self> {
        let ids = slots.ids();
        let indices: Vec<u32> = ids.iter().map(|id| id.value()).collect();
        let sources = expand_sources(&policy.up_specs, &indices);
        for source in &sources {
            log::debug!(
                "core {}: up {}:{}",
                source.core,
                source.channel,
                source.mode.name()
            );
        }
        let include_channel = sources.len() > 1;
        let show_cores = ids.len() > 1;
        let logger = match policy.log.as_ref() {
            Some(log) => Logger::new(
                Some(&log.destination),
                log.format,
                include_channel,
                show_cores,
            )?,
            None => None,
        };

        let mut runner = Self {
            session,
            slots,
            renderer: Renderer::for_session(output, logger, &policy, include_channel, show_cores),
            policy,
            routing: DownRouting::new(),
        };
        runner.refresh_down_routes()?;
        runner.renderer.show_banner()?;
        Ok(runner)
    }

    fn refresh_down_routes(&mut self) -> Result<()> {
        let event = self.routing.reconcile(
            &self.slots.routable_down(),
            &self.slots.missing_down(),
            self.slots.all_attached(),
            self.policy.down_channel,
            self.policy.down_explicit,
        )?;
        if let Some(crate::input::RoutingEvent::Switched { previous, target }) = event {
            self.switch_down_target(previous, target)?;
        }
        Ok(())
    }

    fn switch_down_target(&mut self, previous: CoreId, target: CoreId) -> Result<()> {
        if self.slots.is_attached(previous) {
            self.renderer.erase_core_prompt(previous)?;
        } else {
            self.renderer.reset_core_epoch(previous)?;
        }
        let channel = self
            .policy
            .down_channel
            .expect("keyboard input implies a down channel");
        self.renderer.notice_down_target(target, channel)?;
        if !self.renderer.show_cached_prompt(target)? {
            self.routing.queue(b"\n");
        }
        Ok(())
    }

    fn run(&mut self) -> Result<()> {
        let primary = loop {
            match self.tick() {
                Ok(Signal::Continue) => {}
                Ok(Signal::Quit) => break Ok(()),
                Err(error) => break Err(error),
            }
        };
        let cleanup = self.renderer.finish_session();
        digest_result(primary, cleanup)
    }

    fn tick(&mut self) -> Result<Signal> {
        let now = Instant::now();
        let stats = {
            let Self {
                session,
                slots,
                policy,
                renderer,
                ..
            } = self;
            slots.maintain(session, now, policy, renderer, handle_core_event)?
        };
        self.refresh_down_routes()?;

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
        let timeout = if had_data {
            Duration::ZERO
        } else {
            self.policy.poll_interval
        };
        self.routing.wait_for_key(timeout)
    }

    fn handle_key(&mut self, key_event: KeyEvent) -> Result<Signal> {
        match self.routing.handle_key(key_event) {
            InputAction::Send(bytes) => {
                self.routing.queue(&bytes);
                Ok(Signal::Continue)
            }
            InputAction::Command(command) => {
                let signal = self.dispatch_command(command)?;
                if matches!(signal, Signal::Continue) {
                    self.renderer
                        .flush_output()
                        .context("Error writing to stdout")?;
                }
                Ok(signal)
            }
            InputAction::Ignore => Ok(Signal::Continue),
        }
    }

    fn dispatch_command(&mut self, command: SessionCommand) -> Result<Signal> {
        if command == SessionCommand::Quit {
            return Ok(Signal::Quit);
        }
        let restore_foreground = !matches!(
            command,
            SessionCommand::ResetTarget | SessionCommand::CycleDownCore
        );
        let suspended = self.renderer.suspend_foreground()?;

        match command {
            SessionCommand::Quit => unreachable!("quit handled above"),
            SessionCommand::Help => self.renderer.show_help()?,
            SessionCommand::ShowConfig => {
                self.renderer
                    .show_config(&self.slots.ids(), &self.policy, self.routing.target())?
            }
            SessionCommand::ClearScreen => self.renderer.clear_screen()?,
            SessionCommand::ToggleTimestamps => self.renderer.toggle_timestamps()?,
            SessionCommand::ResetTarget => {
                self.slots.chip_reset(&mut self.session)?;
                self.renderer.reset_target_epoch()?;
                self.routing.clear_all();
                self.refresh_down_routes()?;
                self.renderer.notice_target_reset()?;
            }
            SessionCommand::CycleDownCore => {
                if let Some((previous, target)) = self.routing.cycle() {
                    self.switch_down_target(previous, target)?;
                }
            }
        }

        if restore_foreground {
            if let Some(saved) = suspended {
                self.renderer.restore_foreground(saved)?;
            }
        }
        Ok(Signal::Continue)
    }

    fn flush_pending_input(&mut self) -> Result<()> {
        let Self {
            session,
            slots,
            routing,
            ..
        } = self;
        routing.flush_pending(|id, bytes| slots.write_down(session, id, bytes))
    }
}

fn handle_core_event<W: Write>(renderer: &mut Renderer<W>, event: CoreEvent) -> Result<()> {
    match event {
        CoreEvent::Attached { id, recovered } => {
            renderer.reset_core_epoch(id)?;
            if recovered {
                log::info!("core {id} available again");
                renderer.notice_reattached(id)?;
            } else {
                log::info!("core {id} attached");
            }
        }
        CoreEvent::RttBlockChanged { id } => renderer.reset_core_epoch(id)?,
    }
    Ok(())
}

fn digest_result(primary: Result<()>, cleanup: Result<()>) -> Result<()> {
    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Err(primary), Err(cleanup)) => Err(anyhow!(
            "{primary:#}; Error during session cleanup: {cleanup:#}"
        )),
    }
}

#[cfg(test)]
#[path = "../tests/session.rs"]
mod tests;
