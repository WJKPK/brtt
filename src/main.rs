mod channel;
mod cli;
mod defmt;
mod input;
mod logger;
mod probe_handler;
mod renderer;
mod session;
mod target;
mod terminal;

use anyhow::Result;
use clap::Parser;
use cli::{ChannelSpec, Mode, Opts};
use std::io::Write;

fn main() -> Result<()> {
    // Diagnostic rule: the tool's own status goes through log:: (stderr,
    // gated by RUST_LOG, default brtt=info); target data goes through the
    // renderer (stdout / log file). Never eprintln! status: it cannot be
    // silenced.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("brtt=info"))
        .format(|buffer, record| writeln!(buffer, "[brtt {}] {}", record.level(), record.args()))
        .init();
    let opts = Opts::parse();
    let up_specs = cli::configured_up_specs(&opts.up);
    opts.validate(&up_specs)?;

    match opts.mode() {
        Mode::ListProbes => {
            let probes = probe_handler::list();
            probe_handler::list_probes(std::io::stdout(), &probes);
            Ok(())
        }
        Mode::DebugDefmtTable => {
            let path = opts
                .elf
                .as_deref()
                .expect("clap requires --elf with --debug-defmt-table");
            defmt::DefmtData::from_elf(path)?.debug_summary(&mut std::io::stdout())?;
            Ok(())
        }
        Mode::ListChannels => {
            let elf = load_elf(&opts, &up_specs)?;
            let attached = attach_probe(&opts)?;
            session::list_channels(attached, &opts, elf.as_ref().map(|elf| &elf.region))
        }
        Mode::Session => {
            let elf = load_elf(&opts, &up_specs)?;
            let attached = attach_probe(&opts)?;
            let (defmt, region) = match elf {
                Some(elf) => (elf.defmt, Some(elf.region)),
                None => (None, None),
            };
            session::run(attached, opts, defmt, region)
        }
    }
}

/// Reads `--elf` once for target modes, deriving the defmt table only when
/// the selected up channels need it.
fn load_elf(opts: &Opts, up_specs: &[ChannelSpec]) -> Result<Option<defmt::ElfContents>> {
    let Some(path) = opts.elf.as_deref() else {
        return Ok(None);
    };
    Ok(Some(defmt::ElfContents::load(
        path,
        opts.needs_defmt_data(up_specs),
    )?))
}

fn attach_probe(opts: &Opts) -> Result<probe_handler::AttachedProbe> {
    let probes = probe_handler::list();
    probe_handler::attach(&probes, opts.probe.as_ref(), opts.chip.as_deref())
}
