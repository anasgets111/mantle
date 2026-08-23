---
name: codebase-design
description: Shared vocabulary for designing deep modules. Enforce Systems Thinking, leverage, locality, and YAGNI.
disable-model-invocation: true
---

# Codebase Design

Design **deep modules**: massive implementation hidden behind a minimal interface, placed at a clean seam, fully testable. Optimize for leverage (caller efficiency) and locality (maintainer efficiency).

## Architectural Glossary

Use these exact terms. Do not substitute with "component," "service," "API," or "boundary."

| Term | Strict Definition |
| :--- | :--- |
| **Module** | Anything with an interface and implementation (function, class, package). |
| **Interface** | Everything a caller must know: type signature, invariants, ordering, errors, config. |
| **Implementation** | The internal body of code hiding behind the interface. |
| **Depth** | Leverage at the interface. High depth = minimal interface + massive implementation. |
| **Seam** | The location where a module's interface lives (where behavior can be swapped). |
| **Adapter** | The concrete logic that satisfies an interface at a seam (e.g., Postgres Repo, Stripe API). |
| **Leverage** | Caller benefit: capabilities gained per unit of interface learned. |
| **Locality** | Maintainer benefit: bugs, logic, and tests concentrate in one place. Fix once. |

## Deep vs. Shallow

**Deep module** = small interface + deep implementation. High leverage.

```text
┌─────────────────────┐
│   Small Interface   │ ← Minimal methods/params (e.g., `ChargeCard()`)
├─────────────────────┤
│                     │
│ Deep Implementation │ ← Retries, logging, payload mapping hidden
│                     │
└─────────────────────┘
```

**Shallow module** = large interface + thin implementation. A useless pass-through. Delete it.

```text
┌─────────────────────────────────┐
│        Large Interface          │ ← Requires setting up 5 DTOs
├─────────────────────────────────┤
│       Thin Implementation       │ ← Just calls a Laravel Facade
└─────────────────────────────────┘
```

## Lazy Senior Dev Principles

*   **The Deletion Test (YAGNI):** If you delete a module and complexity vanishes, it was a useless pass-through. If complexity explodes across N callers, it was earning its keep.
*   **Depth belongs to the interface.** A deep module can use small, swappable internal classes. Do not expose them.
*   **The interface is the test surface.** If you have to test past the interface (mocking internal state), the module is the wrong shape.
*   **Seam Discipline:** One adapter = a hypothetical seam (YAGNI violation). Two adapters = a real seam (e.g., HTTP in prod, Array in tests). Do not introduce a seam unless it varies.

## Testability via Interface

**1. Accept dependencies, do not instantiate them.**
```php
// Testable
public function processOrder(Order $order, PaymentGateway $gateway) {}

// Garbage (Hard to test, hidden coupling)
public function processOrder(Order $order) {
    $gateway = new StripeGateway(); 
}
```

**2. Return results, avoid hidden state mutations.**
```php
// Testable
public function calculateDiscount(Cart $cart): int {}

// Garbage (Hidden side effects)
public function applyDiscount(Cart $cart): void {
    $cart->total -= $this->discount;
}
```

## Going Deeper
*   **Deepening a module:** Call `deepening`.
*   **Alternative interface architectures:** Call `design-it-twice`.
