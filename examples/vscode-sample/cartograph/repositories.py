"""Order persistence ports and an in-memory adapter."""

from typing import Protocol

from .domain import Order


class OrderRepository(Protocol):
    def save(self, order: Order) -> None: ...

    def find(self, order_id: str) -> Order | None: ...


class InMemoryOrderRepository:
    def __init__(self) -> None:
        self.orders: dict[str, Order] = {}

    def save(self, order: Order) -> None:
        self.orders[order.order_id] = order

    def find(self, order_id: str) -> Order | None:
        return self.orders.get(order_id)
