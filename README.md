# brtt - Better RTT Client

`brtt` is a host program for interacting with RTT channels on a target device. It is primarily designed to be used as a terminal for the Zephyr shell, providing a seamless debugging and interaction experience.

## Usage

```
brtt [OPTIONS]
```

### Options

- `-p, --probe <PROBE>`: Specify the probe number. Use `list` to see all available probes. [default: 0]
- `-c, --chip <CHIP>`: Specify the target chip type (e.g., `nRF52840_xxAA`). If not provided, `brtt` will attempt to auto-detect it.
- `-l, --list`: List available RTT up and down channels on the target and exit.
- `-u, --up <CHANNEL[:MODE]>`: The RTT "up" channel (target to host) to use. `MODE` can be `ascii` and defaults to `ascii`. Defaults to channel 0 and may be repeated.
- `-d, --down <CHANNEL[:MODE]>`: The RTT "down" channel (host to target) for keyboard input. Only one channel is supported and it defaults to channel 0.
- `-r, --reset`: Reset the target after opening the RTT session.
- `--poll-interval <MILLISECONDS>`: Polling interval for RTT and keyboard input. [default: 10]
- `--scan-region <SCAN_REGION>`: Specify a memory region to scan for the RTT control block. Can be an exact address (e.g., `0x20000000`) or a range (e.g., `0x20000000..0x20010000`).

During a session, press `Ctrl-T` followed by a command key:

- `q`: Quit.
- `?`: Show command help.
- `c`: Show the current configuration.
- `l`: Clear the screen.
- `t`: Toggle timestamps.
- `e`: Toggle local echo.
- `R`: Reset the target.
- `Ctrl-T`: Send a literal `Ctrl-T` to the down channel.
