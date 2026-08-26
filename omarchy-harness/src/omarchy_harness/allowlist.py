"""Allowlisted omarchy/hyprctl commands for oma.script()."""

from __future__ import annotations

import shlex

ALLOWED_BINARIES = {
    "omarchy-launch-or-focus",
    "omarchy-launch-browser",
    "omarchy-launch-terminal",
    "omarchy-launch-spotify",
    "omarchy-launch-editor",
    "omarchy-launch-nautilus",
    "omarchy-theme-set",
    "omarchy-theme-list",
    "omarchy-theme-current",
    "omarchy-audio-output-volume",
    "omarchy-audio-output-switch",
    "omarchy-audio-input-mute",
    "omarchy-brightness-display",
    "omarchy-notification-send",
    "omarchy-capture-screenshot",
    "omarchy-plugin-list",
    "omarchy-menu",
    "omarchy-osd",
    "omarchy-hyprland-session-locked",
    "omarchy-hyprland-window-gaps-toggle",
    "omarchy-hyprland-window-pop",
    "omarchy-reminder",
}

DESTRUCTIVE_BINARIES = {
    "omarchy-system-shutdown",
    "omarchy-system-reboot",
    "omarchy-system-logout",
    "omarchy-hyprland-window-close-all",
    "omarchy-pkg-add",
    "omarchy-pkg-drop",
    "omarchy-pkg-remove",
    "omarchy-system-factory-reset",
}

HYPR_READ = {
    "clients",
    "workspaces",
    "activewindow",
    "monitors",
    "cursorpos",
    "layers",
    "devices",
    "version",
    "binds",
}

HYPR_DISPATCH = {
    "workspace",
    "focuswindow",
    "closewindow",
    "fullscreen",
    "movetoworkspace",
    "togglefloating",
    "movecursor",
    "cyclenext",
    "togglesplit",
}

SHELL_METHODS = {"ping", "summon", "hide", "toggle", "listPlugins"}

POLKIT_HINTS = ("polkit", "hyprpolkit", "lxqt-policykit", "polkit-gnome")


class ScriptError(ValueError):
    pass


def tokenize(command: str) -> list[str]:
    try:
        tokens = shlex.split(command, posix=True)
    except ValueError as e:
        raise ScriptError(str(e)) from e
    if not tokens:
        raise ScriptError("empty command")
    return tokens


def plan(command: str, *, allow_destructive: bool = False) -> tuple[str, list[str]]:
    tokens = tokenize(command)
    program, args = tokens[0], tokens[1:]
    if program in DESTRUCTIVE_BINARIES or (
        program == "omarchy" and args[:1] == ["pkg"]
    ):
        if not allow_destructive:
            raise ScriptError(
                f"'{program}' is destructive; pass allow_destructive=True only after a spoken yes"
            )
        return program, args
    if program == "hyprctl":
        _validate_hyprctl(args)
        return program, args
    if program == "omarchy-shell":
        _validate_shell(args)
        return program, args
    if program in ALLOWED_BINARIES:
        return program, args
    raise ScriptError(
        f"command '{program}' is not allowlisted — use oma.desktop(), oma.see(), or a listed omarchy binary"
    )


def _validate_hyprctl(args: list[str]) -> None:
    if not args:
        raise ScriptError("hyprctl needs a subcommand")
    i = 1 if args[0] == "-j" else 0
    if i >= len(args):
        raise ScriptError("hyprctl -j needs a subcommand")
    sub = args[i]
    if sub in HYPR_READ:
        return
    if sub == "dispatch":
        action = args[i + 1] if i + 1 < len(args) else ""
        if action in HYPR_DISPATCH:
            return
        raise ScriptError(f"hyprctl dispatch '{action}' is not allowlisted")
    raise ScriptError(f"hyprctl '{sub}' is not allowlisted")


def _validate_shell(args: list[str]) -> None:
    if args[:1] != ["shell"]:
        raise ScriptError("omarchy-shell only allows the shell IPC target")
    method = args[1] if len(args) > 1 else ""
    if method not in SHELL_METHODS:
        raise ScriptError(f"omarchy-shell shell '{method}' is not allowlisted")


def looks_like_polkit(class_or_title: str) -> bool:
    hay = (class_or_title or "").lower()
    return any(hint in hay for hint in POLKIT_HINTS)
