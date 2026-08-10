(function installGraphoxideConceptFixture(global) {
  'use strict';

  const rawNodes = [
    { id: 'http-router', label: 'HTTP Router', file: 'cartograph/http_api.py', location: 'L18', kind: 'code', community: 'gateway', communityName: 'API Gateway' },
    { id: 'request-context', label: 'Request Context', file: 'cartograph/http/context.py', location: 'L11', kind: 'code', community: 'gateway', communityName: 'API Gateway' },
    { id: 'auth-middleware', label: 'Auth Middleware', file: 'cartograph/http/auth.py', location: 'L22', kind: 'code', community: 'gateway', communityName: 'API Gateway' },
    { id: 'checkout-controller', label: 'Checkout Controller', file: 'cartograph/http/checkout.py', location: 'L31', kind: 'code', community: 'gateway', communityName: 'API Gateway' },
    { id: 'catalog-controller', label: 'Catalog Controller', file: 'cartograph/http/catalog.py', location: 'L17', kind: 'code', community: 'gateway', communityName: 'API Gateway' },
    { id: 'health-controller', label: 'Health Controller', file: 'cartograph/http/health.py', location: 'L8', kind: 'code', community: 'gateway', communityName: 'API Gateway' },
    { id: 'error-mapper', label: 'Error Mapper', file: 'cartograph/http/errors.py', location: 'L14', kind: 'code', community: 'gateway', communityName: 'API Gateway' },

    { id: 'checkout-service', label: 'Checkout Service', file: 'cartograph/checkout.py', location: 'L42', kind: 'code', community: 'checkout', communityName: 'Checkout Core' },
    { id: 'cart', label: 'Cart', file: 'cartograph/domain.py', location: 'L18', kind: 'code', community: 'checkout', communityName: 'Checkout Core' },
    { id: 'order', label: 'Order', file: 'cartograph/domain.py', location: 'L67', kind: 'code', community: 'checkout', communityName: 'Checkout Core' },
    { id: 'pricing', label: 'Pricing Engine', file: 'cartograph/pricing.py', location: 'L26', kind: 'code', community: 'checkout', communityName: 'Checkout Core' },
    { id: 'tax-calculator', label: 'Tax Calculator', file: 'cartograph/pricing.py', location: 'L91', kind: 'code', community: 'checkout', communityName: 'Checkout Core' },
    { id: 'checkout-repository', label: 'Checkout Repository', file: 'cartograph/repositories.py', location: 'L21', kind: 'code', community: 'checkout', communityName: 'Checkout Core' },
    { id: 'checkout-validator', label: 'Checkout Validator', file: 'cartograph/checkout/validation.py', location: 'L16', kind: 'code', community: 'checkout', communityName: 'Checkout Core' },
    { id: 'idempotency', label: 'Idempotency Guard', file: 'cartograph/checkout/idempotency.py', location: 'L13', kind: 'code', community: 'checkout', communityName: 'Checkout Core' },

    { id: 'payment-orchestrator', label: 'Payment Orchestrator', file: 'cartograph/payments.py', location: 'L35', kind: 'code', community: 'payments', communityName: 'Payments' },
    { id: 'stripe-adapter', label: 'Stripe Adapter', file: 'cartograph/payments/stripe.py', location: 'L20', kind: 'code', community: 'payments', communityName: 'Payments' },
    { id: 'payment-intent', label: 'Payment Intent', file: 'cartograph/payments/models.py', location: 'L9', kind: 'code', community: 'payments', communityName: 'Payments' },
    { id: 'fraud-check', label: 'Fraud Check', file: 'cartograph/payments/fraud.py', location: 'L28', kind: 'code', community: 'payments', communityName: 'Payments' },
    { id: 'refund-service', label: 'Refund Service', file: 'cartograph/payments/refunds.py', location: 'L33', kind: 'code', community: 'payments', communityName: 'Payments' },
    { id: 'payment-webhook', label: 'Payment Webhook', file: 'cartograph/payments/webhook.py', location: 'L24', kind: 'code', community: 'payments', communityName: 'Payments' },
    { id: 'ledger', label: 'Payment Ledger', file: 'cartograph/payments/ledger.py', location: 'L12', kind: 'code', community: 'payments', communityName: 'Payments' },

    { id: 'inventory-service', label: 'Inventory Service', file: 'cartograph/inventory.py', location: 'L31', kind: 'code', community: 'inventory', communityName: 'Inventory' },
    { id: 'stock-reservation', label: 'Stock Reservation', file: 'cartograph/inventory/reservation.py', location: 'L19', kind: 'code', community: 'inventory', communityName: 'Inventory' },
    { id: 'warehouse-client', label: 'Warehouse Client', file: 'cartograph/inventory/warehouse.py', location: 'L27', kind: 'code', community: 'inventory', communityName: 'Inventory' },
    { id: 'inventory-repository', label: 'Inventory Repository', file: 'cartograph/repositories.py', location: 'L74', kind: 'code', community: 'inventory', communityName: 'Inventory' },
    { id: 'availability-policy', label: 'Availability Policy', file: 'cartograph/inventory/policy.py', location: 'L11', kind: 'code', community: 'inventory', communityName: 'Inventory' },
    { id: 'sku', label: 'SKU', file: 'cartograph/inventory/models.py', location: 'L7', kind: 'code', community: 'inventory', communityName: 'Inventory' },

    { id: 'notification-service', label: 'Notification Service', file: 'cartograph/notifications.py', location: 'L29', kind: 'code', community: 'notifications', communityName: 'Notifications' },
    { id: 'email-channel', label: 'Email Channel', file: 'cartograph/notifications/email.py', location: 'L18', kind: 'code', community: 'notifications', communityName: 'Notifications' },
    { id: 'sms-channel', label: 'SMS Channel', file: 'cartograph/notifications/sms.py', location: 'L16', kind: 'code', community: 'notifications', communityName: 'Notifications' },
    { id: 'receipt-template', label: 'Receipt Template', file: 'cartograph/notifications/templates.py', location: 'L14', kind: 'template', community: 'notifications', communityName: 'Notifications' },
    { id: 'event-consumer', label: 'Order Event Consumer', file: 'cartograph/notifications/consumer.py', location: 'L21', kind: 'code', community: 'notifications', communityName: 'Notifications' },
    { id: 'notification-repository', label: 'Delivery Repository', file: 'cartograph/notifications/repository.py', location: 'L18', kind: 'code', community: 'notifications', communityName: 'Notifications' },

    { id: 'event-bus', label: 'Domain Event Bus', file: 'cartograph/platform/events.py', location: 'L23', kind: 'code', community: 'platform', communityName: 'Platform Services' },
    { id: 'postgres', label: 'Postgres Store', file: 'cartograph/platform/database.py', location: 'L12', kind: 'database', community: 'platform', communityName: 'Platform Services' },
    { id: 'redis-cache', label: 'Redis Cache', file: 'cartograph/platform/cache.py', location: 'L10', kind: 'database', community: 'platform', communityName: 'Platform Services' },
    { id: 'config', label: 'Runtime Config', file: 'cartograph/platform/config.py', location: 'L9', kind: 'config', community: 'platform', communityName: 'Platform Services' },
    { id: 'telemetry', label: 'Telemetry', file: 'cartograph/platform/telemetry.py', location: 'L17', kind: 'code', community: 'platform', communityName: 'Platform Services' },
    { id: 'audit-log', label: 'Audit Log', file: 'cartograph/audit.py', location: 'L15', kind: 'code', community: 'platform', communityName: 'Platform Services' },
    { id: 'job-runner', label: 'Background Jobs', file: 'cartograph/platform/jobs.py', location: 'L28', kind: 'code', community: 'platform', communityName: 'Platform Services' },
    { id: 'domain-events', label: 'Domain Events', file: 'cartograph/platform/event_types.py', location: 'L7', kind: 'code', community: 'platform', communityName: 'Platform Services' },
  ];

  const edges = [
    { source: 'http-router', target: 'request-context', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'http-router', target: 'auth-middleware', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'http-router', target: 'checkout-controller', relation: 'routes_to', confidence: 'EXTRACTED' },
    { source: 'http-router', target: 'catalog-controller', relation: 'routes_to', confidence: 'EXTRACTED' },
    { source: 'http-router', target: 'health-controller', relation: 'routes_to', confidence: 'EXTRACTED' },
    { source: 'checkout-controller', target: 'checkout-service', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'checkout-controller', target: 'checkout-validator', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'checkout-controller', target: 'error-mapper', relation: 'imports', confidence: 'EXTRACTED' },
    { source: 'catalog-controller', target: 'inventory-service', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'catalog-controller', target: 'pricing', relation: 'calls', confidence: 'INFERRED' },
    { source: 'health-controller', target: 'postgres', relation: 'reads', confidence: 'INFERRED' },
    { source: 'health-controller', target: 'redis-cache', relation: 'reads', confidence: 'INFERRED' },
    { source: 'auth-middleware', target: 'redis-cache', relation: 'reads', confidence: 'EXTRACTED' },
    { source: 'request-context', target: 'telemetry', relation: 'calls', confidence: 'EXTRACTED' },

    { source: 'checkout-service', target: 'cart', relation: 'reads', confidence: 'EXTRACTED' },
    { source: 'checkout-service', target: 'order', relation: 'creates', confidence: 'EXTRACTED' },
    { source: 'checkout-service', target: 'pricing', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'checkout-service', target: 'tax-calculator', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'checkout-service', target: 'inventory-service', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'checkout-service', target: 'payment-orchestrator', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'checkout-service', target: 'checkout-repository', relation: 'writes', confidence: 'EXTRACTED' },
    { source: 'checkout-service', target: 'idempotency', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'checkout-service', target: 'event-bus', relation: 'publishes', confidence: 'EXTRACTED' },
    { source: 'checkout-service', target: 'audit-log', relation: 'calls', confidence: 'INFERRED' },
    { source: 'checkout-validator', target: 'cart', relation: 'reads', confidence: 'EXTRACTED' },
    { source: 'checkout-validator', target: 'availability-policy', relation: 'calls', confidence: 'INFERRED' },
    { source: 'pricing', target: 'tax-calculator', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'pricing', target: 'redis-cache', relation: 'reads', confidence: 'EXTRACTED' },
    { source: 'checkout-repository', target: 'postgres', relation: 'writes', confidence: 'EXTRACTED' },
    { source: 'idempotency', target: 'redis-cache', relation: 'writes', confidence: 'EXTRACTED' },
    { source: 'order', target: 'domain-events', relation: 'creates', confidence: 'EXTRACTED' },

    { source: 'payment-orchestrator', target: 'payment-intent', relation: 'creates', confidence: 'EXTRACTED' },
    { source: 'payment-orchestrator', target: 'fraud-check', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'payment-orchestrator', target: 'stripe-adapter', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'payment-orchestrator', target: 'ledger', relation: 'writes', confidence: 'EXTRACTED' },
    { source: 'payment-orchestrator', target: 'audit-log', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'stripe-adapter', target: 'config', relation: 'reads', confidence: 'EXTRACTED' },
    { source: 'stripe-adapter', target: 'telemetry', relation: 'calls', confidence: 'INFERRED' },
    { source: 'fraud-check', target: 'redis-cache', relation: 'reads', confidence: 'EXTRACTED' },
    { source: 'payment-webhook', target: 'payment-orchestrator', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'payment-webhook', target: 'event-bus', relation: 'publishes', confidence: 'EXTRACTED' },
    { source: 'refund-service', target: 'stripe-adapter', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'refund-service', target: 'ledger', relation: 'writes', confidence: 'EXTRACTED' },
    { source: 'ledger', target: 'postgres', relation: 'writes', confidence: 'EXTRACTED' },

    { source: 'inventory-service', target: 'availability-policy', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'inventory-service', target: 'inventory-repository', relation: 'reads', confidence: 'EXTRACTED' },
    { source: 'inventory-service', target: 'stock-reservation', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'inventory-service', target: 'warehouse-client', relation: 'calls', confidence: 'INFERRED' },
    { source: 'inventory-service', target: 'redis-cache', relation: 'reads', confidence: 'EXTRACTED' },
    { source: 'stock-reservation', target: 'sku', relation: 'reads', confidence: 'EXTRACTED' },
    { source: 'stock-reservation', target: 'inventory-repository', relation: 'writes', confidence: 'EXTRACTED' },
    { source: 'stock-reservation', target: 'event-bus', relation: 'publishes', confidence: 'EXTRACTED' },
    { source: 'warehouse-client', target: 'config', relation: 'reads', confidence: 'EXTRACTED' },
    { source: 'warehouse-client', target: 'telemetry', relation: 'calls', confidence: 'INFERRED' },
    { source: 'inventory-repository', target: 'postgres', relation: 'reads', confidence: 'EXTRACTED' },

    { source: 'event-bus', target: 'event-consumer', relation: 'dispatches', confidence: 'EXTRACTED' },
    { source: 'event-consumer', target: 'notification-service', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'notification-service', target: 'email-channel', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'notification-service', target: 'sms-channel', relation: 'calls', confidence: 'INFERRED' },
    { source: 'notification-service', target: 'receipt-template', relation: 'renders', confidence: 'EXTRACTED' },
    { source: 'notification-service', target: 'notification-repository', relation: 'writes', confidence: 'EXTRACTED' },
    { source: 'email-channel', target: 'config', relation: 'reads', confidence: 'EXTRACTED' },
    { source: 'sms-channel', target: 'config', relation: 'reads', confidence: 'EXTRACTED' },
    { source: 'notification-repository', target: 'postgres', relation: 'writes', confidence: 'EXTRACTED' },

    { source: 'event-bus', target: 'job-runner', relation: 'dispatches', confidence: 'INFERRED' },
    { source: 'event-bus', target: 'telemetry', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'job-runner', target: 'refund-service', relation: 'calls', confidence: 'AMBIGUOUS' },
    { source: 'job-runner', target: 'inventory-service', relation: 'calls', confidence: 'INFERRED' },
    { source: 'audit-log', target: 'postgres', relation: 'writes', confidence: 'EXTRACTED' },
    { source: 'telemetry', target: 'config', relation: 'reads', confidence: 'EXTRACTED' },
    { source: 'redis-cache', target: 'config', relation: 'reads', confidence: 'INFERRED' },
  ];

  const degreeById = new Map(rawNodes.map((node) => [node.id, 0]));
  for (const edge of edges) {
    degreeById.set(edge.source, (degreeById.get(edge.source) || 0) + 1);
    if (edge.target !== edge.source) {
      degreeById.set(edge.target, (degreeById.get(edge.target) || 0) + 1);
    }
  }

  const deepFreeze = (value) => {
    if (value && typeof value === 'object' && !Object.isFrozen(value)) {
      Object.freeze(value);
      for (const child of Object.values(value)) deepFreeze(child);
    }
    return value;
  };

  global.GRAPHOXIDE_GRAPH_FIXTURE = deepFreeze({
    contractVersion: 1,
    fixtureId: 'cartograph-checkout-v1',
    directed: true,
    builtAtCommit: '8f2c6f1',
    nodes: rawNodes.map((node) => ({ ...node, degree: degreeById.get(node.id) || 0 })),
    edges,
  });
})(globalThis);
