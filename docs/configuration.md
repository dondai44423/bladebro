# Configuration

[Bladebro README](../README.md) · [Setup](setup.md) · [Browser modes](browser-modes.md)

Set environment variables on the Bladebro process. For MCP, set them in the client's server entry. For shell use, set them before the daemon starts. If you already have a daemon, run `bladebro stop` before changing settings. A command that requests a different lane or conflicting launch settings can be refused. For a one-time command, use `--no-daemon`.

## Data and profiles

| Setting | Effect |
|---|---|
| `BLADE_HOME` | Override the complete data root, including daemon state, artifacts, sessions, and profiles. |
| `XDG_STATE_HOME` | On Unix, use its `blade` subdirectory when no Bladebro root is specified. Existing installations may keep their legacy location. |
| `BLADE_PROFILE_DIR` | Override the owned profile directory. Its default is `<data root>/profile`. |
| `BLADE_FRESH=1` | Use an ephemeral browser profile. |
| `BLADE_INCOGNITO=1` / `0` | Enable or disable the agent browser's incognito compatibility mode. |
| `CHROME_PATH` | Select an executable Chrome or Chromium path. An invalid explicit override produces an error. |
| `BLADE_LANE=real` | Select real-browser mode for this process. |

On Unix the normal data root is `~/.local/state/blade` or a previously used `~/.blade`. On Windows it is `%USERPROFILE%\.blade`. Run `bladebro doctor` to see which path resolved. Use a private directory because it can contain cookies, session snapshots, and page artifacts.

On Raspberry Pi OS or Debian trixie arm64 where Pi compatibility is detected, the agent lane can use managed incognito. History, cache, and automatic localStorage persistence are unavailable there. Use named session save/load when needed. The support matrix documents the physical-hardware coverage. [Support matrix](../SUPPORT-MATRIX.md).

## Proxy

Set `BLADE_PROXY` to the proxy URL you intend Chromium to use, for example:

```sh
BLADE_PROXY=http://proxy.example:8080 bladebro nav https://example.com --no-daemon
```

Embedded `user:pass@` credentials are refused at launch. Use an endpoint you can access without inline credentials or an authenticated forwarder you administer. After changing routes, verify connectivity with the configured proxy. The browser route and the DevTools connection are distinct. CDP stays local to the controlled browser in the normal setup. The direct HTTP body-save option on `act download` supports HTTP(S) proxies and refuses other configured types. [Tool guide](tools.md#act).

## Display and interaction

| Setting | Default or effect |
|---|---|
| `BLADE_NO_ACTIVATE=1` | Keep Bladebro from raising the browser window on input, navigation, and tab operations (`open-tab` opens in the background and reports that input focus is suppressed; initial browser launch on macOS/Windows still opens Chrome's normal window). |
| `BLADE_AUDIO` | `0`/`off` (default — browser audio is muted via `--mute-audio`) or `1`/`on` to opt in to audible playback. Persist with `bladebro audio on` / `bladebro audio off`. |
| `BLADE_LOCALE` | Locale, default `en-US`. |
| `BLADE_TZ` | Override timezone using an IANA name such as `Asia/Kathmandu`. |
| `BLADE_CONSENT` | `reject` (default), `accept`, or `off` for recognized consent dialogs. |
| `BLADE_PACE=off` | Disable the input pacing governor. |
| `BLADE_NO_COMPRESS=1` | Disable automatic output pruning. |
| `BLADE_PLAIN=1` | Suppress ANSI styling in CLI output. |
| `BLADE_CMD_TIMEOUT` | Daemon response timeout in seconds, default 300. |
| `BLADE_IDLE_TIMEOUT` | Owned daemon idle timeout in seconds, default 600. |
| `BLADE_NO_UPDATE_CHECK=1` | Skip update checks. |
| `BLADE_TRANSPORT=pipe` | Select the optional MCP DevTools pipe. Default is WebSocket. |
| `BLADE_CHROME_FLAGS` | Extra launch flags, parsed as shell-style arguments. |
| `BLADE_RB_DEBUG=1` | Show browser stderr for real-browser launches. |

Advanced compatibility settings include `BLADE_GPU`, `BLADE_WEBGL`, `BLADE_MEDIA`, `BLADE_NOISE`, and `BLADE_NO_WARMING`. Defaults are the normal supported starting point. Consult the [browser implementation](../src/browser/) and [compatibility evidence](../SUPPORT-MATRIX.md) when diagnosing a host-specific issue.

## Linux and troubleshooting

| Symptom | Check |
|---|---|
| No browser found | Run `bladebro doctor`, install a Chromium-family browser, or set `CHROME_PATH`. |
| Browser launches on a headless server and immediately exits | Ensure Xvfb and a supported window manager such as `xfwm4` are installed. Doctor checks the display prerequisites. |
| Flatpak browser has no access to uploads | Use a detected Flatpak export rather than a wrapper around it. On the agent lane, the driver stages uploads into a shared per-process directory and verifies delivery in-page. The desktop D-Bus and portal must be available. |
| CLI still uses old flags or binary | Run `bladebro stop`, then start the command again. Restart an existing MCP host process separately. |
| A browser you started stays open after the server exits | External attach leaves shutdown to the caller. |
| A download or read produced a file path | The file belongs to the Bladebro host. Use tool artifact paging when the caller cannot access that filesystem. |

When no native Linux browser is found, discovery can use an installed Chromium-family Flatpak export. The supported agent-lane launch shares the owned profile and downloads directory with the sandbox. Local runtime coverage varies by host. [Support matrix](../SUPPORT-MATRIX.md).
