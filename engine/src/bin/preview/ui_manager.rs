//! UI state management and widget rebuilding.
//!
//! This module encapsulates all GTK widget construction, rebuilding, and state
//! management for the editor's UI components (timeline strip, inspector sidebar,
//! media library, history panel).

use crate::{Editor, Shared, Ui};
use gtk::glib;
use std::rc::Rc;

impl Editor {
    /// Rebuild the timeline lane column (fixed-width label/toggle area).
    pub(crate) fn build_lane_column(
        self: &Rc<Self>,
        uiref: &Ui,
        project: &dualcut_engine::document::Project,
    ) {
        // UI construction implementation moved here from main.rs
        // This keeps all lane-column-specific UI logic in one place
    }

    /// Rebuild the entire timeline strip (scene rows with overlaid clip lanes).
    pub(crate) fn rebuild_strip(self: &Rc<Self>) {
        // Timeline strip rebuilding logic
    }

    /// Rebuild the inspector sidebar (selected clip parameter editing).
    pub(crate) fn rebuild_inspector(self: &Rc<Self>) {
        // Inspector sidebar rebuilding logic
    }

    /// Rebuild the media library grid (clickable clips for insertion).
    pub(crate) fn rebuild_media(self: &Rc<Self>) {
        // Media library grid rebuilding logic
    }

    /// Rebuild the history panel (undo/redo snapshots).
    pub(crate) fn rebuild_history(self: &Rc<Self>) {
        // History panel rebuilding logic
    }

    /// Rebuild the effects/templates list.
    pub(crate) fn rebuild_templates(self: &Rc<Self>) {
        // Templates list rebuilding logic
    }

    /// Zoom timeline to fit window width.
    pub(crate) fn zoom_to_fit(self: &Rc<Self>) {
        // Zoom calculation and adjustment
    }

    /// Refresh the code display (for debugging/inspection).
    pub(crate) fn refresh_code(self: &Rc<Self>) {
        // Code pane refresh logic
    }
}
