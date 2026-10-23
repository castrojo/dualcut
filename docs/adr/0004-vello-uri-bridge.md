# ADR 0004: vello:// URI handler bridges live vector rendering into GES

Date: 2026-07-18 · Status: accepted

## Context
Live per-frame vector output needs a GStreamer source GES can use, but the Rust bindings for GES expose only Formatter subclasses, not GESSource subclasses.

## Decision
`vellosrc` is a `GstPushSrc` subclass registered with a `vello://` URI
handler. uridecodebin — and therefore `GESUriClip` — instantiates it from
URIs like `vello://star?fill=%23ffd700&w=220&h=220&spin=1`. Static
`shape` clips keep the cached-PNG raster path (zero per-frame cost).

## Consequences
- Any document `video` clip can point at a live vector source today.
- Shape parameters live in the URI; to animate them per-frame, extend vellosrc properties, not the document mapping.
- A shared wgpu device and one Vello renderer (see `vector.rs`) serve both the PNG path and vellosrc. This renderer reuse makes per-frame output affordable.
