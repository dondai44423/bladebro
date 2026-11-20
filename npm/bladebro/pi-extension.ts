/**
 * Bladebro pi extension — the bladebro MCP server, registered with pi.
 *
 * This is the whole extension. It resolves the bladebro binary this package
 * installed and hands it to pi as a stdio MCP server; pi's own MCP client does
 * everything else — spawn, handshake, tool-call proxying, shutdown. The five
 * tools arrive with the binary's full definitions (descriptions, schemas and
 * the server's `initialize` instructions), so no meaning is copied here and
 * there is nothing that can drift when the Rust side changes.
 *
 * `exposure: "direct"` declares the tools to the model like any other pi tool,
 * and the session waits for this server before the first prompt. The timeout
 * covers the driver's long operations (run/batch/collect), not pi's 60s default.
 *
 * No protocol code, no copied schemas, no rendering: a troubleshooting step
 * belongs at the source — the binary — never in this shim.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { accessSync, constants, existsSync, statSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";

/** npm platform packages → the binary each one carries. */
const PLATFORM_MAP: Record<string, string> = {
  "linux-x64": "bladebro-linux-x64",
  "linux-arm64": "bladebro-linux-arm64",
  "darwin-x64": "bladebro-darwin-x64",
  "darwin-arm64": "bladebro-darwin-arm64",
  "win32-x64": "bladebro-windows-x64",
};

/** Resolve a module loader that works under both jiti CJS and native Node/Bun ESM. */
function getRequire(): NodeRequire {
  if (typeof require === "function") return require;
  return createRequire(import.meta.url);
}

/** Absolute path of this platform's executable binary, or null when not installed. */
function resolveBinary(): string | null {
  const pkgName = PLATFORM_MAP[`${process.platform}-${process.arch}`];
  if (!pkgName) return null;
  try {
    const req = getRequire();
    const pkgJsonPath = req.resolve(`${pkgName}/package.json`);
    const pkgJson = req(pkgJsonPath);
    const defaultName = process.platform === "win32" ? "bladebro.exe" : "bladebro";
    const binPath = join(dirname(pkgJsonPath), pkgJson.main || defaultName);
    if (!existsSync(binPath) || !statSync(binPath).isFile()) return null;
    accessSync(binPath, process.platform === "win32" ? constants.F_OK : constants.X_OK);
    return binPath;
  } catch {
    return null;
  }
}

export default function bladebroExtension(pi: ExtensionAPI) {
  const key = `${process.platform}-${process.arch}`;
  const binary = resolveBinary();

  if (!binary || typeof pi.registerMcpServer !== "function") {
    const why = !binary
      ? `no binary for ${key} — run: npm install bladebro`
      : "this pi build has no MCP registration API — update pi";
    pi.on("session_start", (_event, ctx) => ctx.ui.notify(`Bladebro: ${why}`, "error"));
    return;
  }

  pi.registerMcpServer("bladebro", {
    description: "Stealthy agentic browser driver — act, see, state, run, vision.",
    command: binary,
    args: ["mcp"],
    exposure: "direct",
    timeout: 180,
  });
}
