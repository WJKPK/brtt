use crate::cli::{expand_sources, Opts, SessionConfig};
use crate::defmt::ElfContents;
use crate::input::{InputAction, InteractiveInput, SessionCommand};
use crate::logger::Logger;
use crate::probe_handler::AttachedProbe;
use crate::renderer::Renderer;
use crate::target::{
    AttachOutcome, AttachResult, PollOutcome, PollStats, TargetIo, TargetSlot, RTT_RETRY_TIMEOUT,
    RTT_TIMEOUT,
};
use anyhow::{anyhow, bail, Context, Result};
use brtt::rtt::{RttDiscovery, ScanRegion};
use brtt::RttChannel;
use crossterm::event::KeyEvent;
use probe_rs::Session as ProbeSession;
use std::io::{stdout, BufWriter, Write};
use std::time::{Duration, Instant};

/// `--list`: attach to each configured core, find its RTT control block and
/// print the channels, then return. Retryable absences are reported on stderr
/// and skipped; permanent failures abort; at least one core must list.
pub(crate) fn list_channels(
    attached: AttachedProbe,
    opts: &Opts,
    elves: &[(u32, ElfContents)],
) -> Result<()> {
    let AttachedProbe { mut session, .. } = attached;
    let mut inputs: Vec<(u32, Option<ScanRegion>)> = if elves.is_empty() {
        vec![(0, None)]
    } else {
        elves
            .iter()
            .map(|(index, contents)| (*index, Some(contents.region.clone())))
            .collect()
    };
    inputs.sort_by_key(|(index, _)| *index);

    // A lone sparse core still names itself so the listing is unambiguous.
    let show_headers = inputs.len() > 1 || inputs.iter().any(|(index, _)| *index != 0);
    let mut listed = 0;
    let mut reasons = Vec::new();
    for (index, region) in inputs {
        let discovery = resolve_scan_region(
            region.as_ref(),
            opts.scan_region.as_ref(),
            &session.target().rtt_scan_regions,
        );
        match crate::target::attach_rtt_classified(&mut session, index, &discovery, RTT_TIMEOUT)? {
            AttachOutcome::Attached(mut rtt) => {
                if show_headers {
                    println!("Core {index}:");
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
        let detail = reasons.join("; ");
        bail!("no RTT targets could be listed ({detail}); ensure the cores are running and the ELF addresses match the firmware");
    }
    Ok(())
}

/// Runs the interactive session over every configured core: attach, find each
/// RTT control block, then read and render until the user quits. With no ELF
/// the single default target is core 0, exactly like the old single-target
/// flow. Per-core order follows the numeric index, never the CLI order.
pub(crate) fn run_multi(
    attached: AttachedProbe,
    opts: Opts,
    elves: Vec<(u32, ElfContents)>,
) -> Result<()> {
    let AttachedProbe {
        mut session,
        label,
        chip,
    } = attached;

    let mut inputs: Vec<(u32, Option<ScanRegion>, Option<crate::defmt::DefmtData>)> =
        if elves.is_empty() {
            vec![(0, None, None)]
        } else {
            elves
                .into_iter()
                .map(|(index, contents)| (index, Some(contents.region), contents.defmt))
                .collect()
        };
    inputs.sort_by_key(|(index, _, _)| *index);

    // One config per target; display policy identical, defmt table and scan
    // region per ELF.
    let mut configs: Vec<(u32, SessionConfig)> = Vec::with_capacity(inputs.len());
    for (index, region, defmt) in inputs {
        let discovery = resolve_scan_region(
            region.as_ref(),
            opts.scan_region.as_ref(),
            &session.target().rtt_scan_regions,
        );
        configs.push((
            index,
            SessionConfig::from_opts(&opts, label.clone(), chip.clone(), defmt, discovery)?,
        ));
    }

    // One persistent slot per relevant core. Cores with no applicable up
    // channel are still relevant while down input is enabled, since any of
    // them may expose the routable down channel; with `--no-down` they are
    // skipped without ever touching the probe.
    let mut slots: Vec<TargetSlot> = configs
        .iter()
        .filter(|(index, config)| TargetSlot::relevant(*index, config))
        .map(|(index, config)| TargetSlot::new(*index, config))
        .collect();
    if slots.is_empty() {
        bail!("no configured core selects any up channel and down input is disabled");
    }

    let (mut attached, mut pending) = attach_all(&mut session, &mut slots, RTT_TIMEOUT)?;
    if opts.reset {
        chip_reset(&mut session, &mut slots)?;
        let (reattached, still_pending) = attach_all(&mut session, &mut slots, RTT_TIMEOUT)?;
        if reattached == 0 {
            return Err(open_error(&still_pending));
        }
        attached = reattached;
        pending = still_pending;
    } else if attached == 0 {
        return Err(open_error(&pending));
    }
    if !pending.is_empty() {
        log::info!("continuing with {attached} core(s); pending cores retry in the background");
    }

    let output = BufWriter::new(stdout().lock());
    let mut runner = Session::new(session, slots, &configs[0].1, output)?;
    runner.run()
}

/// One attach attempt over every slot. Retryable absence (disabled core,
/// firmware not initialized yet) parks the slot as pending so healthy cores
/// still start; structural failures and selection mistakes stay hard errors,
/// exactly like the old single-target flow.
fn attach_all(
    session: &mut ProbeSession,
    slots: &mut [TargetSlot<'_>],
    timeout: Duration,
) -> Result<(usize, Vec<(u32, String)>)> {
    let mut attached = 0;
    let mut pending = Vec::new();
    for slot in slots.iter_mut() {
        match slot.try_attach(session, timeout) {
            Ok(AttachResult::Attached { .. }) => attached += 1,
            Ok(AttachResult::Retryable(reason)) => {
                log::warn!("{reason}; continuing without it");
                slot.defer_retry();
                pending.push((slot.index(), reason));
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("core {} failed to attach", slot.index()));
            }
        }
    }
    Ok((attached, pending))
}

/// Aggregate startup error preserving each core's original reason instead of
/// a generic message, so single-core diagnostics match the old flow.
fn open_error(pending: &[(u32, String)]) -> anyhow::Error {
    let detail = pending
        .iter()
        .map(|(_, reason)| reason.clone())
        .collect::<Vec<_>>()
        .join("; ");
    anyhow!("no RTT targets could be opened ({detail}); ensure the cores are running and the ELF addresses match the firmware")
}

/// Issues one chip-wide reset via the lowest attached core, then parks every
/// slot as pending so each core reattaches independently at its own pace.
fn chip_reset(session: &mut ProbeSession, slots: &mut [TargetSlot<'_>]) -> Result<()> {
    let initiator = slots
        .iter()
        .filter(|slot| slot.is_attached())
        .map(TargetSlot::index)
        .min();
    let Some(index) = initiator else {
        bail!("cannot reset target: no configured core is currently accessible");
    };
    let ptr = slots
        .iter()
        .find(|slot| slot.index() == index)
        .and_then(|slot| slot.attached().map(|target| target.rtt_ptr()))
        .expect("initiator is attached");
    let mut core = session.core(index as usize)?;
    TargetIo::reset_device(&mut core, ptr)?;
    for slot in slots.iter_mut() {
        slot.mark_pending();
    }
    log::info!("target reset issued via core {index}; reattaching cores");
    Ok(())
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

struct Session<'config, W: Write> {
    session: ProbeSession,
    slots: Vec<TargetSlot<'config>>,
    /// Shared display/input policy. All configs derive from the same options
    /// and differ only in per-ELF defmt table and scan region.
    policy: &'config SessionConfig,
    renderer: Renderer<W>,
    input: Option<InteractiveInput>,
}

impl<'config, W: Write> Session<'config, W> {
    fn new(
        session: ProbeSession,
        slots: Vec<TargetSlot<'config>>,
        policy: &'config SessionConfig,
        output: W,
    ) -> Result<Self> {
        debug_assert!(!slots.is_empty());
        // Tag policy derives from the relevant core set, not the currently
        // attached subset, so labels stay stable when a late core joins.
        let indices: Vec<u32> = slots.iter().map(TargetSlot::index).collect();
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
        let show_cores = slots.len() > 1;
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
            policy,
            renderer: Renderer::for_session(output, logger, policy, include_channel, show_cores),
            input: None,
        };
        runner.refresh_down_routes()?;
        runner.renderer.show_banner()?;
        Ok(runner)
    }

    /// Current routable cores: attached slots exposing the down channel.
    fn routable_cores(&self) -> Vec<u32> {
        let mut routable: Vec<u32> = self
            .slots
            .iter()
            .filter_map(|slot| {
                slot.attached()
                    .filter(|target| target.down_present())
                    .map(|target| target.index())
            })
            .collect();
        routable.sort_unstable();
        routable
    }

    /// Whether every slot reached a terminal state (attached), i.e. no core
    /// is still pending its first or recovered attach.
    fn no_pending_slots(&self) -> bool {
        self.slots.iter().all(|slot| slot.is_attached())
    }

    /// Rebuilds keyboard routing from the attached set. Creates input when
    /// the first routable core appears, drops it when none remains and no
    /// core can still recover, and errors only when an explicit `--down`
    /// provably exists nowhere.
    fn refresh_down_routes(&mut self) -> Result<()> {
        let routable = self.routable_cores();
        if routable.is_empty() {
            if self.policy.down_explicit && self.no_pending_slots() {
                if let Some(down) = self.policy.down_channel {
                    bail!("down channel {down} does not exist on any configured core");
                }
            }
            if self.input.is_some() && self.no_pending_slots() {
                log::info!("down channel unavailable on all cores; disabling keyboard input");
                self.input = None;
            }
            return Ok(());
        }
        match self.input.as_mut() {
            None => {
                self.input = InteractiveInput::new(self.policy.down_channel, &routable)?;
            }
            Some(input) => {
                let previous = input.down_target();
                for (core, dropped) in input.set_routable(&routable) {
                    log::warn!(
                        "core {core} lost its down channel; dropping {dropped} queued byte(s)"
                    );
                }
                if input.down_target() != previous {
                    log::warn!(
                        "keyboard input moved to core {} (previous target lost its down channel)",
                        input.down_target()
                    );
                }
            }
        }
        if self.policy.down_explicit {
            let down = self.policy.down_channel.expect("routing implies a channel");
            for slot in self.slots.iter() {
                let missing = slot.attached().is_some_and(|target| !target.down_present());
                if missing {
                    log::warn!(
                        "core {} has no down channel {down}; keyboard input unavailable there",
                        slot.index()
                    );
                }
            }
        }
        Ok(())
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
        // Per-core order is numeric and stable; across cores the order is
        // poll order, never temporal — host timestamps correlate instead.
        // Each core handle is acquired and dropped within its iteration:
        // probe-rs never lends two cores at once.
        let now = Instant::now();
        // Phase 1: reattach due pending slots without stalling healthy cores.
        for slot in self.slots.iter_mut() {
            if slot.is_attached() || !slot.due(now) {
                continue;
            }
            match slot.try_attach(&mut self.session, RTT_RETRY_TIMEOUT)? {
                AttachResult::Attached { recovered } => {
                    let index = slot.index();
                    // Fresh decoders need no reset, but the renderer/logger
                    // may hold stale partials from before the loss.
                    self.renderer.reset_core_epoch(index)?;
                    if recovered {
                        log::info!("core {index} available again");
                        self.renderer.notice_reattached(index)?;
                    } else {
                        log::info!("core {index} attached");
                    }
                }
                AttachResult::Retryable(reason) => {
                    log::debug!("{reason}; retrying");
                    slot.defer_retry();
                }
            }
        }
        // Phase 2: poll attached slots; a lost core or moved block parks the
        // slot as pending instead of stopping the remaining cores.
        let mut stats = PollStats::default();
        for position in 0..self.slots.len() {
            if !self.slots[position].is_attached() {
                continue;
            }
            let index = self.slots[position].index();
            let mut core = match self.session.core(index as usize) {
                Ok(core) => core,
                Err(error) => {
                    log::warn!(
                        "core {index} unavailable ({error:#}); continuing with remaining cores"
                    );
                    self.slots[position].mark_pending();
                    continue;
                }
            };
            let Some(target) = self.slots[position].attached_mut() else {
                continue;
            };
            match target.poll(&mut core, &mut self.renderer)? {
                PollOutcome::Data(datum) => {
                    stats.bytes += datum.bytes;
                    stats.messages += datum.messages;
                }
                PollOutcome::Reattach => {
                    log::info!("core {index} RTT block changed; reattaching");
                    self.renderer.reset_core_epoch(index)?;
                    self.slots[position].mark_pending();
                }
            }
        }
        // Phase 3: routing follows the attached set; late cores enable input.
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
        // Do not delay the next RTT read when data was available. When idle,
        // this wait also provides the keyboard poll and CPU backoff.
        let timeout = if !had_data {
            self.policy.poll_interval
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
            SessionCommand::ShowConfig => {
                let cores: Vec<u32> = self.slots.iter().map(TargetSlot::index).collect();
                let down_target = self.input.as_ref().map(|input| input.down_target());
                self.renderer
                    .show_config(&cores, self.policy, down_target)?
            }
            SessionCommand::ClearScreen => self.renderer.clear_screen()?,
            SessionCommand::ToggleTimestamps => self.renderer.toggle_timestamps()?,
            SessionCommand::ResetTarget => {
                chip_reset(&mut self.session, &mut self.slots)?;
                self.renderer.reset_target_epoch()?;
                if let Some(input) = self.input.as_mut() {
                    input.clear_all();
                }
                self.refresh_down_routes()?;
                self.renderer.notice_target_reset()?;
            }
            SessionCommand::CycleDownCore => {
                if let Some(input) = self.input.as_mut() {
                    let core = input.cycle_down_target();
                    let channel = self
                        .policy
                        .down_channel
                        .expect("keyboard input implies a down channel");
                    self.renderer.notice_down_target(core, channel)?;
                }
            }
        }

        if restore_foreground {
            if let Some(saved) = suspended {
                self.renderer.restore_foreground(saved)?;
            }
        }
        Ok(false)
    }

    fn flush_pending_input(&mut self) -> Result<()> {
        let Some(interactive) = self.input.as_mut() else {
            return Ok(());
        };
        if !interactive.has_pending() {
            return Ok(());
        }
        // Each route flushes to its own core; an unavailable core keeps its
        // bytes for retry without blocking other cores.
        for core in interactive.routable() {
            let mut handle = match self.session.core(core as usize) {
                Ok(handle) => handle,
                Err(_) => continue,
            };
            let Some(target) = self
                .slots
                .iter_mut()
                .find(|slot| slot.index() == core)
                .and_then(|slot| slot.attached_mut())
            else {
                continue;
            };
            interactive.flush_route(core, |bytes| target.write_down(&mut handle, bytes))?;
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

#[cfg(test)]
#[path = "../tests/session.rs"]
mod tests;
