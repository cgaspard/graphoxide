"""Application service that coordinates the checkout transaction."""

from dataclasses import dataclass

from .domain import LineItem, Order
from .inventory import InventoryRepository
from .notifications import NotificationService
from .payments import PaymentGateway
from .repositories import OrderRepository


@dataclass(frozen=True)
class CheckoutRequest:
    order_id: str
    customer_email: str
    items: list[LineItem]


@dataclass(frozen=True)
class CheckoutResult:
    order_id: str
    accepted: bool
    reason: str | None = None


class CheckoutService:
    def __init__(
        self,
        orders: OrderRepository,
        inventory: InventoryRepository,
        payments: PaymentGateway,
        notifications: NotificationService,
    ) -> None:
        self.orders = orders
        self.inventory = inventory
        self.payments = payments
        self.notifications = notifications

    def checkout(self, request: CheckoutRequest) -> CheckoutResult:
        order = Order(request.order_id, request.customer_email, request.items)
        if not self.inventory.reserve(order.items):
            return self.reject(order, "insufficient inventory")

        receipt = self.payments.charge(order.order_id, order.total())
        if not receipt.accepted:
            self.inventory.release(order.items)
            return self.reject(order, "payment declined")

        order.mark_paid()
        self.orders.save(order)
        self.notifications.send_confirmation(order, receipt.transaction_id)
        return CheckoutResult(order.order_id, accepted=True)

    def reject(self, order: Order, reason: str) -> CheckoutResult:
        order.mark_failed()
        self.orders.save(order)
        self.notifications.send_failure(order, reason)
        return CheckoutResult(order.order_id, accepted=False, reason=reason)
