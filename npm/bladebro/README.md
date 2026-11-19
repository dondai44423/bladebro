# Bladebro

Drive a real Chromium-family browser from your AI agent or terminal. Five MCP tools (`act`, `see`, `state`, `run`, `vision`) in one Rust binary — persistent Live Page Model, delta-first token efficiency, and real stealth.

Full documentation: **[github.com/dondai44423/bladebro](https://github.com/dondai44423/bladebro)**

## Install

```bash
npm install -g bladebro
bladebro doctor
```

## MCP Server

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

Or without a global install: `"command": "npx"`, `"args": ["-y", "bladebro", "mcp"]`.

## CLI

The first command auto-starts a local daemon and reuses the same browser across calls:

```bash
bladebro nav https://example.com
bladebro see content
bladebro act click e5
bladebro stop

bladebro see content --json    # single-object JSON output
bladebro help --json           # full tool schemas + CLI usage
```

## Agent Integrations

- **Pi**: `pi install npm:bladebro` (registers five native `browser.*` tools).
- **Hermes**: `bladebro hermes on` (registers MCP and suppresses only Hermes' built-in browser toolset; `status` / `off` to inspect or restore).
- **DeepSeek Harness (DSH)**: install via the Web/Desktop **Plugins** page or `dsh plugin --profile web add bladebro` (for Desktop CLI, enable **Manage dsh Command**, quit Desktop, and use its bundled `dsh plugin --profile desktop add bladebro`). Tools appear under `mcp__bladebro__*`. Optional `cordis.patch.yml` overrides (`binaryPath`, `env.CHROME_PATH`, `env.BLADE_HOME`, `toolCallTimeoutMs`: default `180000`) are documented in the [Setup guide](https://github.com/dondai44423/bladebro/blob/main/docs/setup.md#deepseek-harness).

## Requirements & Platforms

- Node.js ≥ 14 (for the npm launcher; standalone binary has zero Node dependency)
- Chrome, Chromium, Brave, Helium, Edge, Vivaldi, or Opera installed on the host
- Prebuilt binaries via `optionalDependencies` (zero install scripts): `linux-x64` and `linux-arm64` (glibc ≥ 2.28; build from source on Alpine/musl), `darwin-arm64`, `darwin-x64`, `windows-x64`

## License

Apache-2.0
