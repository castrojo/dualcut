# ADR 0001: Rust + GStreamer Editing Services + GTK4/libadwaita

Date: 2026-07-18 · Status: accepted

## Context
The native rewrite needed a language and media stack. Constraints: no DOM,
no WebKitGTK, GPU acceleration, GNOME app, Flatpak deliverable.

## Decision
Rust, with GES as the edit engine, gtk4paintablesink for preview,
GTK4/libadwaita for UI, Vello/wgpu for vector graphics, and
rustyscript/deno_core for embedded TypeScript.

## Rationale
Rust is the only ecosystem with all four pillars (survey in ROADMAP.md). GES renders timeline, layers, clips, and transitions for free with maintained bindings. Go has no NLE layer. GJS cannot embed any user TS, and it lacks GPU composition.

## Consequences
- GES objects are not Send: all GStreamer work stays on one thread;
  scripts and workers exchange *documents*, never GES handles.
- GES pipelines are single-timeline: live reload rebuilds the pipeline.
- The GNOME runtime ships no GES; the Flatpak builds it from source.
