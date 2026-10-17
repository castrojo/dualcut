# Editor Decomposition Plan: Extracting Focused Managers

## Overview
The `Editor` struct and its 80+ methods represent a god object coupling several independent domains: UI state, document persistence, GStreamer pipeline management, media library operations, and effects/templates. This plan extracts these into focused manager modules for clarity and testability.

## Current State (4,384 LOC in main.rs)
- **AppState**: Pipeline + Project + History state
- **Ui**: Widget references (40+ fields)
- **Editor**: Main god object with 80+ methods mixing all concerns
- **mod_statements**: 8 submodules (captions, prefs, keys, history, coords, pipeline, skills, export)

## Decomposition Strategy

### Phase 1: Module Structure (This PR)
Extract method groups from `impl Editor` into focused impl blocks organized by file:

1. **ui_manager.rs**
   - `rebuild_strip()`, `rebuild_inspector()`, `rebuild_media()`, `rebuild_history()`, `rebuild_templates()`
   - `build_lane_column()`, `build_scene_form()`, `zoom_to_fit()`, `refresh_code()`
   - **Concern**: All UI widget construction and state synchronization

2. **document_manager.rs**
   - `commit_document()`, `jump_to_history()`, `open_project()`, `save_project_as()`
   - `write_and_rebuild()`, `rebuild_in_memory()`, `undo()`, `redo()`, `rebuild()`
   - **Concern**: Document lifecycle, persistence, history tracking

3. **library_manager.rs**
   - `add_to_library()`, `import_media()`, `insert_media()`, `spawn_thumbnail_worker()`
   - `build_scene_form()`, `insert_template()`, `rebuild_templates()`
   - **Concern**: Media file operations, thumbnails, proxies, effects

4. **pipeline_manager.rs** (future)
   - `swap_pipeline()`, `drain_bus_errors()`, `handle_bus_error()`
   - **Concern**: GStreamer pipeline lifecycle and error handling

### Phase 2: Structural Isolation (Future PR)
After phase 1, create dedicated structs to encapsulate state:
- `DocumentState` for history/undo tracking
- `LibraryState` for pending proxies/thumbnails
- `UiState` for collapsed trees and widget references

### Phase 3: Public Interface (Future PR)
Define explicit public traits for each domain so callers depend on abstractions, not the Editor directly.

## Benefits
- **Cognitive Load**: Each module has a single, clear concern
- **Testing**: UI rebuild logic can be tested independently of document operations
- **Maintenance**: Changes to one domain (e.g., history) don't risk breaking another (e.g., UI)
- **Discoverability**: New contributors can locate relevant code by concern, not by searching through 4K LOC

## File Layout (After Phase 1)
```
engine/src/bin/preview/
├── main.rs                  # Editor struct + coordinator impl + main()
├── ui_manager.rs            # UI rebuilding and state sync
├── document_manager.rs      # Document lifecycle and persistence
├── library_manager.rs       # Media files, thumbnails, effects
├── pipeline.rs              # (existing) GStreamer pipeline
├── captions.rs              # (existing) Whisper integration
├── prefs.rs                 # (existing) GSettings preferences
├── coords.rs                # (existing) Timeline coordinate math
├── keys.rs                  # (existing) Keyboard shortcut handlers
├── history.rs               # (existing) Undo/redo snapshots
├── skills.rs                # (existing) Effect skill install
└── export.rs                # (existing) Export dialog
```

## Phase 1 Scope
- Add three new module files with impl blocks for each domain
- Move method definitions (signatures + minimal docstrings)
- Keep all implementations in main.rs for now (no code motion)
- Add `mod` declarations and `pub use` imports in main.rs

This ensures CI passes (no behavioral changes) while creating the module structure for future extraction.

---

*Architectural Finding: dualcut#169*  
*Related: PR #170 (module layout restructuring)*
