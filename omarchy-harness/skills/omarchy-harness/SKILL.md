---
name: omarchy-harness
description: Control Omarchy/Hyprland from one persistent Python session with screenshots, keyboard, clicks, AT-SPI, and the omarchy CLI. Use for native, Electron, browser, dialog, or cross-app tasks on this Linux desktop.
---

# Omarchy Harness

Use one CLI call per decision point, not per primitive:

```bash
omarchy-harness <<'PY'
app = "Spotify"
print(oma.desktop())
oma.see(app)
oma.key("ctrl+k", app=app)
oma.type("Alessia Cara", app=app)
print(oma.see(app))
PY
```

The CLI preloads `oma`, `Path`, and `subprocess`. Prefer bounded stdin programs.

## Minimize round trips

Bundle deterministic, reversible steps into one program, then verify once. Stop at a genuine decision boundary: ambiguous identity, new coordinates, an irreversible action, or unexpected state.

## Choose the lowest useful mode

1. `oma.script("omarchy-launch-or-focus …")` / `oma.script("hyprctl dispatch workspace 3")` / `oma.script("omarchy-theme-set 'Tokyo Night'")` for a known exact desktop command.
2. `oma.desktop()` for window/workspace identity — never screenshot to find a window.
3. `oma.ax.dump(app)` when a GTK/Qt app exposes named controls.
4. `oma.see(app)` and vision when the tree is empty (terminals, canvas, most Electron).
5. Prefer a known keyboard route; click only for a visible low-risk target.

Think in six verbs: `see`, `key`, `type`, `click`, `ax`, `script`. `desktop()` is the semantic snapshot that should come first.

```python
frame = oma.see("Spotify")
oma.key("ctrl+k", app="Spotify")
oma.type("Alessia Cara", app="Spotify")
oma.click(640, 420, app="Spotify")
print(oma.ax.dump("Spotify"))
oma.script("omarchy-notification-send done")
```

Do not add app-specific helpers. After a failed verified burst, switch mode or stop. Never repair uncertainty with repeated keys, clicks, or deletion loops.

## Invariants

- Typing goes to the focused window. Focus the target (`app=`) before `key`/`type`.
- The seat is shared. A cursor move can retarget keystrokes under focus-follows-mouse.
- Input is refused while the session is locked (`LockedError`) and refused on polkit dialogs (`TrustError`).
- `oma.click` needs `wlrctl`. `oma.see` needs `grim`. `oma.key`/`type` need `wtype`.
- Screenshots land under `$XDG_RUNTIME_DIR/omarchy-harness/` (newest 20 kept).
- Destructive `oma.script` (shutdown, reboot, pkg add/drop, close-all) requires `allow_destructive=True` only after the user confirmed by voice.
- No clipboard in v1.

Run `omarchy-harness doctor` to inspect tools without prompting.
