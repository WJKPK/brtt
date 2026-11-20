use super::core_io::{
    attach_rtt_classified, AttachOutcome, AttachmentLoss, CoreEvent, CoreSlots, Startup,
    RTT_TIMEOUT,
};
use super::logger::Logger;
use super::renderer::Renderer;
use super::session_input::{RoutingEvent, SessionCommand, SessionInput};
use super::SessionPolicy;
use crate::channel::{expand_sources, CoreId};
use crate::elf::ElfContents;
use crate::rtt::ScanRegion;
use anyhow::{anyhow, bail, Context, Result};
use crossterm::event::KeyEvent;
use probe_rs::rtt::RttChannel;
use probe_rs::Session as ProbeSession;
use std::io::{stdout, IsTerminal, Write};
use std::time::{Duration, Instant};

/// Retryable attach failures are logged and skipped; at least one core must list.
pub(crate) fn list_channels(
    mut session: ProbeSession,
    scan_region: Option<&ScanRegion>,
    elves: Vec<(u32, ElfContents)>,
) -> Result<()> {
    let startup = Startup::new(elves, scan_region, &session.target().rtt_scan_regions);
    let setups = startup.setups();
    let show_headers = setups.len() > 1 || setups.iter().any(|(id, _)| *id != CoreId::new(0));
    let mut listed = 0;
    let mut reasons = Vec::new();

    for (id, setup) in setups {
        match attach_rtt_classified(&mut session, *id, &setup.discovery, RTT_TIMEOUT)? {
            AttachOutcome::Attached(mut rtt) => {
                if show_headers {
                    println!("Core {id}:");
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

/// Cores are processed in numeric `CoreId` order rather than CLI argument order.
pub(crate) fn run_multi(
    mut session: ProbeSession,
    policy: SessionPolicy,
    scan_region: Option<&ScanRegion>,
    reset: bool,
    elves: Vec<(u32, ElfContents)>,
) -> Result<()> {
    let startup = Startup::new(elves, scan_region, &session.target().rtt_scan_regions);
    let slots = startup.attach(&mut session, &policy, reset)?;

    let output = stdout().lock();
    let mut runner = Session::new(session, slots, policy, output)?;
    runner.run()
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

enum Signal {
    Continue,
    Quit,
}

struct Session<'setup, W: Write> {
    session: ProbeSession,
    slots: CoreSlots<'setup>,
    policy: SessionPolicy,
    renderer: Renderer<W>,
    routing: SessionInput,
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
            renderer: Renderer::for_session(
                output,
                logger,
                &policy,
                include_channel,
                show_cores,
                stdout().is_terminal(),
                std::env::var_os("NO_COLOR").is_none(),
            ),
            routing: SessionInput::new(policy.down_channel, policy.down_explicit),
            policy,
        };
        runner.refresh_down_routes()?;
        runner.renderer.show_banner()?;
        Ok(runner)
    }

    fn refresh_down_routes(&mut self) -> Result<()> {
        let previous_active = self.routing.active_target();
        let event = self.routing.reconcile(
            &self.slots.routable_down(),
            &self.slots.missing_down(),
            self.slots.all_attached(),
        )?;
        if let Some(RoutingEvent::Switched { previous, target }) = event {
            self.switch_down_target(previous, target)?;
        }
        let active = self.routing.active_target();
        self.renderer.select_down_core(active)?;
        if previous_active != active && event.is_none() {
            if let Some(target) = active {
                self.renderer.show_cached_prompt(target)?;
            } else {
                for id in self.slots.ids() {
                    if self.slots.is_attached(id) && self.renderer.show_cached_prompt(id)? {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    fn switch_down_target(&mut self, previous: CoreId, target: CoreId) -> Result<()> {
        self.renderer.select_down_core(Some(target))?;
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
        self.renderer.show_cached_prompt(target)?;
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
        let mut losses = Vec::new();
        let mut had_events = false;
        let activity = {
            let Self {
                session,
                slots,
                policy,
                renderer,
                ..
            } = self;
            slots.maintain(session, now, policy, renderer, |renderer, event| {
                had_events = true;
                if let CoreEvent::AttachmentLost { id, reason } = event {
                    losses.push((id, reason));
                }
                handle_core_event(renderer, event)
            })?
        };
        for (core, reason) in losses {
            self.discard_pending(core, reason);
        }
        self.refresh_down_routes()?;
        if had_events {
            self.renderer
                .flush_output()
                .context("Error writing to stdout")?;
        }
        if activity {
            self.renderer.flush_data()?;
        }
        if let Some(key_event) = self.wait_for_key(activity)? {
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
        if matches!(
            key_event.kind,
            crossterm::event::KeyEventKind::Press | crossterm::event::KeyEventKind::Repeat
        ) {
            self.renderer.key_pressed(Instant::now())?;
        }
        match self.routing.handle_key(key_event) {
            Some(command) => {
                let signal = self.dispatch_command(command)?;
                if matches!(signal, Signal::Continue) {
                    self.renderer
                        .flush_output()
                        .context("Error writing to stdout")?;
                }
                Ok(signal)
            }
            None => Ok(Signal::Continue),
        }
    }

    fn dispatch_command(&mut self, command: SessionCommand) -> Result<Signal> {
        if command == SessionCommand::Quit {
            return Ok(Signal::Quit);
        }
        if command == SessionCommand::CycleDownCore {
            if let Some(RoutingEvent::Switched { previous, target }) = self.routing.cycle() {
                self.renderer.suspend_foreground()?;
                self.switch_down_target(previous, target)?;
            }
            return Ok(Signal::Continue);
        }
        let restore_foreground = command != SessionCommand::ResetTarget;
        let suspended = self.renderer.suspend_foreground()?;

        match command {
            SessionCommand::Quit => unreachable!("quit handled above"),
            SessionCommand::Help => self.renderer.show_help()?,
            SessionCommand::ShowConfig => self.renderer.show_config(
                &self.slots.ids(),
                &self.policy,
                self.routing.active_target(),
            )?,
            SessionCommand::ClearScreen => self.renderer.clear_screen()?,
            SessionCommand::ToggleTimestamps => self.renderer.toggle_timestamps()?,
            SessionCommand::ResetTarget => {
                self.slots.chip_reset(&mut self.session)?;
                self.renderer.reset_target_epoch()?;
                self.routing.clear_all();
                self.refresh_down_routes()?;
                self.renderer.notice_target_reset()?;
            }
            SessionCommand::CycleDownCore => unreachable!("cycle handled before suspending"),
        }

        if restore_foreground {
            if let Some(saved) = suspended {
                self.renderer.restore_foreground(saved)?;
            }
        }
        Ok(Signal::Continue)
    }

    fn discard_pending(&mut self, core: CoreId, reason: AttachmentLoss) {
        let discarded = self.routing.attachment_lost(core);
        if discarded > 0 {
            let cause = match reason {
                AttachmentLoss::MetadataChanged => "RTT metadata changed",
                AttachmentLoss::TransportUnavailable => "probe transport unavailable",
            };
            log::warn!("core {core}: {cause}; discarded {discarded} pending down byte(s)");
        }
    }

    fn flush_pending_input(&mut self) -> Result<()> {
        let mut events = Vec::new();
        self.routing.flush_pending(|id, bytes| {
            let write = self.slots.write_down(&mut self.session, id, bytes)?;
            if let Some(event) = write.event {
                events.push(event);
            }
            Ok(write.count)
        })?;
        for &event in &events {
            handle_core_event(&mut self.renderer, event)?;
            if let CoreEvent::AttachmentLost { id, reason } = event {
                self.discard_pending(id, reason);
            }
        }
        if !events.is_empty() {
            self.refresh_down_routes()?;
            self.renderer
                .flush_output()
                .context("Error writing to stdout")?;
        }
        Ok(())
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
        CoreEvent::AttachmentLost { id, .. } => renderer.reset_core_epoch(id)?,
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
