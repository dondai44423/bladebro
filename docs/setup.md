# Setup

[Bladebro README](../README.md) · [Configuration](configuration.md) · [Tool guide](tools.md)

Bladebro needs a Chromium-family browser on the host running the process. The npm launcher also requires Node.js 14 or later. The standalone Rust binary runs without the npm launcher.

## Prebuilt binaries

```sh
npm install -g bladebro
bladebro doctor
```

npm selects the native package through optional dependencies. Leave those enabled. The package has no postinstall script. Standalone binaries and corresponding SHA-256 checksums are also available from [GitHub Releases](https://github.com/dondai44423/bladebro/releases).

| Platform | npm binary package |
|---|---|
| Linux x86_64 | `bladebro-linux-x64` |
| Linux ARM64 | `bladebro-linux-arm64` |
| macOS Intel | `bladebro-darwin-x64` |
| macOS Apple Silicon | `bladebro-darwin-arm64` |
| Windows x86_64 | `bladebro-windows-x64` |

Linux releases require glibc 2.28 or later. An existing binary can fail with `ENOENT` if its loader is missing. Check the libc version if reinstalling does not help. On musl-based systems, try building from source. See the [support matrix](../SUPPORT-MATRIX.md) for runtime coverage.

## MCP

Register the following entry in a client that accepts `mcpServers`.

```json
{
  "mcpServers": {
    "bladebro": {
      "command": "bladebro",
      "args": ["mcp"]
    }
  }
}
```

For a desktop client with a different PATH, put the absolute executable path in `command`. To launch through npm without a global install, use `"command": "npx"` with `"args": ["-y", "bladebro", "mcp"]`. The server communicates over stdin and stdout. Client-side restart instructions depend on the host.

## Agent integrations

### Pi

```sh
pi install npm:bladebro
```

Pi registers five direct MCP tools (`mcp__bladebro__act`, `mcp__bladebro__see`, `mcp__bladebro__state`, `mcp__bladebro__run`, `mcp__bladebro__vision`) from the binary's MCP schema. Start a new session if the current one does not pick them up.

### DeepSeek Harness

In the web UI, use **Plugins**, or install into the web profile from the terminal.

```sh
dsh plugin --profile web add bladebro
```

Restart the profile and start a new chat. DSH exposes the MCP tools under `mcp__bladebro__`. Chrome must be installed on the DSH host. Keep a single server registered under the Bladebro name.

For Desktop, use its Plugins page. For terminal setup, enable **Manage dsh Command**, fully quit Desktop, run its bundled command below, and reopen the app. The standalone npm DSH CLI cannot manage the app-owned profile.

```sh
dsh plugin --profile desktop add bladebro
```

Use **Update** in Plugins or `dsh plugin --profile web update bladebro`. Use `remove bladebro` to uninstall from that profile. Package-manager release-age policies can delay version selection. You can request an exact published version with `bladebro@<version>`, subject to that policy.

Optional profile configuration in `cordis.patch.yml`:

```yaml
- id: bladebro
  config:
    binaryPath: /absolute/path/to/bladebro
    env:
      CHROME_PATH: /absolute/path/to/chrome
      BLADE_HOME: /absolute/path/to/private/browser-data
    toolCallTimeoutMs: 180000
```

Omit `binaryPath` to use the package binary. Windows paths can use forward slashes. The default tool-call budget is 180 seconds. If you cancel after browser input has been dispatched, inspect the destination before issuing the action again.

### Hermes

```sh
npm install -g bladebro
bladebro hermes on
```

Start a new Hermes chat and restart a running gateway or desktop client. Setup registers the standard MCP server and suppresses Hermes' built-in browser toolset. Web search and extraction keep their settings. Use a Hermes version with `config get --json --raw`, `config unset`, and `mcp test`.

```sh
bladebro hermes status
bladebro hermes off
```

Use the same `--profile` for setup and restore when choosing a named profile. `HERMES_HOME` and `--hermes /path/to/hermes` also select the target instance. Hermes may expose the tools through Tool Search. Interrupted changes can resume with `on` or restore with `off`. Avoid changing Hermes configuration concurrently with the switch.

## From source

The crate declares Rust 1.86 as its minimum and the checkout pins the toolchain used by CI. Use the repository pin for development.

```sh
git clone https://github.com/dondai44423/bladebro.git
cd bladebro
cargo build --release
./target/release/bladebro doctor
```

See [Contributing](../CONTRIBUTING.md) for the edit and verification workflow.

## Updating

| Install | Update |
|---|---|
| Global npm | `npm update -g bladebro` |
| Standalone binary | `bladebro -u` |
| Source checkout | Pull changes and rebuild |
| Agent extension or plugin | Update through that host's package manager |

The standalone updater verifies the official release checksum and backs up the previous binary. Restore it with `bladebro --rollback`. An npm-managed install directs you to npm. Restart the CLI daemon with `bladebro stop` and restart any MCP client process to pick up a replacement executable.
