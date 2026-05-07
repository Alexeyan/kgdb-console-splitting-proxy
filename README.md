# kgdb-console-splitting-proxy

A small TCP / UDP / serial proxy for kernel and embedded debug workflows —
originally inspired by the C implementation agent-proxy.
Additional features include fixing a few bugs, including a spin loop, connection recovery on suspend/sleep cycles, handling multiple clients at once and a better UI. Written using Claude Code, but also manualy tested and in daily use. 

`kgdb-console-splitting-proxy` sits between a debugger (`gdb`, `kgdb`, ...) and a
serial console or remote terminal server. Its main job is to let one physical
serial line carry both **serial console traffic** and a **kgdb session**
at the same time, without one stepping on the other.

## What it does

- **TCP / UDP / serial proxy.** Forward bytes between a local socket (or
  `stdin`) and a remote endpoint reached over TCP, UDP or a Unix serial
  device.
- **Console / GDB splitter.** Listen on two TCP ports for the same physical
  link. One port shows the console; the other carries GDB remote-protocol
  packets. The splitter recognises `$…#xx` packets and routes them to the
  correct client.
- **Break on connect.** When a client attaches to the GDB port, send a break
  sequence (or a configurable byte) so the kernel drops into `kgdb`.
- **Script port.** Open an extra TCP port that scripts can use to drive the
  remote without disturbing the interactive console.

## Building

Requires a recent Rust toolchain (edition 2024). The crate is Unix-only —
`/dev/ttyUSB0`-style paths are supported, Windows COM ports are not.

```sh
cd kgdb-console-splitting-proxy
cargo build --release
```

The binary lands at `target/release/kgdb-console-splitting-proxy`. Run `cargo test` to execute
the integration tests under `tests/` (TCP proxy and PTY-based serial
splitter); `cargo fmt -- --check` enforces formatting.

## Quick start: kgdb over a local serial port

```sh
# 1. Start the splitter against a local serial port at 115200 baud.
#    `0` means "no remote host" — the remote endpoint is a serial device.
sudo ./kgdb-console-splitting-proxy serial-split
╭─ kgdb-console-splitting-proxy v0.0.1 ────────────────────────────╮
│ ● Mode       serial-split                                        │
│ ⌁ Serial     /dev/ttyUSB0 @ 115200 baud  fd=5       0 B    0 B   │
│ ▣ Console    tcp://localhost:4440  0 connected clients          │
│ ◆ KGDB       tcp://localhost:4441  0 connected clients          │
╰──────────────────────────────────────────────────────────────────╯
# 2. Attach to the console.
nc localhost 4440

# 3. Boot the target with kernel argument:
#       kgdboc=ttyS0,115200

# 4. Attach gdb to the debug port.
gdb ./vmlinux
(gdb) target remote localhost:4441
```

## Command-line interface

The tool accepts three argument styles: a `proxy` subcommand, a
`serial-split` subcommand, and the legacy positional form.

```text
kgdb-console-splitting-proxy proxy [options] --local <endpoint> --remote <endpoint>
kgdb-console-splitting-proxy serial-split [options]
kgdb-console-splitting-proxy [options] <local> <remote-host> <remote>      # legacy form
```

Run `kgdb-console-splitting-proxy --help` for the embedded reference.

### `proxy` endpoints

Local endpoint:

| Form                     | Meaning                                           |
| ------------------------ | ------------------------------------------------- |
| `<port>`                 | Listen on TCP `<port>` on localhost.              |
| `<bind-ip>:<port>`       | Listen on TCP at a specific local address.        |
| `udp:<port>`             | Bind a local UDP port.                            |
| `udp:<bind-ip>:<port>`   | Bind UDP on a specific local address.             |
| `stdin`                  | Use stdin/stdout instead of a local socket.       |
| `<local>+<script-port>`  | Add a TCP script/control listener alongside it.   |
| `<local>^<kgdb-port>`    | Split console traffic from GDB remote packets.    |

Remote endpoint:

| Form                              | Meaning                                          |
| --------------------------------- | ------------------------------------------------ |
| `tcp:<host>:<port>`               | Connect to a remote TCP port.                    |
| `udp:<host>:<port>`               | Connect to a remote UDP port.                    |
| `udp:<host>:<src-port>:<port>`    | Bind a local UDP source port first.              |
| `serial:<path>[,<baud>]`          | Open a Unix serial device (e.g. `/dev/ttyS0`).   |
| `tcplisten:<bind-ip>:<port>`      | Accept the connection from the remote side.      |
| `fifocon:<path>`                  | Accept console-selected TCP ports via FIFO.      |

### Options

| Flag                              | Description                                              |
| --------------------------------- | -------------------------------------------------------- |
| `-h`, `--help`                    | Show help.                                               |
| `--version`                       | Print version.                                           |
| `-v`, `--verbose`                 | Verbose connection logging.                              |
| `-d`, `--log-chars`               | Log every proxied byte (noisy; useful for debugging).    |
| `-D`, `--daemon`                  | Fork into the background.                                |
| `-f`, `--pid-file <file>`         | Write the proxy PID to a file.                           |
| `-B`, `--no-break-on-connect`     | Don't send a break when a kgdb client connects.          |
| `-G`, `--no-gdb-filter`           | Don't filter GDB packets out of the console stream.      |
| `-s`, `--break-byte <byte>`       | Use this byte instead of the default break sequence.     |
| `--telnet`                        | Speak Telnet negotiation on serial-console clients.      |
| `--remote-host <host>`            | Legacy companion to `--remote <port>`.                   |

### `serial-split` subcommand

A focused front-end for the most common case — splitting one serial device
into two TCP ports. Sensible defaults match the kgdb examples above.

| Flag                       | Default          | Description                  |
| -------------------------- | ---------------- | ---------------------------- |
| `--device <path>`          | `/dev/ttyUSB0`   | Serial device path.          |
| `--baud <rate>`            | `115200`         | Serial baud rate.            |
| `--console-port <port>`    | `4440`           | Console / log TCP port.      |
| `--gdb-port <port>`        | `4441`           | kgdb / GDB TCP port.         |

```sh
sudo kgdb-console-splitting-proxy serial-split                                   # all defaults
sudo kgdb-console-splitting-proxy serial-split --device /dev/ttyUSB0 --baud 115200 \
                              --console-port 4440 --gdb-port 4441

ncat localhost 4440     # console / log output
ncat localhost 4441     # kgdb / GDB traffic
```


## FAQ

**Do I need root?**
You need read/write access to the serial device. On most Linux distros that
means `sudo` or membership in the `dialout` / `uucp` group.

**Does it work on Windows?**
No, only Unix-like hosts and Unix serial paths; Windows COM ports are not implemented.

## Project layout

- `src/main.rs` — argument parsing, socket setup, the main `select` loop, and
  the splitter / GDB-packet logic.
- `src/port.rs` — port and connection state shared between the loop and
  protocol helpers.
- `src/rs232.rs` — serial-device configuration (termios, baud rate).
- `tests/` — integration tests covering the CLI, the TCP proxy path, and the
  PTY-backed serial splitter.

## License
GPLv3