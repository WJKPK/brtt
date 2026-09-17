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
use std::io::{IsTerminal, Write};

fn write_diagnostic_line(
    output: &mut impl Write,
    level: log::Level,
    args: std::fmt::Arguments<'_>,
    interactive: bool,
) -> std::io::Result<()> {
    if interactive {
        write!(output, "\r\x1b[2K\x1b[0m[brtt {level}] {args}\x1b[0m\r\n")
    } else {
        writeln!(output, "[brtt {level}] {args}")
    }
}

fn main() -> Result<()> {
    // Diagnostic rule: the tool's own status goes through log:: (stderr,
    // gated by RUST_LOG, default brtt=info); target data goes through the
    // renderer (stdout / log file). Interactive diagnostics clear the
    // renderer's current foreground line before writing at column zero.
    // Never eprintln! status: it cannot be silenced.
    let interactive_stderr = std::io::stdout().is_terminal() && std::io::stderr().is_terminal();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("brtt=info"))
        .format(move |buffer, record| {
            write_diagnostic_line(buffer, record.level(), *record.args(), interactive_stderr)
        })
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

#[cfg(test)]
#[path = "../tests/main.rs"]
mod tests;
