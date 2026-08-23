---
name: deepening
description: Guidelines for deepening a cluster of shallow modules safely based on dependency types.
---

# Deepening

How to safely collapse a cluster of shallow modules into one deep module. The dependency type dictates the seam and testing strategy.

## Dependency Categories

Assess the dependencies of the target module. This dictates how you test across the seam.

### 1. In-Process
Pure computation, in-memory state, zero I/O. 
*   **Action:** Always deepenable. Merge the modules. 
*   **Testing:** Test directly through the new interface. Zero adapters required.

### 2. Local-Substitutable
Dependencies with local test equivalents (e.g., Postgres database, Redis, local filesystem).
*   **Action:** Deepenable. The seam remains internal. 
*   **Testing:** Run the test suite against the local substitute (e.g., Laravel's `RefreshDatabase`, `Storage::fake()`). Do not extract a port/adapter for this at the module's external interface.

### 3. Remote but Owned (Ports & Adapters)
Internal APIs or microservices you control across a network.
*   **Action:** Define a **port** (interface) at the seam. The deep module owns the logic.
*   **Testing:** Inject an **adapter**. Implement an HTTP adapter for production, and an Array/In-Memory adapter for tests. 

### 4. True External (Mock)
Third-party services you do not control (e.g., Stripe, Cloudflare API).
*   **Action:** The deep module accepts the external dependency as an injected port.
*   **Testing:** Tests provide a Mock adapter.

## Seam Discipline

*   **The Single Adapter Fallacy:** One adapter = a hypothetical seam. YAGNI. Do not introduce a port unless you have at least two concrete adapters (usually Prod vs. Test).
*   **Internal vs. External Seams:** A deep module can have internal seams private to its implementation. Do not expose them through the public interface just because your tests want them. 

## Testing Strategy: Replace, Don't Layer

*   **Delete Waste:** Old unit tests tied to the previous shallow modules are now technical debt. Delete them.
*   **The Interface is the Test Surface:** Write new tests hitting only the deepened module's interface. 
*   **Assert Outcomes, Not State:** Assert on observable results (database records, returned DTOs, fired events). Do not assert on internal state or use reflection.
*   **Implementation Agnosticism:** If an internal refactor breaks your test, you tested past the interface. Fix the test.
