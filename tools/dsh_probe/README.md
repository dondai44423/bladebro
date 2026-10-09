Run against an actual installed DeepSeek Harness, with this checkout's packed
Bladebro package installed in the same disposable project:

```sh
npm install --prefix work/dsh/runtime @deepseek-ai/dsh
npm pack ./npm/bladebro --pack-destination work/dsh
npm install --prefix work/dsh/runtime ./work/dsh/bladebro-$(node -p "require('./npm/bladebro/package.json').version").tgz
DSH_RUNTIME=work/dsh/runtime BLADEBRO="$PWD/target/release/bladebro" node tools/dsh_probe/run.mjs
```

Node >=22.19 and Chromium are required. Each run creates fresh temporary browser
and harness data. It uses real Cordis, ToolRuntime, the host's native MCP bridge,
Bladebro and Chromium. The only model adapter supplies image-capability metadata;
its inference entry throws. No credentials or paid API are needed.

Assertions cover exact live schemas/descriptions, all five tools, an exact
Unicode form POST and server receipt, hidden text exclusion, Unicode storage,
a real PNG in the native durable attachment store, honest errors, permission
and cancellation gates, failed handshake cleanup, duplicate namespace refusal,
MCP-child crash/reconnect without mutation replay, unload and re-enable.
Child tracking instruments Node's spawn only to observe/kill owned fixture
processes. The production plugin uses DSH's unmodified MCP supervisor.

Run `node tools/dsh_probe/package.mjs` for portable package/resolver boundaries
(all five mapped platforms plus unsupported platforms, paths, config and missing
optional binaries). These simulated branches do not claim native OS coverage.

`profile.mjs` additionally boots a full real Web profile containing the packed
plugin. Set explicit disposable `DSH_HOME` and `BLADE_HOME` and `DSH_RUNTIME` to
the harness installation. It verifies the Plugins inventory, authenticated HTML,
Standard/PTC/Cordis agent capability visibility, and native Plugins disable /
re-enable while preserving a second independent real MCP server. Its metadata
adapter never calls a model. It creates test sessions only under that DSH home.
Require the final `DSH full profile probe: 16 checks passed` receipt; the host's
shutdown controller can otherwise mask an exception's process exit code.
