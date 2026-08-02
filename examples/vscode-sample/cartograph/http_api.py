"""Transport adapter around the checkout application service."""

from dataclasses import asdict
from typing import Callable

from .checkout import CheckoutRequest, CheckoutService


def create_checkout_handler(service: CheckoutService) -> Callable[[CheckoutRequest], dict]:
    def checkout_handler(request: CheckoutRequest) -> dict:
        result = service.checkout(request)
        return {
            "status": 201 if result.accepted else 422,
            "body": asdict(result),
        }

    return checkout_handler
