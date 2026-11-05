# russhpit

russhpit is a Rust port of [Endlessh](https://github.com/skeeto/endlessh), the SSH tarpit by Chris Wellons. It preserves the design and core behavior of the original C program.
It also defines behavior for partial writes, reload failures, config errors, and other cases that the C program leaves undefined.

russhpit sends an endless stream of slow, random lines before the SSH identification string. RFC 4253 permits a server to send these lines. An SSH client waits for the identification string and remains connected.

Read [the original Endlessh article](https://nullprogram.com/blog/2019/03/22/) for an explanation of the design and its resource costs.

## Build

russhpit supports Unix-like systems.

```console
cargo build --release
./target/release/russhpit -h
```

The default port is `2222`.

```console
# start the server
./target/release/russhpit -v
```

Test the server from another terminal:
```console
ssh -v -p 2222 localhost
```


The SSH client waits while russhpit sends non-identification lines.
Use the client's interrupt key to close the test connection.

Many systems reserve port `22` for privileged processes.
Use the system's service or packet-filter configuration if russhpit must listen on that port.

## Usage

```text
Usage: russhpit [-46hsvV] [-d MS] [-f CONFIG] [-l LEN] [-m LIMIT] [-p PORT]
  -4        Bind to IPv4 only
  -6        Bind to IPv6 only
  -d INT    Message millisecond delay [10000]
  -f        Set and load config file [/etc/russhpit/config]
  -h        Print this help message and exit
  -l INT    Maximum banner line length (3-255) [32]
  -m INT    Maximum number of clients [4096]
  -p INT    Listening port [2222]
  -s        Print diagnostics to syslog instead of standard output
  -v        Print diagnostics (repeatable)
  -V        Print version information and exit
```

FreeBSD uses `/usr/local/etc/russhpit.config` as the default config path.
Other supported systems use `/etc/russhpit/config`.

Arguments apply from left to right.
The `-f` option loads its file at its position in the argument list.
A later option can replace a value from that file.

One `-v` enables connection records.
A second `-v` enables debug records.
The `-s` option sends these records to syslog.

## Configuration

The config format and keys match Endlessh:

```text
# Listen for SSH connections on this port.
Port 2222

# Wait this many milliseconds between banner lines.
Delay 10000

# Set the maximum banner line length, including CRLF.
MaxLineLength 32

# Accept no more than this number of clients at one time.
MaxClients 4096

# Use 0 for quiet, 1 for normal records, or 2 for debug records.
LogLevel 0

# Use 0 for dual-stack, 4 for IPv4, or 6 for IPv6.
BindFamily 0
```

The valid ranges are:

- `Port`: `1..=65535`
- `Delay`: `1..=2147483647` milliseconds
- `MaxLineLength`: `3..=255` bytes
- `MaxClients`: `1..=2147483647`
- `LogLevel`: `0`, `1`, or `2`
- `BindFamily`: `0`, `4`, or `6`

The default config file is optional.
A path given with `-f` must exist and must contain valid startup values.

## Signals

- `SIGTERM` drains the client records and stops the server.
- `SIGHUP` reloads the active config file.
- `SIGUSR1` writes connection totals to the active log.
- `SIGPIPE` does not stop the server.

If a reload cannot bind a replacement listener, russhpit keeps the current listener.
Invalid reload values produce warnings and keep their previous values.

## Port behavior and fixes

russhpit follows the C implementation where that behavior is safe and defined.
The port also makes these choices:

- It keeps unsent bytes after a partial socket write.
- It uses monotonic time for delays and connection durations.
- It uses non-blocking listener and client sockets.
- It keeps the old listener when a reload cannot bind a new listener.
- It rejects malformed config values instead of accepting token prefixes.
- It reports a missing explicit config file as an error.
- It documents the implemented `-s` and `-V` options.
- It waits one full delay before it sends the first banner line.

The tests cover the banner generator, config and argument parsing, queue and send behavior, the event loop, Unix sockets, signals, and a localhost client.

## License

russhpit is available under the [MIT License](LICENSE). Files that state a different license keep that license.
See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) for details.
