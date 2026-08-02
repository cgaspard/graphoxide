"""Payment gateway port and demo implementation."""

from dataclasses import dataclass
from typing import Protocol


@dataclass(frozen=True)
class PaymentReceipt:
    transaction_id: str
    accepted: bool


class PaymentGateway(Protocol):
    def charge(self, order_id: str, amount: int) -> PaymentReceipt: ...


class DemoPaymentGateway:
    def charge(self, order_id: str, amount: int) -> PaymentReceipt:
        return PaymentReceipt(
            transaction_id=f"demo-{order_id}",
            accepted=0 < amount < 10_000,
        )
