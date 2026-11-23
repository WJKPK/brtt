mod channel;
mod cli;
mod defmt;
mod elf;
mod probe;
mod rtt;
mod session;

use anyhow::Result;
use clap::Parser;
use cli::{Opts, ResolvedCommand};
use probe::ProbeCatalog;
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
    let command = Opts::parse().resolve()?;

    match command {
        ResolvedCommand::ListProbes => {
            probe::ProbeCatalog::discover().write_to(std::io::stdout())?;
            Ok(())
        }
        ResolvedCommand::DebugDefmtTable(elf_specs) => {
            for elf in elf_specs {
                println!("Core {}: {}", elf.core, elf.path.display());
                defmt::DefmtData::debug_from_elf(&elf.path)?;
            }
            Ok(())
        }
        ResolvedCommand::ListChannels(target) => {
            let selected = target.probe.select(ProbeCatalog::discover())?;
            let rtt_symbols = target.load_rtt_symbols()?;
            let attached = selected.attach(target.chip)?;
            session::list_channels(attached, target.scan_region.as_ref(), rtt_symbols)
        }
        ResolvedCommand::Session {
            target,
            policy,
            reset,
        } => {
            let selected = target.probe.select(ProbeCatalog::discover())?;
            let elves = target.load_session_elves(&policy)?;
            let attached = selected.attach(target.chip)?;
            session::run_multi(attached, policy, target.scan_region.as_ref(), reset, elves)
        }
    }
}
