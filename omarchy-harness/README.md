# omarchy-harness

The Linux equivalent of [macos-harness](https://github.com/browser-use/macos-harness) for **Omarchy** (Hyprland + Quickshell). One persistent Python process, six verbs, no app-specific tools.

```bash
omarchy-harness <<'PY'
print(oma.desktop())
oma.see("kitty")
oma.type("echo perla", app="kitty")
oma.script("omarchy-theme-current")
PY
```

Requires an Omarchy session: `hyprctl`, `grim`, `wtype`, `busctl`. Clicks also need `wlrctl`.

```bash
uv pip install -e .
omarchy-harness doctor
```

This is not a sandbox. It can see your screen and type into focused windows. Treat a session like screen sharing. Plugins and this harness run as your user.

See `skills/omarchy-harness/SKILL.md` for the agent workflow.
