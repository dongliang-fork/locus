"""Managed Git worktrees and reusable, private Unity project slots."""
from __future__ import annotations

import os
from dataclasses import dataclass
from typing import Any, TYPE_CHECKING

from ._models import WorkspaceRef
from ._scope import workspace_payload

if TYPE_CHECKING:
    from ._client import Client


@dataclass(frozen=True, slots=True)
class Worktree:
    checkout_id: str
    project_id: str
    root: str
    repo_root: str
    project_relative_path: str
    branch: str | None
    head_oid: str
    materialization_epoch: int
    managed: bool
    lifecycle: str
    dirty: bool
    pool_slot: bool
    assignment_id: str | None
    editor_version: str | None
    last_error: str | None
    workspace_ref: WorkspaceRef

    @classmethod
    def from_payload(cls, row: dict[str, Any]) -> Worktree:
        return cls(checkout_id=row["checkoutId"], project_id=row["projectId"],
            root=row["root"], repo_root=row["repoRoot"], project_relative_path=row["projectRelativePath"],
            branch=row.get("branch"), head_oid=row["headOid"], materialization_epoch=row["materializationEpoch"],
            managed=row["managed"], lifecycle=row["lifecycle"], dirty=row["dirty"], pool_slot=row["poolSlot"],
            assignment_id=row.get("assignmentId"), editor_version=row.get("editorVersion"),
            last_error=row.get("lastError"), workspace_ref=WorkspaceRef.from_payload(row["workspaceRef"]))


@dataclass(frozen=True, slots=True)
class PoolAcquisition:
    worktree: Worktree
    reused: bool
    preserved_library: bool


@dataclass(frozen=True, slots=True)
class WorktreeDefaults:
    """Where omitted directories go for the source checkout.

    ``root_source`` is ``"process"`` (``--locus-worktree-root`` / ``LOCUS_WORKTREE_ROOT``),
    ``"settings"`` (the persisted setting) or ``"repository"`` (beside the main repository).
    """
    container: str
    root_source: str
    max_slots: int | None


def _optional_directory(value: str | None, name: str) -> str | None:
    if value is not None and not value.strip():
        raise ValueError(f"{name} cannot be empty; omit it to use the default directory")
    return value


class Worktrees:
    def __init__(self, client: Client) -> None:
        self._client = client

    async def _call(self, action: str, params: dict[str, Any] | None = None, *, workspace_ref: Any = None) -> Any:
        return await self._client.rpc("worktrees." + action, {
            "workspaceRef": workspace_payload(workspace_ref),
            "executionDelegation": os.environ.get("LOCUS_SDK_EXECUTION_DELEGATION"),
            **(params or {}),
        }, timeout=300)

    async def list(self, *, workspace_ref: Any = None) -> list[Worktree]:
        """List recorded siblings, including idle/removed slots, without starting services."""
        return [Worktree.from_payload(row) for row in await self._call("list", workspace_ref=workspace_ref)]

    async def get(self, checkout_id: str, *, workspace_ref: Any = None) -> Worktree:
        """Explicitly resolve a current sibling and register its active runtime."""
        if not checkout_id.strip():
            raise ValueError("checkout_id cannot be empty")
        return Worktree.from_payload(await self._call("get", {"checkoutId": checkout_id.strip()}, workspace_ref=workspace_ref))

    async def defaults(self, *, workspace_ref: Any = None) -> WorktreeDefaults:
        """Resolve the container and pool capacity used when arguments are omitted."""
        row = await self._call("defaults", workspace_ref=workspace_ref)
        return WorktreeDefaults(row["container"], row["rootSource"], row["maxSlots"])

    async def create(self, *, branch: str, destination: str | None = None, start_ref: str | None = None,
        include_dirty: bool = False, workspace_ref: Any = None) -> Worktree:
        """Create a managed worktree. Omit ``destination`` to use a new directory in the default container."""
        if not branch.strip():
            raise ValueError("branch cannot be empty")
        return Worktree.from_payload(await self._call("create", {
            "destination": _optional_directory(destination, "destination"), "branch": branch,
            "startRef": start_ref, "includeDirty": include_dirty,
        }, workspace_ref=workspace_ref))

    async def discover(self, *, workspace_ref: Any = None) -> list[str]:
        return await self._call("discover", workspace_ref=workspace_ref)

    async def import_worktree(self, target_root: str, *, workspace_ref: Any = None) -> Worktree:
        if not target_root.strip():
            raise ValueError("target_root cannot be empty")
        return Worktree.from_payload(await self._call("import", {"targetRoot": target_root}, workspace_ref=workspace_ref))

    async def remove(self, worktree: Worktree | WorkspaceRef, *, workspace_ref: Any = None) -> None:
        """Remove a clean, closed, unassigned managed checkout; never force-delete user work."""
        await self._call("remove", {"worktree": workspace_payload(worktree)}, workspace_ref=workspace_ref)

    async def operations(self, *, workspace_ref: Any = None) -> list[dict[str, Any]]:
        return await self._call("operations", workspace_ref=workspace_ref)

    async def acquire(self, *, commit: str, pool_root: str | None = None, max_slots: int | None = None,
        branch: str | None = None, assignment_id: str | None = None, workspace_ref: Any = None) -> PoolAcquisition:
        """Assign a private Unity slot at an exact commit; reuse compatible idle Library.

        Omit ``pool_root`` to reuse a released compatible slot where it lives, or else
        use the default container. Omit ``max_slots`` to use the Unity Editor limit.
        """
        if not commit.strip():
            raise ValueError("commit cannot be empty")
        if max_slots is not None and (isinstance(max_slots, bool) or not isinstance(max_slots, int) or max_slots <= 0):
            raise ValueError("max_slots must be a positive integer")
        row = await self._call("acquire", {"poolRoot": _optional_directory(pool_root, "pool_root"), "commit": commit,
            "maxSlots": max_slots, "branch": branch, "assignmentId": assignment_id}, workspace_ref=workspace_ref)
        return PoolAcquisition(Worktree.from_payload(row["worktree"]), row["reused"], row["preservedLibrary"])

    async def release(self, worktree: Worktree, *, workspace_ref: Any = None) -> Worktree:
        """Return the exact assignment after its editor is closed and checkout is clean."""
        if not isinstance(worktree, Worktree) or not worktree.assignment_id:
            raise ValueError("release requires the assigned Worktree returned by acquire")
        return Worktree.from_payload(await self._call("release", {
            "worktree": workspace_payload(worktree), "assignmentId": worktree.assignment_id,
        }, workspace_ref=workspace_ref))
