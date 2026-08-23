---
name: diagnosing-bugs
description: Diagnosis loop for hard bugs and performance regressions. Forked for Laravel/Livewire and Quickshell/QML.
---

# Diagnosing Bugs

A strict discipline for hard bugs. Stop guessing. Build a loop, form a hypothesis, measure, fix.

## 0. Redact & Read
*   **Context:** Determine the project domain (Laravel/Livewire vs. Quickshell/QML). Read `CONTEXT.md` and ADRs.
*   **Security:** Redact all secrets (`<REDACTED>`) before outputting artifacts. Quote only the specific log lines carrying the signal.

## 1. Build a Feedback Loop (The Hard Part)
If you do not have a tight pass/fail signal that goes red on *this specific bug*, you will fail. Do not read code to guess. Build the loop.

### Domain A: Laravel & Livewire (Web)
*No bash wrappers for PHP logic.*
1.  **Tinker REPL:** Isolate logic and run directly via `php artisan tinker --execute="dd(User::find(1)->calculate());"`.
2.  **Failing Test:** A Pest/PHPUnit test hitting the exact class, endpoint, or Eloquent model.
3.  **Browser Script:** Dusk or Playwright for Livewire reactivity/DOM state issues.
4.  **Database Isolation:** A raw Postgres query proving the data state or performance bottleneck.

### Domain B: Quickshell & QML (Linux Desktop UI)
1.  **CLI Execution:** Write `.sh` scripts to run `quickshell path/to/file.qml` or `qmlscene`. Diff stdout/stderr.
2.  **LSP/Linting:** Run `qmlls` to catch static QML binding errors.
3.  **IPC/State Monitoring:** Bash loops utilizing `dbus-monitor`, `hyprctl`, or `journalctl` to trace state changes feeding the UI.

### Tighten It
*   Make it fast (seconds, not minutes).
*   Make it deterministic (seed RNG, mock APIs, isolate UI components).
*   *Completion Criterion:* You must name **one runnable command** that reliably reproduces the exact symptom.

## 2. Reproduce & Minimize
Watch the loop go red. Confirm it is the *user's exact symptom*.

**Minimize (The Deletion Test for Bugs):**
Cut inputs, callers, Livewire components, or QML objects one by one. Re-run. Keep only what is load-bearing for the failure. Shrink the hypothesis space.

## 3. Hypothesize (3-5 Ranked)
Before touching the code, state 3-5 falsifiable hypotheses.
*   *Format:* "If [X] is the cause, then [changing Y] will make it pass / [changing Z] will alter the error message."
*   Show the list to the user.

## 4. Instrument & Measure
Map probes to the hypotheses. Change one variable at a time.
*   **Laravel Logic:** Use Xdebug, `dd()`, or Ray.
*   **Quickshell/QML:** `console.log()` inside QML, or examine shell stdout.
*   **Performance:** Telescope (Laravel) or `EXPLAIN ANALYZE` (Postgres).
*   **Logging:** Tag all debug logs with `[DEBUG]`. Never "log everything."

## 5. Fix & Regression Test
1.  Turn the minimized repro into a permanent test (Pest for Laravel, or a stable QML shell assertion).
2.  Watch it fail.
3.  Apply the fix.
4.  Watch it pass.

## 6. Cleanup
*   [ ] Original loop runs green.
*   [ ] Regression test runs green.
*   [ ] `grep -r "DEBUG"` is empty.
*   [ ] `dd()`, `console.log()` and temporary routes are deleted.
*   [ ] The correct hypothesis is documented in the commit message.
