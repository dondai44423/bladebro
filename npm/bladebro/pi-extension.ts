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
import { Text } from "@earendil-works/pi-tui";
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

// ── TUI rendering helpers ─────────────────────────────────────────────

/** Extract text from MCP tool result content blocks. */
function getResultText(result: any): string {
  if (!result?.content) return "";
  return result.content
    .filter((c: any) => c.type === "text")
    .map((c: any) => c.text)
    .join("\n");
}

/** Format the `act` tool call display. */
function formatActCall(args: any, theme: any): string {
  const action = args.action || "?";
  const bold = (s: string) => theme.bold(s);
  const accent = (s: string) => theme.fg("accent", s);
  const muted = (s: string) => theme.fg("muted", s);
  const title = bold("act");

  switch (action) {
    case "navigate": {
      const url = args.url || "";
      return `${title} ${accent("navigate")} ${muted("→")} ${accent(url)}`;
    }
    case "click": {
      const target = args.ref || args.text || args.label || "";
      return `${title} ${accent("click")} ${muted(target)}`;
    }
    case "type": {
      const target = args.ref || args.label || "";
      const text = args.text || "";
      const press = args.press ? ` ${muted("+ " + args.press)}` : "";
      return `${title} ${accent("type")} ${muted(target ? `${target} ` : "")}${muted(`"${text}"`)}${press}`;
    }
    case "scroll": {
      const dy = args.dy || 0;
      const dx = args.dx || 0;
      const arrow = dy > 0 ? "↓" : dy < 0 ? "↑" : dx > 0 ? "→" : "←";
      const mag = Math.abs(dy || dx);
      return `${title} ${accent("scroll")} ${muted(`${arrow}${mag}`)}`;
    }
    case "fill": {
      const n = args.fields?.length || 0;
      const submit = args.submit ? ` ${muted("+ submit")}` : "";
      return `${title} ${accent("fill")} ${muted(`(${n} field${n !== 1 ? "s" : ""})`)}${submit}`;
    }
    case "batch": {
      const n = args.steps?.length || 0;
      return `${title} ${accent("batch")} ${muted(`(${n} step${n !== 1 ? "s" : ""})`)}`;
    }
    case "hover": {
      const target = args.ref || args.text || args.label || "";
      return `${title} ${accent("hover")} ${muted(target)}`;
    }
    case "press": {
      return `${title} ${accent("press")} ${muted(args.key || "")}`;
    }
    case "eval": {
      const js = (args.js || "").slice(0, 50);
      return `${title} ${accent("eval")} ${muted(`"${js}${js.length >= 50 ? "…" : ""}"`)}`;
    }
    case "wait": {
      const cond = args.condition || "settle";
      const t = args.timeout || 10;
      return `${title} ${accent("wait")} ${muted(`${cond} (${t}s)`)}`;
    }
    case "read": {
      return `${title} ${accent("read")} ${muted(args.ref || "")}`;
    }
    case "back": return `${title} ${accent("back")}`;
    case "forward": return `${title} ${accent("forward")}`;
    case "reload": return `${title} ${accent("reload")}`;
    case "select": {
      const target = args.ref || "";
      const opt = args.option || "";
      return `${title} ${accent("select")} ${muted(`${target} "${opt}"`)}`;
    }
    case "pdf": return `${title} ${accent("pdf")}`;
    case "download": return `${title} ${accent("download")}`;
    case "collect": return `${title} ${accent("collect")}`;
    case "clear": return `${title} ${accent("clear")} ${muted(args.ref || "")}`;
    case "upload": return `${title} ${accent("upload")} ${muted(args.ref || "")}`;
    default: return `${title} ${accent(action)}`;
  }
}

/** Format the `see` tool call display. */
function formatSeeCall(args: any, theme: any): string {
  const bold = (s: string) => theme.bold(s);
  const accent = (s: string) => theme.fg("accent", s);
  const muted = (s: string) => theme.fg("muted", s);
  const title = bold("see");

  const mode = args.mode;
  if (mode === "content") return `${title} ${accent("content")}`;
  if (mode === "outline") return `${title} ${accent("outline")}`;
  if (mode === "model") return `${title} ${accent("model")}`;

  const find = args.find;
  if (find) return `${title} ${accent("find")} ${muted(`"${find}"`)}`;

  const extract = args.extract;
  if (extract) return `${title} ${accent(`extract=${extract}`)}`;

  const scope = args.scope;
  if (scope) return `${title} ${accent("scope")} ${muted(scope)}`;

  const filter = args.filter;
  if (filter) return `${title} ${accent("model")} ${muted(`[${filter}]`)}`;

  const logs = args.logs;
  if (logs) return `${title} ${accent("logs")} ${muted(logs)}`;

  return `${title} ${accent("model")}`;
}

