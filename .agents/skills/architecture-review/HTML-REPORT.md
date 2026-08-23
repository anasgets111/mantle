# HTML Report Template

The architecture review is a single, self-contained HTML file written to `/tmp`. It uses Tailwind and Mermaid via CDNs. Mix Mermaid (for call graphs) with hand-built HTML/SVG (for layered mass diagrams) to accurately represent module depth. 

## 1. HTML Scaffold

```html
<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <title>Architecture Review — {{repo name}}</title>
    <script src="[https://cdn.tailwindcss.com](https://cdn.tailwindcss.com)"></script>
    <script type="module">
      import mermaid from "[https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.esm.min.mjs](https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.esm.min.mjs)";
      mermaid.initialize({ startOnLoad: true, theme: "neutral", securityLevel: "loose" });
    </script>
    <style>
      /* Custom edge/depth styles */
      .seam { stroke-dasharray: 4 4; }
      .leak { stroke: #dc2626; stroke-width: 2px; }
      .deep { background: linear-gradient(135deg, #0f172a, #1e293b); color: white; }
    </style>
  </head>
  <body class="bg-stone-50 text-slate-900 font-sans">
    <main class="max-w-5xl mx-auto px-6 py-12 space-y-12">
      <header>
        <h1 class="text-2xl font-bold">{{repo name}} / Architecture Review</h1>
        <p class="text-sm text-slate-500">Generated {{date}}. Legend: Solid = Module, Dashed = Seam, Red = Leak, Dark = Deep Module.</p>
      </header>
      
      <section id="candidates" class="space-y-10">
        <!-- Render <article> candidate cards here -->
      </section>
      
      <section id="top-recommendation" class="mt-12 p-6 bg-white border border-slate-200 rounded-lg">
        <!-- Render top recommendation here -->
      </section>
    </main>
  </body>
</html>
```

## 2. Candidate Card Layout

Render one `<article>` per refactor target. Zero fluff. 

*   **Title:** Actionable command (e.g., "Collapse the Order Pipeline").
*   **Badges:** Recommendation (`Strong` [emerald], `Worth exploring` [amber], `Speculative` [slate]) + Dependency Type.
*   **Files:** Monospaced list of impacted files.
*   **Before / After Diagram:** Side-by-side visual core (see Diagram Patterns below).
*   **Problem:** 1 sentence. State the root cause of the friction.
*   **Solution:** 1 sentence. State the exact architectural change.
*   **Wins:** Bullet points (≤6 words each). Must use strict vocabulary.
*   **ADR Warning (Optional):** 1-line amber callout if contradicting an existing ADR.

## 3. Diagram Patterns

Pick the tool that best exposes the architectural flaw. 

**Mermaid Graph (Dependencies / Call Flow)**
Use for deep call stacks or leaked abstractions across seams. 

```html
<div class="rounded-lg border border-slate-200 bg-white p-4">
  <pre class="mermaid">
    flowchart LR
      A[Controller] --> B[Service]
      B --> C[Repository]
      C -.leak.-> D[External API]
      classDef leak stroke:#dc2626,stroke-width:2px;
      class C,D leak
  </pre>
</div>
```

**Hand-Built HTML (Mass & Depth)**
Use standard `<div>` elements to visually compare surface area.
*   **Before:** Interface rectangle is tall (shallow module).
*   **After:** Interface rectangle is short, implementation is tall (deep module, `.deep` class).

## 4. Strict Vocabulary & Tone Rules

*   **Mandatory Nouns/Verbs:** module, interface, implementation, depth, deep, shallow, seam, adapter, leverage, locality.
*   **Banned Synonyms:** component, service, unit, API, boundary, wrapper. 
*   **Wins formatting examples:**
    *   "Locality: bugs concentrate in one module."
    *   "Leverage: one interface, N call sites."
    *   "Interface shrinks; implementation absorbs complexity."
*   **No Hedging:** Drop "it's worth noting that..." or "easier to maintain." State the structural fact.

## 5. Top Recommendation Section

One card at the bottom. 
*   Target name. 
*   One sentence explaining why it provides the highest leverage for the lowest effort. 
*   Anchor link to the candidate card.
