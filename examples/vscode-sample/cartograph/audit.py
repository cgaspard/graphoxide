"""Audit trail for important checkout outcomes."""

from dataclasses import dataclass


@dataclass(frozen=True)
class AuditEvent:
    name: str
    subject_id: str
    details: dict[str, str | int | bool]


class AuditLog:
    def record(self, event: AuditEvent) -> None:
        print(f"[audit] {event.name}: {event.subject_id} {event.details}")
