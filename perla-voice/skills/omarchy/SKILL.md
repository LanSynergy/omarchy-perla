# Omarchy

How the desktop Perla lives on actually works. Read this before answering "how
do I…" or guessing a command name.

Omarchy is an opinionated **Arch Linux** desktop on **Hyprland**, a tiling
Wayland compositor. It is not a distro-with-a-theme: the shell, the menu, the
notifications and the theming are its own, and almost all of it is reachable as
commands.

## The one thing to remember

**Everything is a command.** `$OMARCHY_PATH/bin` holds ~429 `omarchy-*`
scripts, and `omarchy` is a router over them:

```
omarchy theme set "Tokyo Night"     →  omarchy-theme-set "Tokyo Night"
omarchy update                      →  omarchy-update
omarchy plugin list                 →  omarchy-plugin-list
```

Every script carries metadata in comments, which is why the router can list and
check them:

```
# omarchy:summary=Apply an Omarchy theme
# omarchy:group=theme
# omarchy:args=<theme-name>
# omarchy:examples=omarchy theme list | omarchy theme set "Tokyo Night"
# omarchy:requires-sudo=true
```

`omarchy commands --json` dumps all of it. **Never invent a command name** —
call `omarchy_help` and use what comes back.

## Layout

| Path | What |
|---|---|
| `$OMARCHY_PATH/bin` | the ~429 commands |
| `$OMARCHY_PATH/default/` | packaged defaults (hypr, bash, themes) |
| `$OMARCHY_PATH/shell/` | the Quickshell/QML shell — bar, menu, panels |
| `$OMARCHY_PATH/docs/` | how the machinery works |
| `$OMARCHY_PATH/manual/` | the user manual |
| `~/.config/omarchy/` | user config: `shell.json`, `plugins/`, themes |
| `~/.config/hypr/` | user Hyprland config (Lua) |

`OMARCHY_PATH` is normally `/usr/share/omarchy`; on a dev machine
`/etc/omarchy.conf` points it at a git checkout instead.

**Never write inside `$OMARCHY_PATH`.** It is package-owned and an update
overwrites it. User changes belong in `~/.config/`.

## Config is Lua, not INI

Hyprland config here is Lua. A binding is:

```lua
o.bind("SUPER + SHIFT + R", "SSH", "alacritty -e ssh your-server")
hl.unbind("SUPER + SPACE")          -- drop a packaged binding first
hl.monitor({ output = "DP-2", mode = "2560x1440@144", position = "0x0", scale = 1 })
```

Plain `bind = SUPER, K, exec, …` lines are the *upstream Hyprland* syntax and
do **not** belong in a `.lua` file here. Key names are uppercase:
`SUPER + BACKSPACE`, `SUPER + ALT + SPACE`.

## The shell

The bar, menu, notifications and panels are one **Quickshell** process running
QML. Drive it over IPC:

```
omarchy-shell shell ping
omarchy-shell shell rescanPlugins
omarchy-shell shell toggle omarchy.menu '{"menu":"root"}'
```

Bar items are plugins with ids (`omarchy.clock`, `omarchy.tray`). Third-party
plugins live in `~/.config/omarchy/plugins/<vendor>.<name>/` with a
`manifest.json`; `omarchy plugin list|enable|disable|validate` manages them.
Layout and order live in `~/.config/omarchy/shell.json`.

Editing plugin QML? `rescanPlugins` only re-reads the registry — a plugin with
`keepLoaded: true` keeps its old QML in memory. **`omarchy-restart-shell`** is
what actually reloads it, in place, without starting a second Quickshell.

## Themes

Themes are system-wide: one switch restyles terminal, editor, shell, borders
and wallpaper together.

```
omarchy theme list
omarchy theme set "Tokyo Night"
omarchy theme next
```

Backgrounds are per-theme, under `themes/<name>/backgrounds/`, and are `.webp`
— Qt needs `qt6-imageformats` installed or they silently fail to decode.

## Updating

```
omarchy update            # packages + migrations, with a snapshot when available
omarchy update-available  # is there anything?
```

On a normal install Omarchy itself arrives as the `omarchy` /
`omarchy-settings` pacman packages. On a dev-linked box `omarchy update` also
does `git pull --ff-only` on the checkout — but only `$OMARCHY_PATH`-resolved
trees (`bin/`, `shell/`, `themes/`, `config/`) come down that way. Anything the
`omarchy-settings` package installs at fixed system paths (`/etc`, systemd
units, `/etc/skel`) does not.

## Keys worth knowing

| Key | Does |
|---|---|
| `SUPER + K` | hotkey cheatsheet — the answer to "what are the shortcuts" |
| `SUPER + SPACE` | Omarchy menu (apps, settings, everything) |
| `SUPER + RETURN` | terminal |
| `SUPER + W` | close window |
| `SUPER + 1..9` | switch workspace |

`SUPER` is the modifier for nearly everything. When unsure, tell the user to
press `SUPER + K`.

## Answering questions about Omarchy

1. Call `omarchy_help` with the topic. It returns real commands (name, args,
   examples) and excerpts from `docs/` and `manual/` **on this machine**.
2. Answer from that. If nothing matched, say so rather than inventing.
3. To *do* the thing, prefer in this order:
   `launch_or_focus` → `omarchy_run` → `summon` → `type_text` / `press_key` →
   `click_at`. Clicking is the last resort; a command is exact, a click is a
   guess about pixels.
