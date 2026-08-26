from __future__ import annotations

import json
import os
import subprocess
import time
from pathlib import Path
from typing import Any, Callable

from . import ax as ax_mod
from .allowlist import ScriptError, looks_like_polkit, plan as allowlist_plan
from .hypr import compact_desktop, cursorpos, default_run, find_window, hypr_json, which

Run = Callable[[list[str]], subprocess.CompletedProcess[str]]


class LockedError(RuntimeError):
    pass


class TrustError(RuntimeError):
    pass


def runtime_dir() -> Path:
    base = os.environ.get("XDG_RUNTIME_DIR")
    if base:
        return Path(base) / "omarchy-harness"
    return Path("/tmp") / f"omarchy-harness-{os.environ.get('USER', 'user')}"


class Oma:
    def __init__(self, run: Run | None = None):
        self._run = run or default_run
        self.ax = Ax(self)

    def _exec(self, argv: list[str]) -> subprocess.CompletedProcess[str]:
        return self._run(argv)

    def _beacon(self, last: str, driving: bool) -> None:
        path = runtime_dir() / "state.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_suffix(".json.tmp")
        tmp.write_text(
            json.dumps(
                {"driving": driving, "pid": os.getpid(), "last": last},
                indent=2,
            )
            + "\n"
        )
        tmp.replace(path)

    def _act(self, last: str) -> None:
        self._beacon(last, True)

    def _idle(self, last: str) -> None:
        self._beacon(last, False)

    def desktop(self) -> dict[str, Any]:
        clients = hypr_json("clients", self._run)
        workspaces = hypr_json("workspaces", self._run)
        monitors = hypr_json("monitors", self._run)
        try:
            active = hypr_json("activewindow", self._run)
        except Exception:
            active = None
        cursor = cursorpos(self._run)
        return compact_desktop(clients, workspaces, monitors, active, cursor)

    def see(
        self,
        app: str | None = None,
        scale: float | None = None,
        quality: int = 72,
    ) -> dict[str, Any]:
        """Capture a window as a scaled JPEG.

        A full-resolution PNG is the wrong trade for a vision model: it
        downsamples the image anyway, so the extra pixels buy nothing while
        costing upload time and image tokens. grim can scale and encode JPEG
        during capture, which is cheaper than doing either afterwards. The
        scale used is returned so callers can map image coordinates back to
        real screen pixels.
        """
        desktop = self.desktop()
        win = find_window(desktop, app)
        if not win:
            raise RuntimeError(f"no window matching {app!r}")
        geo = win["geometry"]
        region = f"{geo['x']},{geo['y']} {geo['w']}x{geo['h']}"
        dest_dir = runtime_dir()
        dest_dir.mkdir(parents=True, exist_ok=True)
        if scale is None:
            longest = max(int(geo["w"] or 0), int(geo["h"] or 0)) or 1
            # ~1024px on the long edge keeps UI text legible while cutting a
            # 1080p window to roughly a tenth of the bytes.
            scale = min(1.0, 1024 / longest)
        dest = dest_dir / f"see-{int(time.time() * 1000)}.jpg"
        proc = self._exec([
            "grim", "-g", region,
            "-s", f"{scale:.4f}",
            "-t", "jpeg", "-q", str(int(quality)),
            str(dest),
        ])
        if proc.returncode != 0:
            raise RuntimeError((proc.stderr or proc.stdout or "grim failed").strip())
        self._prune_shots(dest_dir)
        return {
            "path": str(dest),
            "geometry": geo,
            "scale": scale,
            "capture_scale": scale,
            "format": "jpeg",
            "app": win.get("class"),
            "address": win.get("address"),
            "title": win.get("title"),
        }

    def key(self, combo: str, app: str | None = None) -> None:
        self._prepare_input(app)
        argv = ["wtype", *combo_to_wtype(combo)]
        proc = self._exec(argv)
        if proc.returncode != 0:
            raise RuntimeError((proc.stderr or proc.stdout or "wtype failed").strip())
        self._idle("key")

    def type(self, text: str, app: str | None = None) -> None:
        self._prepare_input(app)
        proc = self._exec(["wtype", "--", text])
        if proc.returncode != 0:
            raise RuntimeError((proc.stderr or proc.stdout or "wtype failed").strip())
        self._idle("type")

    def _move_cursor(self, x: int, y: int) -> None:
        """Put the pointer at an absolute screen position.

        Omarchy 4 configures Hyprland in Lua, and `hyprctl dispatch` wraps its
        argument in `hl.dispatch(...)` — so the old `dispatch movecursor X Y`
        form is no longer valid Lua and fails every time. The Lua dispatcher
        wants a table. Upstream Hyprland still takes the plain form, so try the
        Lua one first and fall back rather than pinning to either.
        """
        attempts = [
            ["hyprctl", "dispatch", f"hl.dsp.cursor.move({{x = {int(x)}, y = {int(y)}}})"],
            ["hyprctl", "dispatch", "movecursor", str(int(x)), str(int(y))],
        ]
        last = None
        for argv in attempts:
            proc = self._exec(argv)
            if proc.returncode == 0:
                return
            last = proc
        raise RuntimeError(
            (last.stderr or last.stdout or "movecursor failed").strip() if last else "movecursor failed"
        )

    def click(self, x: int, y: int, app: str | None = None, button: str = "left") -> None:
        self._prepare_input(app)
        self._move_cursor(x, y)
        if not which("wlrctl"):
            raise RuntimeError("click needs wlrctl (pacman -S wlrctl); cursor was moved")
        btn = {"left": "left", "right": "right", "middle": "middle"}.get(button, button)
        proc = self._exec(["wlrctl", "pointer", "click", btn])
        if proc.returncode != 0:
            raise RuntimeError((proc.stderr or proc.stdout or "wlrctl click failed").strip())
        self._idle("click")

    def script(self, command: str, allow_destructive: bool = False) -> dict[str, Any]:
        program, args = allowlist_plan(command, allow_destructive=allow_destructive)
        self._act("script")
        try:
            proc = self._exec([program, *args])
        finally:
            self._idle("script")
        if proc.returncode != 0:
            raise RuntimeError((proc.stderr or proc.stdout or "command failed").strip())
        return {"ok": True, "stdout": (proc.stdout or "").strip()[:4000]}

    def _prepare_input(self, app: str | None) -> None:
        if self._session_locked():
            raise LockedError("session is locked; refusing keyboard/pointer")
        desktop = self.desktop()
        win = find_window(desktop, app) if app else find_window(desktop, None)
        if win:
            hay = f"{win.get('class') or ''} {win.get('title') or ''}"
            if looks_like_polkit(hay):
                raise TrustError("refusing to drive a polkit/auth dialog")
            addr = win.get("address")
            if addr:
                self._exec(["hyprctl", "dispatch", "focuswindow", f"address:{addr}"])
        self._act("input")

    def _session_locked(self) -> bool:
        locked_bin = which("omarchy-hyprland-session-locked")
        if locked_bin:
            proc = self._exec([locked_bin])
            return proc.returncode == 0
        return False

    def _prune_shots(self, dest_dir: Path, keep: int = 20) -> None:
        shots = sorted(dest_dir.glob("see-*.png"), key=lambda p: p.stat().st_mtime, reverse=True)
        for old in shots[keep:]:
            try:
                old.unlink()
            except OSError:
                pass


