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

use anyhow::{Context, Result};
use clap::Parser;
use cli::{Opts, ProbeInfo};
use std::io::Write;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("brtt=warn"))
        .format(|buffer, record| writeln!(buffer, "[brtt {}] {}", record.level(), record.args()))
        .init();
    let opts = Opts::parse();

    let up_specs = cli::configured_up_specs(&opts.up);
    opts.validate(&up_specs)?;
    let elf_bytes = opts.elf.as_deref().map(defmt::read_elf).transpose()?;
    let defmt_data = if opts.needs_defmt_data(&up_specs) {
        let path = opts
            .elf
            .as_deref()
            .context("--elf is required when using an up channel with :defmt")?;
        let bytes = elf_bytes
            .as_deref()
            .context("failed to read the ELF provided via --elf")?;
        Some(defmt::DefmtData::load(path, bytes)?)
    } else {
        None
    };
    if opts.debug_defmt_table {
        let data = defmt_data.as_ref().ok_or_else(|| {
            anyhow::anyhow!("--debug-defmt-table requires --elf with a defmt table")
        })?;
        data.debug_summary(&mut std::io::stdout())?;
        return Ok(());
    }

    let elf_region = match (opts.elf.as_deref(), elf_bytes.as_deref()) {
        (Some(path), Some(bytes)) => Some(defmt::rtt_region_from_elf(path, bytes)?),
        _ => None,
    };
    drop(elf_bytes);

    let probes = probe_handler::list();

    if matches!(opts.probe, Some(ProbeInfo::List)) {
        probe_handler::list_probes(std::io::stdout(), &probes);
        return Ok(());
    }

    let attached = probe_handler::attach(&probes, opts.probe.as_ref(), opts.chip.as_deref())?;

    session::start(attached, opts, defmt_data, elf_region)
}
