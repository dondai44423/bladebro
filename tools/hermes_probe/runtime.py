"""Run under the installed Hermes Python environment; no model calls."""
import argparse
import json
import re
import sys
from pathlib import Path

p = argparse.ArgumentParser()
p.add_argument('--hermes-root', required=True, type=Path)
p.add_argument('--url')
p.add_argument('--native', action='store_true')
a = p.parse_args()
sys.path.insert(0, str(a.hermes_root))
from hermes_cli.config import load_config
from hermes_cli.tools_config import _get_platform_tools
from tools.mcp_tool_discovery import discover_mcp_tools
from tools.mcp_tool_lifecycle import shutdown_mcp_servers
from toolsets import TOOLSETS
import model_tools

count = 0

def check(ok, label):
    global count
    assert ok, label
    count += 1
    print(f'PASS {label}', flush=True)

c = load_config()
disabled = c.get('agent', {}).get('disabled_toolsets', [])
expected = {f'mcp__bladebro__{n}' for n in ['act', 'see', 'state', 'run', 'vision']}
try:
    names = set(discover_mcp_tools(['bladebro']))
    check(names == (set() if a.native else expected), 'discovery matches browser selection')
    # Compare tool selection to the same platform's unsuppressed policy: a
    # deliberately restricted platform must not gain web tools during setup.
    for platform in ['cli', 'telegram', 'discord', 'whatsapp', 'slack', 'signal',
                     'homeassistant', 'qqbot', 'yuanbao', 'teams', 'google_chat', 'desktop']:
        ts = sorted(_get_platform_tools(c, platform))
        selected = model_tools._select_tool_names(ts, disabled, True)
        baseline = model_tools._select_tool_names(ts, [n for n in disabled if n != 'browser'], True)
        check((bool(selected & set(TOOLSETS['browser']['tools'])) if a.native and 'browser' in ts else
               not (selected & set(TOOLSETS['browser']['tools']))), f'{platform}: browser policy')
        check(selected & {'web_search', 'web_extract'} == baseline & {'web_search', 'web_extract'},
              f'{platform}: web policy preserved')
        if not a.native:
            check(expected <= selected, f'{platform}: all five MCP tools selected')
    if a.native:
        print(f'PASS TOTAL {count}', flush=True)
        sys.exit(0)
    ts = sorted(_get_platform_tools(c, 'cli'))
    raw = model_tools.get_tool_definitions(enabled_toolsets=ts, disabled_toolsets=disabled,
                                         quiet_mode=True, skip_tool_search_assembly=True)
    schemas = json.dumps(raw)
    check(expected <= {d['function']['name'] for d in raw}, 'five full schemas available')
    check(not any(re.search(r'\b' + re.escape(n) + r'\b', schemas)
                  for n in TOOLSETS['browser']['tools']), 'no native browser names in full schemas')
    assembled = model_tools.get_tool_definitions(enabled_toolsets=ts, disabled_toolsets=disabled, quiet_mode=True)
    catalog = json.dumps(assembled)
    check(all(n in catalog for n in expected), 'all five tools discoverable through Tool Search')
    check(not any(re.search(r'\b' + re.escape(n) + r'\b', catalog)
                  for n in TOOLSETS['browser']['tools']), 'no native browser names in assembled schemas')

    def call(name, args):
        result = model_tools.handle_function_call(name, args, task_id='bladebro-hermes-probe',
                    enabled_toolsets=ts, disabled_toolsets=disabled)
        text = result if isinstance(result, str) else json.dumps(result)
        print(f'RESULT {name} {text[:1800]}', flush=True)
        return text

    if a.url:
        result = call('tool_describe', {'names': sorted(expected)})
        check(all(n in result for n in expected), 'Tool Search schema loading')
        result = call('mcp__bladebro__act', {'action': 'navigate', 'url': a.url})
        check('Hermes fixture' in result, 'act navigates through Hermes dispatcher')
        result = call('mcp__bladebro__see', {'mode': 'content'})
        check('HERMES_VISIBLE_SENTINEL' in result and 'HERMES_HIDDEN_SENTINEL' not in result,
              'see returns visible content without hidden text')
        result = call('mcp__bladebro__state', {'op': 'set-ls', 'name': 'hermes-probe', 'value': 'Δ✓'})
        check('error' not in result.lower(), 'state writes storage through Hermes')
        result = call('mcp__bladebro__state', {'op': 'ls'})
        check('Δ✓' in result or '\\u0394' in result, 'state Unicode storage readback')
        result = call('mcp__bladebro__run', {'steps': [
            {'action': 'fill', 'fields': [{'label': 'Name', 'text': 'Hermes Δ✓'}]},
            {'action': 'click', 'text': 'Save'}, {'action': 'see', 'mode': 'content'}]})
        body = json.loads(result)
        check('Saved Hermes Δ✓' in body.get('result', '') and 'readback mismatch' not in result,
              'run submits exact Unicode value and reads the actual receipt')
        result = call('mcp__bladebro__vision', {'marks': True})
        body = json.loads(result)
        paths = re.findall(r'MEDIA:([^\n]+?\.png)', body.get('result', ''))
        check(bool(paths), 'vision survives MCP image conversion')
        data = Path(paths[0]).read_bytes()
        check(data.startswith(b'\x89PNG\r\n\x1a\n') and len(data) > 1000,
              'vision cached image is an actual nonempty PNG')
        result = call('mcp__bladebro__act', {'action': 'invalid-hermes-probe'})
        check('error' in result.lower(), 'invalid operation remains an honest error')
    print(f'PASS TOTAL {count}', flush=True)
finally:
    shutdown_mcp_servers()
