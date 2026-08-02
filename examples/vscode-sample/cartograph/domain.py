"""Order entities and business rules."""

from dataclasses import dataclass, field
from enum import Enum


class OrderStatus(str, Enum):
    PENDING = "pending"
    PAID = "paid"
    FAILED = "failed"


@dataclass(frozen=True)
class LineItem:
    sku: str
    quantity: int
    unit_price: int

    def subtotal(self) -> int:
        return self.quantity * self.unit_price


@dataclass
class Order:
    order_id: str
    customer_email: str
    items: list[LineItem]
    status: OrderStatus = field(default=OrderStatus.PENDING)

    def total(self) -> int:
        return sum(item.subtotal() for item in self.items)

    def mark_paid(self) -> None:
        self.status = OrderStatus.PAID

    def mark_failed(self) -> None:
        self.status = OrderStatus.FAILED
