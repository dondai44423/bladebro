# Hermes integration verification

The native `cargo test --release` contract test (`tests/hermes_cli.rs`) runs the
real Bladebro executable against a separate fake Hermes CLI process. It covers
setup, restore, profiles, recovery, conflicting edits, misleading successful
writes, and private prior settings on Linux, macOS, and Windows CI.

This additional probe checks that contract against an **installed Hermes CLI
and its actual tool dispatcher**, using disposable homes and local HTTP
fixtures. It makes no model calls and copies no credentials:

```sh
python3 tools/hermes_probe/run.py \
  --hermes-root /path/to/hermes-agent \
  --python /path/to/hermes-dependency-environment/bin/python \
  --output /tmp/hermes-probe-results
```

`--hermes` selects the CLI executable; `--binary` defaults to
`target/release/bladebro`. Use the Python environment selected by your Hermes
launcher, which may differ from an older checkout's `venv`. Chrome/Chromium
and Bladebro's normal headful Linux prerequisites must be installed.

Assertions check browser-only suppression across platform tool policies,
unchanged web selection, full and Tool Search schemas, and actual dispatch of
all five MCP tools. Browser checks include exact Unicode form submission,
hidden-text exclusion, storage readback, and a real cached PNG under a home
containing spaces and Unicode, with a long inherited temporary path. Configuration checks cover readback, restoration,
repeated commands, failed connection, malformed input, recovery and edit conflicts.
Unix-only wrapper and symlink checks supplement the portable native tests.

Run twice with separate output directories. Logs include fixture/tool results;
passing this probe does not establish native GUI behavior on other operating
systems or guarantee compatibility with future breaking Hermes CLI changes.
