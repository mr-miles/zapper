# Fuzzing

Fuzz targets for the parsers that read what a guest sends: they run on the host, in the daemon that holds the secrets, so a bug in one is a bug in the boundary. Each target calls an entry point in `zapper_core::fuzz` (feature `fuzz`), which runs the parser and checks what must hold whatever the input.

| Target | Reads |
|---|---|
| `dns_query` | A DNS query from the guest, and the resolver's answer to it |
| `tls_hello` | The first TLS record of a connection: its length and the server name (passthrough is decided on it) |
| `authority` | A host and port as written in CONNECT or Host, and host name validation |
| `guest_line` | The guest agent's control channel: bounded lines of JSON messages |
| `frames` | Terminal frames, by the host's reader and the guest's codec |
| `mcp_line` | The MCP gateway's lines |

Run by hand; it needs nightly and cargo-fuzz (`rustup toolchain install nightly`, `cargo install cargo-fuzz`). From this directory:

```
cargo +nightly fuzz run -O dns_query corpus/dns_query seeds/dns_query -- -max_total_time=600
```

`seeds/` holds a valid input or two per target, committed; `corpus/` is what the fuzzer finds and keeps, and `artifacts/` the inputs that crashed, both ignored by git. To replay a crash: `cargo +nightly fuzz run -O <target> artifacts/<target>/<file>`. Once fixed, add the input as a unit test next to the parser.
