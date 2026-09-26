# qol_probe — the editor-input contract

Locks the input-tool behaviors that real-world framework editors (Reddit's
Lexical composer, Slack, x.com) forced us to fix. The old build lied here:
`type` reported success while text piled up in the wrong node, `clear`
claimed "cleared" without emptying anything, `Control+a` was unsupported,
and a late-hydrating draft scrambled everything.

## Run

```bash
python3 tools/qol_probe/run.py          # uses target/release/bladebro
BLADEBRO=/path/to/bladebro python3 tools/qol_probe/run.py
```

Own fixture server (port 8794), own `BLADE_HOME` (`/tmp/qol_probe_home`),
no display leakage. Exit 0 = all passed. Takes ~10s.

## What it proves

1. `type` into a **wrapper ref** lands in the live editor and the end state
   is EXACTLY the typed text — even when a draft hydrates mid-typing
   (bounded corrective pass in `finalize_edit`).
2. The wrapper element is never written to (the old JS fallback used to
   dump text into it and call it success).
3. The verdict is honest: it names where the text landed (`landed in eN:
   the live editor`) and never claims a clear that did not empty the field.
4. `clear` on the live editor verifies empty.
5. A stale wrapper ref **heals** (hidden textbox match) and the action
   still lands in the live editor.
6. `press` accepts chords: `Control+a` + `Backspace` clears a field — the
   manual recipe the old build made impossible.
7. `fill` works as a **batch step** and as a **run step** (both surfaces used
   to reject it — schema enum drift plus a missing runtime arm).
8. `wait` steps report condition match vs `→ else` on timeout; `else` on a
   `settle` condition errors by name instead of being silently ignored.
9. `extract=auto` picks substance over unit-count fragments (`extract.html`):
   clean titles, urls and prices — no `"2 units"` garbage.
10. the **pause contract**: `rb pause` refuses every disruptive path
    (`act ... url=`, `see <url>`, `open-tab`/`switch-tab`/`close-tab`,
    `collect`), the page never moves, reads (`see`, `eval`) stay available,
    and `rb resume` restores navigation.

## The fixture models

- **Facade composer**: wrapper textarea → focus mounts the rich editor
  (contenteditable), which takes focus and hides the wrapper.
- **Late draft**: localStorage draft prepends 600ms AFTER the mount — i.e.
  mid-typing if the agent types fast (the reported scramble).
- **Dead execCommand**: `document.execCommand` returns false and changes
  nothing. Only trusted keyboard input works. Any fix that leans on
  programmatic editing fails here — by design; that is the observed
  contract of Lexical-class editors.
- **Remount stress**: an editor that replaces its node once on first input
  (ref churn).
- **Extract fixture** (`extract.html`): a unit-count widget group (the
  "2 units" garbage class) next to a real listing-card group — auto-extract
  must pick the cards (url + price + clean title) and drop the fragments.

## Notes

- Assertions are **end-state based** on purpose: character timing varies
  with load, and the contract is about the final DOM + the verdict text,
  not about a specific intermediate sequence.
- Under Xvfb the browser window never gets OS focus, so Chrome defers a
  programmatic `focus()` event until the first trusted input. The product
  handles both worlds (deferred and immediate focus); the fixture exercises
  the harder one.
