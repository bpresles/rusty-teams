---
status: accepted
date: 2026-11-15
---

# ADR 0023: macOS desktop toasts use Notification Center

The fork changelog for 0.4.0-joe.6 called this record 0017. A later upstream sync shipped guest orgs as ADR 0017, so this note became 0018. Upstream then shipped regional format as ADR 0018, so this note is 0023.

## Context

ADR 0015 put Linux toasts on `org.freedesktop.Notifications` and left macOS on GPUI popups / stubs. For a macOS daily-driver build we need an OS surface that matches Teams expectations (corner banners, Do not disturb courtesy where available) without inventing a second GPUI popup path.

## Decision

On macOS, message / ring / meeting toasts that already go through the shared desktop-notification policy use Notification Center via `notify-rust`. Show/hide rules and copy stay shared with Linux. The app keeps playing its own tones. Action callbacks are best-effort (macOS Notification Center is weaker than freedesktop actions). Windows keeps the GPUI card.

## Consequences

- macOS users get system banners without a Wayland-style popup toolkit fight.
- Behaviour differs from Windows (history, Focus settings, limited actions).
- Signing / notarization still matter for predictable TCC and Gatekeeper; this ADR does not replace that packaging work.
