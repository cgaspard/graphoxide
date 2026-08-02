"""Executable example of the successful checkout path."""

from cartograph.audit import AuditLog
from cartograph.checkout import CheckoutRequest, CheckoutService
from cartograph.domain import LineItem
from cartograph.inventory import InventoryRepository
from cartograph.notifications import NotificationService
from cartograph.payments import DemoPaymentGateway
from cartograph.repositories import InMemoryOrderRepository


def test_accepts_an_in_stock_order() -> None:
    service = CheckoutService(
        InMemoryOrderRepository(),
        InventoryRepository(),
        DemoPaymentGateway(),
        NotificationService(AuditLog()),
    )
    result = service.checkout(
        CheckoutRequest(
            order_id="order-42",
            customer_email="developer@example.test",
            items=[LineItem("keyboard", quantity=1, unit_price=125)],
        )
    )
    assert result.accepted is True
