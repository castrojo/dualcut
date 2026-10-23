# ADR 0002: JSON document as truth, TypeScript as scripting layer

Date: 2026-07-18 · Status: accepted

## Context
Dual usage (GUI + programmatic) needs a representation both can edit.
Remotion-style code-as-document and tldraw-style data-as-document.

## Decision
The project is a JSON document (schema-validated, diffable, readable
without execution). TypeScript is a script layer that
*produces* document changes (`export function edit(p: Project): Project`),
in-app and over HTTP; it is never the persisted format.

## Consequences
- The GUI round-trips every edit; agents introspect and validate cheaply;
  hot reload is safe (no code execution on load).
- Generative workflows still work: generate with TS, persist as JSON.
- The document schema versions as API surface: `.d.ts` + JSON Schema
  ship in-repo and must move with `document.rs`.
