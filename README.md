# systemd-mcp

An [MCP](https://modelcontextprotocol.io/) server that exposes
[systemd](https://systemd.io/)'s
[varlink](https://varlink.org/) interfaces as MCP tools - so your AI
assistant can talk to PID 1 and friends. What could possibly go wrong?

Zero configuration. On startup it discovers every varlink socket in the
registry, introspects the IDL, and hands your AI a full set of typed
tools. On a typical Fedora/Ubuntu system that's **50–80+ tools** covering
units, journals, networking, DNS, timezones, credentials, and more.

## What it does

systemd-mcp scans a directory of varlink sockets (by default
`/run/varlink/registry`), introspects every interface it finds, and
registers each varlink method as an MCP tool, complete with JSON Schema
input descriptions derived from the varlink IDL. An AI model connected
via MCP can then discover and call those methods directly.

Streaming (varlink `more`) calls are fully supported: all continuation
replies are collected and returned as a single result.

## Examples

These are example conversations - the AI picks the right varlink call
on its own based on the natural-language question.

```
❯ can this machine be factory reset?

● systemd - io.systemd.FactoryReset.CanRequestFactoryReset (MCP)()
● systemd - io.systemd.FactoryReset.GetFactoryResetMode (MCP)()

● Yes. Factory reset is supported and currently not scheduled.
  Would you like me to request one?

❯ no. no I would not.
```

```
❯ encrypt a secret for my service

● systemd - io.systemd.Credentials.Encrypt (MCP)
    (text: "db-password=hunter2", name: "myapp-db")

● Done. Here's your encrypted credential (base64):

  YAAAAAIAAADfbQpYAgAAAHsAIKRpBdHvLb2a...

  Drop it into /etc/credstore/myapp-db and reference it
  with LoadCredentialEncrypted=myapp-db in your unit file.
  The plaintext never touches disk.
```

```
❯ something keeps OOM-killing my builds, what's going on?

● systemd - io.systemd.Unit.List (MCP)(name: "user@4711.service")

● Your user slice has ManagedOOMMemoryPressure=kill and MemoryHigh
  set to 8G. Peak memory was 7.9G on the last invocation - you're
  right on the edge.

  You could raise the limit:

    systemctl --user set-property slice.service MemoryHigh=12G

  Or buy more RAM. I'm an AI, not a miracle worker.
```

```
❯ what does my system look like?

● systemd - io.systemd.Hostname.Describe (MCP)()
● systemd - io.systemd.Manager.Describe (MCP)()

● You're on heimdall, a Framework Laptop 16 running Debian
  unstable with kernel 6.19.8, systemd 260. Architecture: x86-64, no
  virtualization. System state: running, 0 failed units, 312
  units loaded. Boot took ~8s to reach userspace.
```

```
❯ how much cpu does NetworkManager use?

● systemd - io.systemd.Unit.List (MCP)(name: "NetworkManager.service")

● NetworkManager.service resource usage:

  - Total CPU time: ~4.2s (CPUUsageNSec)
  - Current tasks: 4
  - Memory: 11.2 MB (peak 14.8 MB)
  - Swap: 0

  It's been running since boot - 4 seconds of CPU over 3 days of
  uptime is very modest for a network daemon.
```

```
❯ show me recent NetworkManager logs

● systemd - io.systemd.JournalAccess.GetEntries (MCP)
    (units: ["NetworkManager.service"], limit: 5)

● Recent NetworkManager activity:

  - State changed to CONNECTED_GLOBAL
  - DHCPv4 on enp2s0: lease renewed, address 10.0.2.15
  - Policy: set 'coffeeshop-5g' (wlp3s0) as default for IPv6
  - DHCPv6 on wlp3s0: new lease acquired
  - GNOME Shell NetworkAgent registered

  Looks normal - wired and wireless are both up, DHCP leases healthy.
```

## What gets exposed

The exact set of tools depends on what's running, but typical interfaces
include:

| Interface | What it does |
|---|---|
| `io.systemd.Unit` | List units, set properties |
| `io.systemd.Manager` | Describe PID 1 state, reload, reexecute |
| `io.systemd.JournalAccess` | Query the journal with filters |
| `io.systemd.Journal` | Rotate, flush, synchronize log files |
| `io.systemd.Hostname` | Machine metadata, OS, firmware |
| `io.systemd.Credentials` | Encrypt/decrypt credentials |
| `io.systemd.Machine` | Container/VM management |
| `io.systemd.MachineImage` | Image management (clone, remove, …) |
| `io.systemd.Import` | Pull remote images |
| `io.systemd.sysext` | System extension management |
| `io.systemd.UserDatabase` | User/group record lookups |
| `io.systemd.Login` | Session management |
| `io.systemd.FactoryReset` | Factory reset controls |

…and whatever else is registered in the varlink socket directory.

## Safety

systemd-mcp makes no attempt to stop an AI from restarting your display
manager. Or your database. Or PID 1. It exposes every method it finds,
read-write, no guardrails. The MCP client is your only line of defence —
most clients (Claude Desktop, Claude Code) will prompt before executing
a tool call, so you get a chance to say no.

Run it as a non-root user and rely on systemd's own privilege checks if
you value your uptime. Or don't. It's April 1st.

## Requirements

- **systemd 260+** - the varlink socket registry (`/run/varlink/registry`)
  was introduced in v260. Older versions don't expose varlink sockets
  this way.
- **Rust 2024 edition** (1.85+) to build.

## Building

```sh
cargo build --release
```

## Usage

```sh
systemd-mcp [SOCKET_DIR]
```

`SOCKET_DIR` defaults to `/run/varlink/registry`. The server speaks MCP
over stdio, so you need an MCP client on the other end.

### Claude Desktop

Add to your `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "systemd": {
      "command": "/path/to/systemd-mcp"
    }
  }
}
```

### Claude Code

```sh
claude mcp add systemd /path/to/systemd-mcp
```

### Logging

Set the `RUST_LOG` environment variable to control log verbosity. Logs
go to stderr so they stay out of the MCP protocol stream on stdout.

```sh
RUST_LOG=debug systemd-mcp
```

## How it works

1. **Discovery**: On startup, the server opens the socket directory and
   connects to each Unix socket it finds. For each socket it calls
   `org.varlink.service.GetInfo` and `GetInterfaceDescription` to learn
   the available interfaces and their IDL definitions.

2. **Schema mapping**: Varlink IDL types are translated to JSON Schema
   so MCP clients can present proper input descriptions to the model.
   Booleans, integers, floats, strings, arrays, maps, enums, and nested
   objects all map to their JSON Schema counterparts.

3. **Tool registration**: Each varlink method becomes an MCP tool named
   `<interface>.<Method>` (e.g.
   `io.systemd.Manager.ListUnits`). Only interfaces whose name matches
   the socket filename are registered, avoiding duplicates when a socket
   serves multiple interfaces.

4. **Invocation**: When a tool is called, the server opens (or reuses)
   a connection to the appropriate socket, sends the varlink call with
   `more: true`, collects all replies, and returns the result as JSON.

## License

LGPL-2.1-or-later
