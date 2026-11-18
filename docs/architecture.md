# Architecture

[Bladebro README](../README.md) · [Tool guide](tools.md) · [Browser modes](browser-modes.md)

Bladebro is a Rust program that controls an installed Chromium-family browser over the Chrome DevTools Protocol (CDP). MCP speaks stdio JSON-RPC. The CLI can reuse a daemon or work in one-shot mode. Both reach the same tool handlers.

```text
MCP client               Terminal
    |                       |
    +-- Five tool handlers -+
               |
       Live Page Model
         ^           |
     observe       act
         |           v
       Chromium over CDP
```

## Live Page Model

Perception takes structural information from the live DOM, assigns compact refs, and preserves the current model across calls. Actions normally return an observable verdict and a delta. Content, outline, filtered controls, and screenshots are separate views, so an agent can choose the detail it needs.

Refs represent controls within the current page. A semantic signature includes frame, role, name, and rank. A structural fingerprint helps re-resolve controls when the underlying DOM node is replaced. If a different item takes a slot, the model can retire the old ref and issue a new one. Form controls keep their refs during ordinary editing. Refresh refs after navigation or a tab switch.

## Actions and recovery

- **Address before input.** Resolve ambiguity before typing or clicking. Targeted keys require a focus check.
- **Dispatch mutations once.** Read back results where possible. A quiet DOM or a connection failure does not justify another click. Safe observations can retry.
- **Report limits on reads.** Truncation, partial extraction, and artifact pagination are visible to the caller.
- **Close the browser you own.** Owned shutdown requests a graceful browser close so persistent storage can flush before process-tree cleanup. Attached browsers remain under the caller's ownership.

## State and layout

The default lane owns a separate profile. Real-browser mode uses a selected install and can clone or attach to an existing profile. WebSocket is the default CDP transport. The MCP pipe transport is an opt-in alternative. [Browser modes](browser-modes.md).

Bladebro can reuse domain-level consent and settle choices after successful encounters. The policy for the current visit still applies. Extraction adapters fit inside `see extract=auto`; they share the same tool schema and disclose partial results.

| Area | Source |
|---|---|
| Tool schemas, MCP and CLI | [src/mcp](../src/mcp/) · [src/cli](../src/cli/) |
| Perception, refs and deltas | [src/page](../src/page/) |
| Actions and outcomes | [src/action](../src/action/) |
| Transport and browser launch | [src/cdp](../src/cdp/) · [src/browser](../src/browser/) |
| Browser selection, cloning and attach | [src/realbrowser](../src/realbrowser/) |
| Persistence and files | [src/state.rs](../src/state.rs) · [src/session_profile](../src/session_profile/) · [src/artifacts.rs](../src/artifacts.rs) |

## Verification

The [support matrix](../SUPPORT-MATRIX.md) separates packages, CI coverage, and actual browser runs. The [historical benchmark](../STEALTH_BENCH.md) records one run with its conditions and per-site outcomes. Use those sources for measured claims. The [contributor guide](../CONTRIBUTING.md) covers changes to code and verification instruments.
