use super::channel_decode::{ChannelDecoders, UpSink};
use super::core_io::{
    attach_rtt_classified, resolve_rtt_discoveries, AttachOutcome, AttachmentLoss, CoreEvent,
    CoreSlots, Startup, RTT_TIMEOUT,
};
use super::logger::Logger;
use super::renderer::{format_defmt_frame, Renderer};
use super::session_input::{KeyOutcome, RoutingEvent, SessionCommand, SessionInput};
use super::terminal_decode::TerminalChunk;
use super::SessionPolicy;
use crate::channel::{expand_sources, CoreChannel, CoreId, SourceLabel};
use crate::defmt::DecodedFrame;
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
    slots: CoreSlots,
    decoders: ChannelDecoders<'setup>,
    policy: SessionPolicy,
    renderer: Renderer,
    logger: Option<Logger>,
    routing: SessionInput,
}

impl<'setup> Session<'setup> {
    fn new(
        identity: ProbeIdentity,
        slots: CoreSlots,
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
            renderer: Renderer::for_session(&policy, label),
            logger,
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
        let cleanup = self.finish_session();
        digest_result(primary, cleanup)
    }

    fn finish_session(&mut self) -> Result<()> {
        self.renderer.finish_session()?;
        if let Some(logger) = &mut self.logger {
            logger.flush()?;
        }
        Ok(())
    }

    fn tick(&mut self) -> Result<Signal> {
        let poll = self.slots.poll(Instant::now())?;
        if poll.had_data {
            self.process_polled_bytes()?;
        }
        if poll.had_events {
            self.handle_polled_events()?;
            self.refresh_down_routes()?;
        }
        if poll.had_data {
            if let Some(logger) = &mut self.logger {
                logger.flush_files()?;
            }
        }
        if poll.had_data || poll.had_events {
            self.renderer
                .flush_output()
                .context("Error writing to stdout")?;
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
        let mut sink = SessionSink {
            renderer: &mut self.renderer,
            logger: self.logger.as_mut(),
        };
        self.decoders
            .process_chunks(self.slots.polled_chunks(), &mut sink)
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
            KeyOutcome::Command(command) => {
                let signal = self.dispatch_command(command)?;
                if matches!(signal, Signal::Continue) {
                    self.renderer
                        .flush_output()
                        .context("Error writing to stdout")?;
                }
                Ok(signal)
            }
            KeyOutcome::QueuedInput | KeyOutcome::Ignored => Ok(Signal::Continue),
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
                self.reset_target_epoch()?;
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
        for pending in self.routing.pending_writes() {
            let write = self.slots.write_down(pending.core(), pending.bytes())?;
            pending.consume(write.count);
            if let Some(event) = write.event {
                events.push(event);
            }
        }
        if !events.is_empty() {
            self.handle_core_events(&events)?;
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
                self.reset_core_epoch(id)?;
                if recovered {
                    log::info!("core {id} available again");
                    self.renderer.notice_reattached(id)?;
                } else {
                    log::info!("core {id} attached");
                }
            }
            CoreEvent::AttachmentLost { id, reason } => {
                self.reset_core_epoch(id)?;
                self.discard_pending(id, reason);
            }
        }
        Ok(())
    }

    fn reset_core_epoch(&mut self, core: CoreId) -> Result<()> {
        self.decoders.reset_core(core);
        self.renderer.reset_core_epoch(core)?;
        if let Some(logger) = &mut self.logger {
            logger.reset_core(core)?;
        }
        Ok(())
    }

    fn reset_target_epoch(&mut self) -> Result<()> {
        self.decoders.reset_all();
        self.renderer.reset_target_epoch()?;
        if let Some(logger) = &mut self.logger {
            logger.reset()?;
        }
        Ok(())
    }
}

struct SessionSink<'a> {
    renderer: &'a mut Renderer,
    logger: Option<&'a mut Logger>,
}

impl UpSink for SessionSink<'_> {
    fn raw_bytes(&mut self, source: CoreChannel, bytes: &[u8]) -> Result<()> {
        if let Some(logger) = self.logger.as_deref_mut() {
            logger.write_bytes(source, bytes)?;
        }
        Ok(())
    }

    fn terminal(&mut self, source: CoreChannel, chunk: TerminalChunk, at: Instant) -> Result<()> {
        if let Some(logger) = self.logger.as_deref_mut() {
            logger.write_terminal_decoded(
                source,
                chunk.lines.iter().map(|line| line.plain.as_slice()),
                &chunk.partial.plain,
            )?;
        }
        self.renderer
            .render_terminal_chunk(source, chunk, at)
            .context("Error rendering terminal output")
    }

    fn defmt_frame(
        &mut self,
        source: CoreChannel,
        frame: &DecodedFrame,
        at: Instant,
    ) -> Result<()> {
        let line = format_defmt_frame(frame);
        if let Some(logger) = self.logger.as_deref_mut() {
            logger.write_defmt_decoded(source, line.as_bytes())?;
        }
        self.renderer
            .render_defmt_line(source, line.as_bytes(), frame.level, at)
            .context("Error rendering defmt frame")
    }

    fn defmt_warning(&mut self, source: CoreChannel, warning: &str, at: Instant) -> Result<()> {
        let line = self.renderer.format_defmt_warning(warning);
        if let Some(logger) = self.logger.as_deref_mut() {
            logger.write_defmt_decoded(source, line.as_bytes())?;
        }
        self.renderer
            .render_defmt_line(source, line.as_bytes(), None, at)
            .context("Error rendering defmt warning")
    }

    fn needs_styled_terminal_output(&self) -> bool {
        self.renderer.is_interactive()
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
