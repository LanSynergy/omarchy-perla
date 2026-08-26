"""Omarchy computer-use harness — six verbs, one process."""

from .oma import LockedError, TrustError, Oma

__all__ = ["Oma", "LockedError", "TrustError"]
