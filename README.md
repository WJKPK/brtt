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
- `Ctrl-T`: Send a literal `Ctrl-T` to the down channel.

## Why vt100?

Terminal channels are screen-oriented: the shell repaints the current line instead of sending
finished lines. `brtt` runs those bytes through a stateful ANSI/UTF-8 terminal model (`vt100`)
and derives both the live view and the decoded log from that single decode, so display and log
never disagree. That model keeps labels, timestamps, multi-channel output, and `Ctrl-T` commands
on logical line boundaries and the host terminal clean across errors, resets, and reattaches.

`defmt` frames are already logical messages, so they render directly; `--log-format raw` stores
exact RTT bytes. See `brtt --help` for the operational summary.
