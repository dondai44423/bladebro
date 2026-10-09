# Bladebro

Stealthy and efficient agentic browser driver for AI agents. Few tools, full control, real stealth, max token efficiency.

## Install

```bash
npm install -g bladebro
```

Or use without installing:

```bash
npx bladebro mcp
```

### DeepSeek Harness (DSH)

Add Bladebro as a native plugin in the Web UI's **Plugins** page, or run:

```bash
dsh plugin --profile web add bladebro
```

Restart the profile and start a new chat. Desktop users can install `bladebro`
from the same **Plugins** page. For terminal setup, enable Desktop's **Manage dsh
Command**, fully quit the app, then use its bundled command:

```bash
dsh plugin --profile desktop add bladebro
```

Use Desktop's command for its profile; the npm DSH CLI cannot manage that
app-owned profile. Open Desktop again after installation.

DSH discovers the five tools as `mcp__bladebro__act`, `see`, `state`, `run` and
`vision` (each with the same prefix). Schemas and descriptions come from the
binary; DSH handles permissions, image attachments, reconnection and cleanup
through its native MCP bridge. DSH may also expose its own generic MCP resource
helpers and choose PTC presentation. No API key, separate MCP configuration, global
Bladebro install or extra plugin package is needed. Chrome/Chromium must be
installed on the machine running the DSH host. A remotely hosted Web UI controls
that host's browser, rather than the device viewing the UI.

Disable/re-enable the **Bladebro** bundle in Plugins, or remove it with
`dsh plugin --profile web remove bladebro` (use `desktop` with Desktop's command).
Restart the profile and begin a new chat after package changes. Other tools and
settings remain intact. Do not also configure a second MCP server named
`bladebro`; DSH refuses the duplicate namespace.

Updates use DSH's package manager: `dsh plugin --profile web update bladebro`,
or **Update** in Plugins. The plugin and native binary travel in the same
package, so ordinary Bladebro releases update both. Existing global/Pi installs
are separate. No background downloader or installation scripts run.

Advanced configuration belongs in the profile's `cordis.patch.yml`:

```yaml
- id: bladebro
  config:
    # Optional absolute source-build/portable executable; package binary is default.
    binaryPath: /absolute/path/to/bladebro
    env:
      CHROME_PATH: /absolute/path/to/chrome
      BLADE_HOME: /absolute/path/to/private/browser-data
    toolCallTimeoutMs: 180000
```

Windows YAML paths can use forward slashes, such as `C:/Tools/bladebro.exe`.
The optional `cwd` and `reconnect` settings use DSH's native MCP configuration.
The default per-call budget is 180 seconds; raise it for longer `run` sequences.
Cancellation stops waiting, but an already-dispatched browser mutation may
finish: observe its destination before trying it again.

The npm CLI keeps its Node.js >=14 requirement; the DSH plugin uses the host's
Node.js requirement. Prebuilt binaries cover Linux x64/ARM64 (glibc >=2.28),
macOS Intel/Apple Silicon and Windows x64. Other targets need a source build and
`binaryPath`. See the official [DSH plugin commands](https://github.com/deepseek-ai/deepseek-harness/blob/master/apps/cli/reference/README.md#plugin-management)
and [Desktop command runtime](https://github.com/deepseek-ai/deepseek-harness/blob/master/apps/desktop/README.md#bundled-command-runtime).

## MCP and CLI

### Option 1: MCP Server

For AI agents that speak MCP (Model Context Protocol). Point your agent at it:

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

Five tools: `act`, `see`, `state`, `run`, `vision`. Full docs at [github.com/dondai44423/bladebro](https://github.com/dondai44423/bladebro).

### Option 2: CLI

For AI agents that run shell commands. One persistent Chrome instance across all commands (auto-daemon):

```bash
bladebro nav https://example.com     # auto-starts daemon + Chrome
bladebro see content                 # uses same Chrome
bladebro act click e5                # uses same Chrome
bladebro stop                        # cleans up

# JSON output for agents:
bladebro nav https://example.com --json
bladebro see content --json

# Agent discovery (returns tool schemas + CLI mapping):
bladebro help --json
```

## Requirements

- Node.js >= 14
- Chrome/Chromium installed (Bladebro finds it automatically)
- Linux prebuilt binaries: glibc >= 2.28

## Platforms

Prebuilt binaries available for:

- Linux x86_64 (`linux-x64`)
- Linux ARM64 (`linux-arm64`)
- macOS Apple Silicon (`darwin-arm64`)
- macOS Intel (`darwin-x64`)
- Windows x86_64 (`windows-x64`)

Other platforms: [build from source](https://github.com/dondai44423/bladebro#from-source).

## License

Apache-2.0
