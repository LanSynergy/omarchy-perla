"""Best-effort AT-SPI dump via busctl. Missing a11y is a structured miss, not a crash."""

from __future__ import annotations




def dump(app: str | None, run=None) -> dict:
    from .hypr import default_run

    runner = run or default_run
    proc = runner(["busctl", "--user", "tree", "org.a11y.Bus"])
    if proc.returncode != 0:
        return {
            "available": False,
            "reason": (proc.stderr or proc.stdout or "AT-SPI bus not available").strip(),
            "app": app,
        }
    return {
        "available": True,
        "app": app,
        "note": "AT-SPI bus is up. Full named-control targeting is best-effort; fall back to oma.see() for canvas/Electron/terminals.",
        "tree_sample": (proc.stdout or "")[:1500],
    }


def query(app: str | None, run=None, **filters) -> dict:
    data = dump(app, run=run)
    data["filters"] = {k: v for k, v in filters.items() if v is not None}
    return data
