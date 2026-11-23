use super::channel_decode::ChannelDecoders;
use super::core_io::{
    attach_rtt_classified, resolve_rtt_discoveries, AttachOutcome, AttachmentLoss, CoreEvent,
    CoreSlots, Startup, RTT_TIMEOUT,
};
use super::logger::Logger;
use super::renderer::Renderer;
use super::session_input::{RoutingEvent, SessionCommand, SessionInput};
use super::SessionPolicy;
use crate::channel::{expand_sources, CoreId, SourceLabel};
use crate::elf::ElfContents;
use crate::probe::{AttachedTarget, ProbeIdentity};
use crate::rtt::ScanRegion;
use anyhow::{anyhow, bail, Context, Result};
use crossterm::event::KeyEvent;
use probe_rs::rtt::RttChannel;
use std::time::{Duration, Instant};

/// Retryable attach failures are logged and skipped; at least one core must list.
pub(crate) fn list_channels(
    target: AttachedTarget,
    scan_region: Option<&ScanRegion>,
    rtt_symbols: Vec<(CoreId, Option<u64>)>,
) -> Result<()> {
    let mut session = target.session;
    let discoveries =
        resolve_rtt_discoveries(rtt_symbols, scan_region, &session.target().rtt_scan_regions);
    let show_headers =
        discoveries.len() > 1 || discoveries.iter().any(|(id, _)| *id != CoreId::new(0));
    let mut listed = 0;
    let mut reasons = Vec::new();

    for (id, discovery) in &discoveries {
        match attach_rtt_classified(&mut session, *id, discovery, RTT_TIMEOUT)? {
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
    target: AttachedTarget,
    policy: SessionPolicy,
    scan_region: Option<&ScanRegion>,
    reset: bool,
    elves: Vec<(CoreId, ElfContents)>,
) -> Result<()> {
    let AttachedTarget { session, identity } = target;
    let startup = Startup::new(
        elves,
        scan_region,
        &session.target().rtt_scan_regions,
        &policy,
    );
    let slots = startup.attach(session, reset)?;
    let decoders = startup.decoders()?;
    let mut runner = Session::new(identity, slots, decoders, policy)?;
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

struct Session<'setup> {
    identity: ProbeIdentity,
    slots: CoreSlots<'setup>,
    decoders: ChannelDecoders<'setup>,
    policy: SessionPolicy,
    renderer: Renderer,
    routing: SessionInput,
}

impl<'setup> Session<'setup> {
    fn new(
        identity: ProbeIdentity,
        slots: CoreSlots<'setup>,
        decoders: ChannelDecoders<'setup>,
        policy: SessionPolicy,
    ) -> Result<Self> {
        let ids = slots.ids();
        let sources = expand_sources(&policy.up_specs, &ids);
        for source in &sources {
            log::debug!(
                "core {}: up {}:{}",
                source.core,
                source.channel,
                source.mode.name()
            );
        }
        let label = SourceLabel::for_session(sources.len(), ids.len());
        let logger = match policy.log.as_ref() {
            Some(log) => Logger::new(Some(&log.destination), log.format, label)?,
            None => None,
        };

        let mut runner = Self {
            identity,
            slots,
            decoders,
            renderer: Renderer::for_session(&policy, logger, label),
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
            self.renderer.switch_down_target(
                previous,
                target,
                self.slots.is_attached(previous),
                self.policy.down_channel,
            )?;
        } else {
            let active = self.routing.active_target();
            self.renderer
                .sync_down_target(previous_active, active, self.slots.attached_ids())?;
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
        let poll = self.slots.poll(Instant::now())?;
        if poll.had_data {
            self.process_polled_bytes()?;
        }
        if poll.had_events {
            self.handle_polled_events()?;
        }
        self.refresh_down_routes()?;
        if poll.had_events {
            self.renderer
                .flush_output()
                .context("Error writing to stdout")?;
        }
        if poll.had_data {
            self.renderer.flush_data()?;
        }
        if let Some(key_event) = self.wait_for_key(poll.had_data)? {
            if let Signal::Quit = self.handle_key(key_event)? {
                return Ok(Signal::Quit);
            }
        }
        self.flush_pending_input()?;
        Ok(Signal::Continue)
    }

    fn process_polled_bytes(&mut self) -> Result<()> {
        self.decoders
            .process_chunks(self.slots.polled_chunks(), &mut self.renderer)
    }

    fn handle_polled_events(&mut self) -> Result<()> {
        for index in 0..self.slots.polled_events().len() {
            let event = self.slots.polled_events()[index];
            self.handle_core_event(event)?;
        }
        Ok(())
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
                self.renderer.switch_down_target(
                    previous,
                    target,
                    self.slots.is_attached(previous),
                    self.policy.down_channel,
                )?;
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
                &self.identity,
                &self.policy,
                self.routing.active_target(),
            )?,
            SessionCommand::ClearScreen => self.renderer.clear_screen()?,
            SessionCommand::ToggleTimestamps => self.renderer.toggle_timestamps()?,
            SessionCommand::ResetTarget => {
                self.slots.chip_reset()?;
                self.decoders.reset_all();
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
            let write = self.slots.write_down(id, bytes)?;
            if let Some(event) = write.event {
                events.push(event);
            }
            Ok(write.count)
        })?;
        self.handle_core_events(&events)?;
        if !events.is_empty() {
            self.refresh_down_routes()?;
            self.renderer
                .flush_output()
                .context("Error writing to stdout")?;
        }
        Ok(())
    }

    fn handle_core_events(&mut self, events: &[CoreEvent]) -> Result<()> {
        for &event in events {
            self.handle_core_event(event)?;
        }
        Ok(())
    }

    fn handle_core_event(&mut self, event: CoreEvent) -> Result<()> {
        match event {
            CoreEvent::Attached { id, recovered } => {
                self.decoders.reset_core(id);
                self.renderer.reset_core_epoch(id)?;
                if recovered {
                    log::info!("core {id} available again");
                    self.renderer.notice_reattached(id)?;
                } else {
                    log::info!("core {id} attached");
                }
            }
            CoreEvent::AttachmentLost { id, reason } => {
                self.decoders.reset_core(id);
                self.renderer.reset_core_epoch(id)?;
                self.discard_pending(id, reason);
            }
        }
        Ok(())
    }
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
