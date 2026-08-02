"""Inventory reservation and compensation operations."""

from .domain import LineItem


class InventoryRepository:
    def __init__(self) -> None:
        self.available = {"keyboard": 12, "mouse": 30}

    def reserve(self, items: list[LineItem]) -> bool:
        if not all(self.has_stock(item) for item in items):
            return False
        for item in items:
            self.available[item.sku] = self.available.get(item.sku, 0) - item.quantity
        return True

    def release(self, items: list[LineItem]) -> None:
        for item in items:
            self.available[item.sku] = self.available.get(item.sku, 0) + item.quantity

    def has_stock(self, item: LineItem) -> bool:
        return self.available.get(item.sku, 0) >= item.quantity