/** Format the `state` tool call display. */
function formatStateCall(args: any, theme: any): string {
  const bold = (s: string) => theme.bold(s);
  const accent = (s: string) => theme.fg("accent", s);
  const muted = (s: string) => theme.fg("muted", s);
  const title = bold("state");

  const op = args.op || "?";
  const extra = args.url || args.name || "";
  return `${title} ${accent(op)}${extra ? ` ${muted(extra)}` : ""}`;
}

/** Format the `run` tool call display. */
function formatRunCall(args: any, theme: any): string {
  const bold = (s: string) => theme.bold(s);
  const title = bold("run");
  return title;
}

/** Format the `vision` tool call display. */
function formatVisionCall(args: any, theme: any): string {
  const bold = (s: string) => theme.bold(s);
  const title = bold("vision");
  return title;
}

/** Format tool result for display. */
function formatResult(text: string, expanded: boolean, isError: boolean, theme: any): string {
  const out = (s: string) => theme.fg("toolOutput", s);
  const warn = (s: string) => theme.fg("warning", s);
  const muted = (s: string) => theme.fg("muted", s);
  const accent = (s: string) => theme.fg("accent", s);

  if (!text) return muted("(no output)");

  const lines = text.split("\n");

  if (isError) {
    // Error: show all lines in warning color
    return lines.map((l) => warn(l)).join("\n");
  }

  // Truncate if not expanded
  const maxLines = expanded ? 9999 : 15;
  const visible = lines.slice(0, maxLines);
  const skipped = lines.length - visible.length;

  const formatted = visible.map((line) => {
    // Outcome lines → accent
    if (line.startsWith("outcome:")) return accent(line);
    // Warning lines (⚠) → warning color
    if (line.includes("\u{26a0}")) return warn(line);
    // Verdict lines with arrow → toolOutput
    return out(line);
  });

  if (skipped > 0) {
    formatted.push(muted(`  … ${skipped} more line${skipped !== 1 ? "s" : ""} (expand to view)`));
  }

  return formatted.join("\n");
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
        const text = data.toString();
        for (const line of text.split("\n")) {
          const trimmed = line.trim();
          if (!trimmed) continue;
          if (/error|panic|fatal|warn|fail/i.test(trimmed)) {
            process.stderr.write(line + "\n");
          }
        }
      });

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

const TOOL_FORMATTERS: Record<string, (args: any, theme: any) => string> = {
  act: formatActCall,
  see: formatSeeCall,
  state: formatStateCall,
  run: formatRunCall,
  vision: formatVisionCall,
};

export default function bladebroExtension(pi: ExtensionAPI) {
  let client: McpStdio | null = null;
  let binaryPath: string | null = null;
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

    let tools: any[];
    try {
      tools = await client!.listTools();
    } catch (err: any) {
      ctx.ui.notify(`Bladebro: tools/list failed: ${err.message}`, "error");
      return;
    }

    for (const tool of tools) {
      const formatter = TOOL_FORMATTERS[tool.name];

      pi.registerTool({
        name: tool.name,
        label: tool.name,
        description: tool.description,
        parameters: Type.Unsafe(tool.inputSchema),
        renderShell: "default",
        renderCall(args: any, theme: any, context: any) {
          const text = new Text("", 0, 0);
          if (formatter) {
            text.setText(formatter(args, theme));
          } else {
            text.setText(theme.bold(tool.name));
          }
          return text;
        },
        renderResult(result: any, options: any, theme: any, context: any) {
          const text = context.lastComponent ?? new Text("", 0, 0);
          const body = getResultText(result);
          text.setText(formatResult(body, options.expanded, result.isError, theme));
          return text;
        },
        async execute(_toolCallId, params, _signal, _onUpdate, _ctx) {
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

    ctx.ui.notify(`Bladebro ready (${tools.length} tools)`, "info");
  });

  pi.on("session_shutdown", async () => {
    if (client) {
      await client.stop();
      client = null;
    }
  });
}