class Ax:
    def __init__(self, oma: Oma):
        self._oma = oma

    def dump(self, app: str | None = None) -> dict:
        return ax_mod.dump(app, run=self._oma._run)

    def query(self, app: str | None = None, **filters) -> dict:
        return ax_mod.query(app, run=self._oma._run, **filters)


SPECIAL = {
    "esc": "Escape",
    "escape": "Escape",
    "return": "Return",
    "enter": "Return",
    "tab": "Tab",
    "space": "space",
    "backspace": "BackSpace",
    "delete": "Delete",
    "up": "Up",
    "down": "Down",
    "left": "Left",
    "right": "Right",
}


def combo_to_wtype(combo: str) -> list[str]:
    parts = [p.strip().lower() for p in combo.replace("-", "+").split("+") if p.strip()]
    if not parts:
        raise ValueError("empty key combo")
    mods = []
    key = parts[-1]
    for part in parts[:-1]:
        if part in {"ctrl", "control"}:
            mods.append("ctrl")
        elif part in {"alt", "mod1"}:
            mods.append("alt")
        elif part in {"shift"}:
            mods.append("shift")
        elif part in {"super", "meta", "win", "mod4"}:
            mods.append("logo")
        else:
            raise ValueError(f"unknown modifier '{part}'")
    argv: list[str] = []
    for m in mods:
        argv.extend(["-M", m])
    wkey = SPECIAL.get(key, key)
    argv.extend(["-k", wkey])
    for m in reversed(mods):
        argv.extend(["-m", m])
    return argv
