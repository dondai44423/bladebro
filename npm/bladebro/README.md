# Bladebro

God-tier agentic browser driver for AI agents. Few tools, full control, real stealth, max token efficiency.

## Install

```bash
npm install -g bladebro
```

Or use without installing:

```bash
npx bladebro mcp
```

## Usage

Bladebro speaks MCP (Model Context Protocol) over stdio. Point your AI agent at it:

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

## Requirements

- Node.js >= 14
- Chrome/Chromium installed (Bladebro finds it automatically)

## Platforms

Prebuilt binaries available for:

- Linux x86_64 (`linux-x64`)
- macOS Apple Silicon (`darwin-arm64`) — coming soon
- macOS Intel (`darwin-x64`) — coming soon
- Windows x86_64 (`win32-x64`) — coming soon

Other platforms: [build from source](https://github.com/dondai44423/bladebro#building-from-source).

## License

MIT
