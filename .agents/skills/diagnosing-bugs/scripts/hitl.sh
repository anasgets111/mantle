#!/usr/bin/env bash
# Ponytail HITL (Human-In-The-Loop) Loop.
# Use ONLY when a bug requires manual interaction (Livewire DOM diffs, Quickshell rendering)
# and cannot be caught via headless loops.
#
# Usage: bash scripts/ponytail-hitl.sh

set -euo pipefail

step() {
  printf '\n>>> %s\n' "$1"
  read -r -p "    [Enter when done] " _
}

capture() {
  local var="$1" question="$2" answer
  printf '\n>>> %s\n' "$question"
  read -r -p "    > " answer
  printf -v "$var" '%s' "$answer"
}

printf "Ponytail HITL Diagnostics\n=========================\n"

# ==============================================================================
# TEMPLATE A: LARAVEL / LIVEWIRE
# ==============================================================================
# step "Open the browser to http://localhost:8000."
# step "Clear storage/logs/laravel.log. Open DevTools Network tab."
#
# capture TRIGGERED "Click the Livewire action button. Did the XHR request 500? (y/n)"
# capture DOM_STATE "Did the DOM revert or duplicate elements? (Describe/none)"
# capture ERROR_MSG "Paste the exact exception from laravel.log (or 'none'):"


# ==============================================================================
# TEMPLATE B: QUICKSHELL / QML (Arch Linux / Hyprland)
# ==============================================================================
# step "Kill existing quickshell instances: pkill quickshell"
# step "Run the target widget in a terminal: quickshell path/to/widget.qml &"
#
# capture RENDERED "Did the QML window render without syntax errors? (y/n)"
# capture STATE_SYNC "Change the Hyprland workspace. Did the Quickshell widget update? (y/n)"
# capture ERROR_MSG "Paste the exact error from the quickshell stdout (or 'none'):"

# --- UNCOMMENT AND EDIT ONE OF THE TEMPLATES ABOVE ---

printf '\n--- [ PONYTAIL HITL RESULTS ] ---\n'
# printf 'TRIGGERED=%s\n' "$TRIGGERED"
# printf 'DOM_STATE=%s\n' "$DOM_STATE"
# printf 'RENDERED=%s\n' "$RENDERED"
# printf 'STATE_SYNC=%s\n' "$STATE_SYNC"
# printf 'ERROR_MSG=%s\n' "$ERROR_MSG"
