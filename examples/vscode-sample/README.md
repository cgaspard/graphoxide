# Cartograph sample store

This compact, dependency-free Python application models an order checkout flow. It is the bundled development fixture for the Graphoxide VS Code extension—small enough to understand at a glance, but connected enough to exercise communities, call paths, impact analysis, and source navigation.

## Architecture

```text
HTTP handler → CheckoutService → InventoryRepository
                               → PaymentGateway
                               → OrderRepository
                               → NotificationService → AuditLog
```

The `CheckoutService` coordinates the main workflow. A failed payment releases reserved inventory; a successful payment saves the order and sends a confirmation. The HTTP handler is the outer entry point and `cartograph/main.py` is the composition root.

## Try it in the extension

From the Graphoxide development repository, press `F5` and select **Graphoxide: Run VS Code Extension**. The pre-launch task builds Graphoxide, validates the extension, extracts this project, and opens it in an Extension Development Host.

Good graph exercises:

- Query: `How does checkout handle a failed payment?`
- Query: `What happens after an order is saved?`
- Explain: `cartograph_checkout_checkoutservice_checkout`
- Path: `cartograph_http_api_create_checkout_handler` → `cartograph_inventory_inventoryrepository_release`
- Affected: `cartograph_checkout_checkoutservice_reject`
- Architectural hubs: `CheckoutService`, `Order`, and `NotificationService`
- Open `cartograph/checkout.py` to exercise Graphoxide CodeLens and source reveal.
- Start watch mode, edit a method, save, and watch the graph refresh.
- Generate the architecture report or export an interactive HTML graph.

`graphoxide-out/` is deliberately ignored. The extension-host pre-launch task regenerates it from source every time.
