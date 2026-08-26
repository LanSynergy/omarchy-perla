"""hyprctl JSON helpers. `run` is injectable so tests need no compositor."""

from __future__ import annotations

import json
import shutil
import subprocess
from typing import Any, Callable

Run = Callable[[list[str]], subprocess.CompletedProcess[str]]


def default_run(argv: list[str], timeout: float = 8.0) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        argv,
        check=False,
        text=True,
        capture_output=True,
        timeout=timeout,
    )


def which(name: str) -> str | None:
    return shutil.which(name)


def hypr_json(sub: str, run: Run = default_run) -> Any:
    proc = run(["hyprctl", "-j", sub])
    if proc.returncode != 0:
        raise RuntimeError((proc.stderr or proc.stdout or "hyprctl failed").strip())
    return json.loads(proc.stdout or "null")


def cursorpos(run: Run = default_run) -> dict[str, int]:
    proc = run(["hyprctl", "cursorpos"])
    text = (proc.stdout or "").strip().replace(" ", "")
    if "," in text:
        x, y = text.split(",", 1)
        try:
            return {"x": int(float(x)), "y": int(float(y))}
        except ValueError:
            pass
    return {"x": 0, "y": 0}


def compact_desktop(clients: Any, workspaces: Any, monitors: Any, active: Any, cursor: dict[str, int]) -> dict[str, Any]:
    windows = []
    for c in clients or []:
        if not isinstance(c, dict):
            continue
        at = c.get("at") or [0, 0]
        size = c.get("size") or [0, 0]
        windows.append(
            {
                "address": c.get("address"),
                "class": c.get("class"),
                "title": c.get("title"),
                "workspace": (c.get("workspace") or {}).get("id"),
                "mapped": bool(c.get("mapped", True)),
                "focus": bool(c.get("focusHistoryID") == 0),
                "geometry": {
                    "x": at[0] if len(at) > 0 else 0,
                    "y": at[1] if len(at) > 1 else 0,
                    "w": size[0] if len(size) > 0 else 0,
                    "h": size[1] if len(size) > 1 else 0,
                },
            }
        )
    return {
        "windows": windows,
        "workspaces": workspaces,
        "monitors": monitors,
        "active": active,
        "cursor": cursor,
    }


def find_window(desktop: dict[str, Any], app: str | None) -> dict[str, Any] | None:
    windows = desktop.get("windows") or []
    if not app:
        for w in windows:
            if w.get("focus"):
                return w
        return windows[0] if windows else None
    needle = app.lower()
    for w in windows:
        hay = f"{w.get('class') or ''} {w.get('title') or ''}".lower()
        if needle in hay:
            return w
    return None
