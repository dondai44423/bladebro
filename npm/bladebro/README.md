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

## Two ways to use Bladebro

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

## Platforms

Prebuilt binaries available for:

- Linux x86_64 (`linux-x64`)
- Linux ARM64 (`linux-arm64`)
- macOS Apple Silicon (`darwin-arm64`)
- macOS Intel (`darwin-x64`)
- Windows x86_64 (`windows-x64`)

Other platforms: [build from source](https://github.com/dondai44423/bladebro#building-from-source).

## License

Apache-2.0
