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

use anyhow::{bail, Result};
use clap::Parser;
use cli::{ChannelEncoding, ChannelSpec, Mode, Opts};
use std::io::Write;
use std::path::PathBuf;

fn main() -> Result<()> {
    // Diagnostic rule: the tool's own status goes through log:: (stderr,
    // gated by RUST_LOG, default brtt=info); target data goes through the
    // renderer (stdout / log file). Never eprintln! status: it cannot be
    // silenced.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("brtt=info"))
        .format(|buffer, record| writeln!(buffer, "[brtt {}] {}", record.level(), record.args()))
        .init();
    let opts = Opts::parse();
    let resolved = opts.resolve()?;

    match opts.mode() {
        Mode::ListProbes => {
            let probes = probe_handler::list();
            probe_handler::list_probes(std::io::stdout(), &probes);
            Ok(())
        }
        Mode::DebugDefmtTable => {
            if resolved.elf_specs().is_empty() {
                bail!("--debug-defmt-table requires --elf");
            }
            for (index, path) in resolved.elf_specs() {
                println!("Core {index}: {}", path.display());
                defmt::DefmtData::from_elf(path)?.debug_summary(&mut std::io::stdout())?;
            }
            Ok(())
        }
        Mode::ListChannels => {
            let elves = load_all_elfs(resolved.elf_specs(), resolved.up_specs())?;
            let attached = attach_probe(&opts)?;
            session::list_channels(attached, &opts, elves)
        }
        Mode::Session => {
            let elves = load_all_elfs(resolved.elf_specs(), resolved.up_specs())?;
            let attached = attach_probe(&opts)?;
            session::run_multi(attached, opts, elves, resolved.up_specs())
        }
    }
}

/// Reads every `--elf` once, deriving each defmt table only when a defmt
/// channel selects that core. A terminal-only core whose ELF lacks `.defmt`
/// must not fail startup.
fn load_all_elfs(
    specs: &[(u32, PathBuf)],
    up_specs: &[ChannelSpec],
) -> Result<Vec<(u32, defmt::ElfContents)>> {
    specs
        .iter()
        .map(|(index, path)| {
            let needs_defmt = up_specs
                .iter()
                .any(|spec| spec.mode == ChannelEncoding::Defmt && spec.applies_to(*index));
            defmt::ElfContents::load(path, needs_defmt).map(|contents| (*index, contents))
        })
        .collect()
}

fn attach_probe(opts: &Opts) -> Result<probe_handler::AttachedProbe> {
    let probes = probe_handler::list();
    probe_handler::attach(&probes, opts.probe.as_ref(), opts.chip.as_deref())
}
