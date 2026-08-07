/**
 * Bladebro pi extension — native pi agent integration.
 *
 * Spawns the bladebro binary as a stdio MCP subprocess, discovers tools
 * via `tools/list`, and registers them natively with pi via
 * `pi.registerTool()`. The agent gets 5 first-class tools (act, see,
 * state, run, vision) with no adapter, no config files, no proxy.
 *
 * Tool definitions come from the binary at startup — zero maintenance.
 * When the Rust tool defs change, the extension picks them up automatically.
 *
 * Lifecycle:
 * - session_start → spawn binary, MCP handshake, tools/list, register tools
 * - tool call     → proxy to binary via tools/call, return result
 * - session_shutdown → kill binary (which kills Chrome)
 *
 * Chrome launches lazily inside the binary (first tools/call only).
 * The binary process itself starts in milliseconds.
 *
 * Auto-adapt: tool descriptions, schemas, new tools, removed tools,
 * changed parameters — all fetched from the binary at session start.
 * The extension never hardcodes tool definitions.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { spawn, type ChildProcess } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";

// ── Binary resolution ─────────────────────────────────────────────────

const PLATFORM_MAP: Record<string, string> = {
  "linux-x64": "bladebro-linux-x64",
  "linux-arm64": "bladebro-linux-arm64",
  "darwin-x64": "bladebro-darwin-x64",
  "darwin-arm64": "bladebro-darwin-arm64",
  "win32-x64": "bladebro-windows-x64",
};

function resolveBinary(): string | null {
  const key = `${process.platform}-${process.arch}`;
  const pkgName = PLATFORM_MAP[key];
  if (!pkgName) return null;
  try {
    const pkgJsonPath = require.resolve(`${pkgName}/package.json`);
    const pkgJson = require(pkgJsonPath);
    const binName = pkgJson.main || "bladebro";
    const binPath = join(dirname(pkgJsonPath), binName);
    return existsSync(binPath) ? binPath : null;
  } catch {
    return null;
  }
}

// ── Minimal MCP stdio client ──────────────────────────────────────────

interface PendingReq {
  resolve: (v: any) => void;
  reject: (e: any) => void;
  timer: ReturnType<typeof setTimeout>;
}

class McpStdio {
  private proc: ChildProcess | null = null;
  private nextId = 1;
  private pending = new Map<number, PendingReq>();
  private buffer = "";
  private alive = false;

  start(binaryPath: string): Promise<void> {
    return new Promise((resolve, reject) => {
      this.proc = spawn(binaryPath, ["mcp"], {
        stdio: ["pipe", "pipe", "pipe"],
        env: { ...process.env },
      });

      const onExit = () => {
        this.alive = false;
        for (const [, p] of this.pending) {
          clearTimeout(p.timer);
          p.reject(new Error("bladebro process exited"));
        }
        this.pending.clear();
      };

      this.proc.on("exit", onExit);
      this.proc.on("error", (err) => {
        onExit();
        reject(err);
      });

      this.proc.stdout!.on("data", (data: Buffer) => this.onData(data));
      this.proc.stderr!.on("data", (data: Buffer) => {
        // Forward only errors/warnings to pi stderr. Info lines like
        // "[bladebro] MCP server ready" are suppressed to keep the
        // agent TUI clean on startup.
        const text = data.toString();
        for (const line of text.split("\n")) {
          const trimmed = line.trim();
          if (!trimmed) continue;
          if (/error|panic|fatal|warn|fail/i.test(trimmed)) {
            process.stderr.write(line + "\n");
          }
        }
      });

      // MCP initialize handshake, then signal ready.
      this.request("initialize", {
        protocolVersion: "2025-06-18",
        capabilities: {},
        clientInfo: { name: "pi-bladebro", version: "1.0.0" },
      }, 10000)
        .then(() => {
          this.notify("notifications/initialized", {});
          this.alive = true;
          resolve();
        })
        .catch(reject);
    });
  }

  private onData(data: Buffer): void {
    this.buffer += data.toString();
    let idx: number;
    while ((idx = this.buffer.indexOf("\n")) >= 0) {
      const line = this.buffer.slice(0, idx).trim();
      this.buffer = this.buffer.slice(idx + 1);
      if (line) this.onMessage(line);
    }
  }

  private onMessage(line: string): void {
    let msg: any;
    try { msg = JSON.parse(line); } catch { return; }
    if (msg.id !== undefined && this.pending.has(msg.id)) {
      const entry = this.pending.get(msg.id)!;
      this.pending.delete(msg.id);
      clearTimeout(entry.timer);
      if (msg.error) entry.reject(msg.error);
      else entry.resolve(msg.result);
    }
  }

  private request(method: string, params: any, timeoutMs = 180000): Promise<any> {
    return new Promise((resolve, reject) => {
      const id = this.nextId++;
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`MCP timeout: ${method} (${timeoutMs}ms)`));
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, timer });
      const msg = JSON.stringify({ jsonrpc: "2.0", id, method, params });
      this.proc?.stdin?.write(msg + "\n");
    });
  }

  private notify(method: string, params: any): void {
    const msg = JSON.stringify({ jsonrpc: "2.0", method, params });
    this.proc?.stdin?.write(msg + "\n");
  }

  async listTools(): Promise<any[]> {
    const result = await this.request("tools/list", {}, 10000);
    return result.tools || [];
  }

  async callTool(name: string, args: any): Promise<any> {
    return this.request("tools/call", { name, arguments: args });
  }

  isAlive(): boolean { return this.alive && this.proc !== null; }

  async stop(): Promise<void> {
    if (!this.proc) return;
    this.alive = false;
    try { this.proc.stdin?.end(); } catch {}
    this.proc.kill("SIGTERM");
    // Wait up to 5s for graceful exit (Chrome shutdown takes up to 3s),
    // then SIGKILL. The orphan reaper handles any leaked Chrome.
    await new Promise<void>((resolve) => {
      const t = setTimeout(() => {
        this.proc?.kill("SIGKILL");
        resolve();
      }, 5000);
      this.proc?.on("exit", () => { clearTimeout(t); resolve(); });
    });
    this.proc = null;
  }
}

// ── Extension ─────────────────────────────────────────────────────────

export default function bladebroExtension(pi: ExtensionAPI) {
  let client: McpStdio | null = null;
  let binaryPath: string | null = null;
  // Restart lock: prevents concurrent restarts from spawning
  // multiple binaries. When two parallel tool calls both see
  // isAlive() === false, they share the same restart promise.
  let restartPromise: Promise<void> | null = null;

  async function ensureClient(): Promise<McpStdio> {
    if (client?.isAlive()) return client;
    if (restartPromise) {
      await restartPromise;
      return client!;
    }
    if (!binaryPath) throw new Error("Bladebro binary not available");
    const p = (async () => {
      const c = new McpStdio();
      await c.start(binaryPath!);
      client = c;
    })();
    restartPromise = p;
    try {
      await p;
    } finally {
      restartPromise = null;
    }
    return client!;
  }

  pi.on("session_start", async (_event, ctx) => {
    binaryPath = resolveBinary();
    if (!binaryPath) {
      const key = `${process.platform}-${process.arch}`;
      ctx.ui.notify(
        `Bladebro: no binary for ${key}. Run: npm install bladebro`,
        "error",
      );
      return;
    }

    try {
      await ensureClient();
    } catch (err: any) {
      ctx.ui.notify(`Bladebro: failed to start: ${err.message}`, "error");
      client = null;
      return;
    }

    // Discover tools from the binary and register them natively.
    let tools: any[];
    try {
      tools = await client!.listTools();
    } catch (err: any) {
      ctx.ui.notify(`Bladebro: tools/list failed: ${err.message}`, "error");
      return;
    }

    for (const tool of tools) {
      pi.registerTool({
        name: tool.name,
        label: tool.name,
        description: tool.description,
        parameters: Type.Unsafe(tool.inputSchema),
        async execute(_toolCallId, params, _signal, _onUpdate, _ctx) {
          // Ensure the binary is alive — restart if it died.
          // ensureClient handles the restart race condition.
          const c = await ensureClient();
          const result = await c.callTool(tool.name, params);
          return {
            content: result.content || [],
            details: {},
            isError: result.isError || false,
          };
        },
      });
    }

    ctx.ui.notify(`Bladebro: ${tools.length} tools ready`, "info");
  });

  pi.on("session_shutdown", async () => {
    if (client) {
      await client.stop();
      client = null;
    }
  });
}
