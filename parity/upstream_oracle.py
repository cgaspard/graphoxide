"""Purity checks shared by the pinned Graphify oracle runners."""

from __future__ import annotations

from pathlib import Path, PurePosixPath
from typing import Iterable


# These trees contain Python that the Graphify CLI or its pytest inventory may
# execute. Ignored environments and top-level tool caches are intentionally not
# included: the oracle runners use an isolated, non-editable uv environment.
EXECUTABLE_SOURCE_ROOTS = frozenset({"graphify", "tests", "tools"})


def _is_valid_bytecode_cache(checkout: Path, relative: PurePosixPath) -> bool:
    """Allow only ordinary cache files backed by a real source module."""
    if (
        len(relative.parts) < 3
        or relative.parent.name != "__pycache__"
        or relative.suffix.casefold() != ".pyc"
    ):
        return False
    module = relative.name.split(".", 1)[0]
    if not module:
        return False
    source = checkout.joinpath(*relative.parent.parent.parts, f"{module}.py")
    cache = checkout.joinpath(*relative.parts)
    return (
        source.is_file()
        and not source.is_symlink()
        and cache.is_file()
        and not cache.is_symlink()
    )


def ignored_executable_artifact(
    checkout: Path, ignored_paths: Iterable[str]
) -> str | None:
    """Return the first ignored file capable of contaminating oracle source."""
    checkout = checkout.resolve()
    for raw in sorted(path for path in ignored_paths if path):
        relative = PurePosixPath(raw)
        if (
            relative.is_absolute()
            or ".." in relative.parts
            or not relative.parts
            or relative.parts[0] not in EXECUTABLE_SOURCE_ROOTS
        ):
            continue
        if _is_valid_bytecode_cache(checkout, relative):
            continue
        return relative.as_posix()
    return None
