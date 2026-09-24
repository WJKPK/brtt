# brtt - Better RTT Client

`brtt` is a host program for interacting with RTT channels on a target device. It is primarily
designed to be used as a terminal for the Zephyr shell, providing a seamless debugging and
interaction experience.

## Usage

```
brtt [OPTIONS]
```

Run `brtt --help` for the full option list.

`brtt` supports multiple up channels at once, interactive terminal input/output over a down
channel, host-side timestamps with millisecond precision, and logging to file with an optional
per-channel split. The interactive session uses a `tio`-like interface with a `Ctrl-T` command
prefix.

### Example: nRF54L15 with two up channels

This monitors the terminal on channel 0 and defmt output on channel 1:

```sh
brtt \
  --chip nRF54L15 \
  --up 0:terminal \
  --up 1:defmt \
  --elf "path/to/elf"
```

### Example: dual-core STM32H745 with one ELF per core

Each `--elf` attaches one core (`INDEX=PATH`, bare paths fill the lowest free
index). Channels are selected per core with `--up CORE:CHANNEL[:MODE]`; merged
output tags name the source (`[c0:ch0]` / `[c1:ch0]`). Cross-core order is poll
order, never target time — host timestamps correlate instead:

```sh
brtt \
  --chip STM32H745ZITx \
  --elf "0=m7.elf" \
  --elf "1=m4.elf" \
  --up 0:0:terminal \
  --up 1:0:defmt
```

A bare `--up 0` selects channel 0 on every configured core. A core that starts
later joins in the background; `--no-down` keeps otherwise unused cores from
being inspected solely for down-channel routing.

An ELF without `_SEGGER_RTT` can still provide a defmt table: supply `--scan-region`
to locate RTT explicitly, or let the target's RAM regions be scanned.

If a core loses its RTT attachment, pending keyboard bytes for that core are
discarded rather than replayed after reattachment. Input typed while no down
channel is available is also discarded; `Ctrl-T` commands remain available.

## Nix

The flake provides a reproducible source build for the supported Unix systems.

Install the latest package directly:

```sh
nix profile install github:michal4132/brtt
```

Use it from another flake without writing a derivation:

```nix
inputs.brtt.url = "github:michal4132/brtt";

# Use inputs.brtt.packages.${system}.default in a package list.
```

The flake also exports `overlays.default` for users who prefer `pkgs.brtt` after adding the overlay
to their nixpkgs import.

## Interactive commands

During a session, press `Ctrl-T` followed by a command key:

- `q`: Quit.
- `?`: Show command help.
- `c`: Show the current configuration.
- `l`: Clear the screen.
- `t`: Toggle timestamps.
- `R`: Reset the target.
- `d`: Switch keyboard input to the next core exposing the down channel.
- `Ctrl-T`: Send a literal `Ctrl-T` to the down channel.

## Why vt100?

Terminal channels are screen-oriented: the shell repaints the current line instead of sending
finished lines. `brtt` runs those bytes through a stateful ANSI/UTF-8 terminal model (`vt100`)
and derives both the live view and the decoded log from that single decode, so display and log
never disagree. That model keeps labels, timestamps, multi-channel output, and `Ctrl-T` commands
on logical line boundaries and the host terminal clean across errors, resets, and reattaches.

`defmt` frames are already logical messages, so they render directly; `--log-format raw` stores
exact RTT bytes. See `brtt --help` for the operational summary.
