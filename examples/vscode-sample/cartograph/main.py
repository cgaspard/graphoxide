"""Composition root for the sample store."""

from .audit import AuditLog
from .checkout import CheckoutService
from .http_api import create_checkout_handler
from .inventory import InventoryRepository
from .notifications import NotificationService
from .payments import DemoPaymentGateway
from .repositories import InMemoryOrderRepository


def build_checkout_handler():
    service = CheckoutService(
        orders=InMemoryOrderRepository(),
        inventory=InventoryRepository(),
        payments=DemoPaymentGateway(),
        notifications=NotificationService(AuditLog()),
    )
    return create_checkout_handler(service)


handle_checkout = build_checkout_handler()
