"""Customer notification workflow."""

from .audit import AuditEvent, AuditLog
from .domain import Order


class NotificationService:
    def __init__(self, audit_log: AuditLog) -> None:
        self.audit_log = audit_log

    def send_confirmation(self, order: Order, transaction_id: str) -> None:
        self.deliver(order.customer_email, f"Order {order.order_id} paid with {transaction_id}")
        self.audit_log.record(
            AuditEvent("confirmation.sent", order.order_id, {"recipient": order.customer_email})
        )

    def send_failure(self, order: Order, reason: str) -> None:
        self.deliver(order.customer_email, f"Order {order.order_id} failed: {reason}")
        self.audit_log.record(AuditEvent("checkout.failed", order.order_id, {"reason": reason}))

    def deliver(self, recipient: str, message: str) -> None:
        print(f"Sending to {recipient}: {message}")
