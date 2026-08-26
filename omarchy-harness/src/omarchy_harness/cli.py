from __future__ import annotations

import argparse
import sys
from pathlib import Path

from .hypr import which
from .oma import Oma, runtime_dir


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="omarchy-harness",
        description="Drive Omarchy/Hyprland with see/key/type/click/ax/script.",
    )
    parser.add_argument(
        "command",
        nargs="?",
        default="exec",
        choices=["exec", "doctor", "stop", "skill"],
        help="exec reads a Python program from stdin (default).",
    )
    args = parser.parse_args(argv)

    if args.command == "doctor":
        return doctor()
    if args.command == "stop":
        return stop()
    if args.command == "skill":
        skill = Path(__file__).resolve().parents[2] / "skills" / "omarchy-harness" / "SKILL.md"
        if not skill.is_file():
            skill = Path.home() / ".grok/skills/omarchy-harness/SKILL.md"
        print(skill)
        return 0 if skill.is_file() else 1
    return exec_stdin()


def exec_stdin() -> int:
    source = sys.stdin.read()
    if not source.strip():
        print("omarchy-harness: pass a Python program on stdin", file=sys.stderr)
        return 2
    oma = Oma()
    ns = {
        "oma": oma,
        "Path": Path,
        "subprocess": __import__("subprocess"),
    }
    try:
        exec(compile(source, "<stdin>", "exec"), ns, ns)  # noqa: S102 — the product is a trusted stdin program
    except SystemExit as e:
        return int(e.code or 0)
    return 0


def stop() -> int:
    path = runtime_dir() / "state.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text('{"driving": false, "pid": 0, "last": "stop"}\n')
    print("omarchy-harness: idle")
    return 0


def doctor() -> int:
    tools = {
        "hyprctl": which("hyprctl"),
        "grim": which("grim"),
        "wtype": which("wtype"),
        "busctl": which("busctl"),
        "wlrctl": which("wlrctl"),
        "omarchy-hyprland-session-locked": which("omarchy-hyprland-session-locked"),
    }
    display = bool(__import__("os").environ.get("WAYLAND_DISPLAY"))
    print("omarchy-harness doctor")
    print(f"  WAYLAND_DISPLAY: {'yes' if display else 'NO'}")
    missing = []
    for name, path in tools.items():
        optional = name in {"wlrctl", "omarchy-hyprland-session-locked"}
        mark = path or ("(optional, missing)" if optional else "MISSING")
        print(f"  {name}: {mark}")
        if path is None and not optional:
            missing.append(name)
    if missing:
        print("missing required tools:", ", ".join(missing))
        return 1
    print("ok")
    return 0
