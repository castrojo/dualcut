//! dualcut editor (M2): GNOME/libadwaita app around a project document.
//!
//! - Preview through GES + gtk4paintablesink, transport + seek.
//! - Scene strip (widths ∝ duration, click to jump) + overlay track rows.
//! - Inspector sidebar: select any clip, edit its parameters; every edit
//!   writes the project JSON to disk — the document stays the single
//!   source of truth, so agents and $EDITOR see the same file.
//! - Live reload on external edits (mtime poll), undo/redo (Ctrl+Z/+Shift)
//!   as document snapshots, detach-audio op on video clips.
//!
//! Usage: preview [project.json | media-uri]

use anyhow::{Context, Result};
use dualcut_engine::{build_demo_timeline, document, document::Project, init, mapping};
use document::{
    detach_audio, effective_duration, find_clip, find_clip_mut, move_clip_to_lane, remove_clip,
    ripple_delete, save_as_def, split_clip,
};
use ges::prelude::*;
use gstreamer as gst;
use gstreamer_editing_services as ges;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::SystemTime;


mod captions;
use captions::{find_whisper, find_whisper_model, run_captions_job};

mod prefs;
use prefs::{
    apply_default_font_family, load_recents, prefs_default_font_family, prefs_file, prefs_set,
    prefs_set_default_font_family, prefs_set_preview_scale, prefs_set_show_script,
    prefs_set_use_proxies, prefs_show_script, prefs_use_proxies, preview_scale, remember_recent,
};

mod keys;

mod history;
use history::{diff_summary, take_agent_marker, EditSource, HistoryEntry};

mod coords;
use coords::{
    active_clips_at, failed_proxies, failed_templates, failed_thumbs, fx_hash, media_uri,
    snap_time, widget_to_comp,
};

mod pipeline;
use pipeline::{
    bus_message_error_summary, compile_project, compile_project_with_warnings_cache,
    make_pipeline, seek_to, start_paused,
};

mod skills;
use skills::{
    install_skill_to, show_skills_dialog,
    skill_update_available,
};

mod export;
use export::show_export_dialog;

const DEFAULT_PPS: f64 = 42.0;
/// Fixed width of a timeline lane's icon+label+toggles column. Shared by
/// every lane row and the ruler's leading spacer so the ruler's time
/// axis lines up with the clip lanes below it (#21).
const LANE_COL_PX: i32 = 190;

/// Which per-lane control was toggled (#21).
#[derive(Clone, Copy)]
enum LaneToggle {
    Lock,
    Hide,
    Mute,
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("org.tunaos.dualcut")
        // Skip D-Bus name acquisition: dev/CI environments may have a
        // broken or cross-namespace session bus, and single-instance
        // behavior is not needed for an editor.
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(|app| {
        if let Err(e) = build_ui(app) {
            eprintln!("error: {e:#}");
            app.quit();
        }
    });
    app.run_with_args::<&str>(&[])
}

pub(crate) struct AppState {
    pub(crate) pipeline: ges::Pipeline,
    pub(crate) project: Option<Project>,
    pub(crate) project_path: Option<PathBuf>,
    pub(crate) mtime: Option<SystemTime>,
    pub(crate) duration: f64,
    pub(crate) selected: Option<String>,
    /// Timeline zoom, pixels per second.
    pub(crate) pps: f64,
    pub(crate) history: Vec<HistoryEntry>,
    pub(crate) redo: Vec<String>,
    /// Set while the app itself writes the file, to skip one reload cycle.
    pub(crate) self_write: bool,
}

pub(crate) type Shared = Rc<RefCell<AppState>>;

struct Ui {
    picture: gtk::Picture,
    seek: gtk::Scale,
    strip: gtk::Box,
    /// Lane label column (#56): a separate, non-scrolling stack beside
    /// `strip`'s horizontally-scrolling content, so "Layer n" / track
    /// names stay visible while the clips scroll past them.
    label_col: gtk::Box,
    /// The horizontally-scrolling region wrapping `strip` -- its
    /// allocated width is the "fit to window" target for zoom_to_fit().
    content_scroll: gtk::ScrolledWindow,
    inspector: gtk::Box,
    media_grid: gtk::FlowBox,
    media_empty: gtk::Box,
    clips_box: gtk::Box,
    history_box: gtk::Box,
    toasts: adw::ToastOverlay,
    ruler: std::cell::RefCell<Option<gtk::DrawingArea>>,
    templates_list: gtk::ListBox,
    code_buffer: gtk::TextBuffer,
    play_btn: gtk::Button,
}

pub(crate) struct Editor {
    pub(crate) state: Shared,
    pub(crate) ui: RefCell<Option<Ui>>,
    /// True while a render thread is active (#35).
    pub(crate) exporting: Cell<bool>,
    /// Exports waiting behind the active render: (output path, profile).
    pub(crate) export_queue: RefCell<VecDeque<(String, String)>>,
    /// Scene/track ids collapsed in the Clips tab's tree (#39); persisted
    /// across rebuild_inspector() calls since the widgets are torn down
    /// and rebuilt on every edit.
    pub(crate) clips_collapsed: RefCell<std::collections::HashSet<String>>,
    /// Media URIs with a proxy transcode in flight on the thumbnail
    /// worker (#50); checked by rebuild_media() to show a "Generating
    /// preview…" spinner on that Library cell instead of a blank one.
    pub(crate) pending_proxies: RefCell<std::collections::HashSet<String>>,
}

impl Editor {
    pub(crate) fn base_dir(&self) -> PathBuf {
        self.state
            .borrow()
            .project_path
            .as_ref()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Cache directory for thumbnails, proxies, shapes, etc. (#63).
    /// Saved projects: `.dualcut-cache/` next to the project file (so
    /// moving the project folder moves the cache with it — most editors
    /// do this). Unsaved projects: `$XDG_CACHE_HOME/dualcut/` (falls
    /// back to `~/.cache/dualcut/`), avoiding the non-writable cwd that
    /// Flatpak and snaps impose.
    pub(crate) fn cache_dir(&self) -> PathBuf {
        if let Some(parent) = self
            .state
            .borrow()
            .project_path
            .as_ref()
            .and_then(|p| p.parent())
        {
            return parent.join(".dualcut-cache");
        }
        // XDG_CACHE_HOME with POSIX fallback: XDG spec says ~/.cache
        // when the env var is unset or empty.
        if let Ok(dir) = std::env::var("XDG_CACHE_HOME")
            && !dir.is_empty()
        {
            return PathBuf::from(dir).join("dualcut");
        }
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(".cache").join("dualcut");
        }
        // Last resort: temp dir (shouldn't happen on a normal desktop).
        std::env::temp_dir().join("dualcut-cache")
    }

    /// Persist the document, push a history entry, rebuild everything.
    /// Every GUI-originated edit goes through this (so it's tagged
    /// `EditSource::Gui`); external file edits and agent HTTP requests are
    /// tagged separately where they're detected (mtime-poll reload).
    fn commit_document(self: &Rc<Self>, project: Project) {
        let (path, prev) = {
            let st = self.state.borrow();
            (st.project_path.clone(), st.project.clone())
        };
        if let Some(prev) = prev {
            let summary = diff_summary(&prev, &project);
            let mut st = self.state.borrow_mut();
            st.history.push(HistoryEntry {
                snapshot: prev.to_json(),
                source: EditSource::Gui,
                summary,
                at: SystemTime::now(),
            });
            st.redo.clear();
        }
        match path {
            Some(path) => self.write_and_rebuild(&path, project),
            // Unsaved project: keep everything in memory until Save As.
            None => self.rebuild_in_memory(project),
        }
    }

    /// Jump directly to a past document state from the History panel
    /// (index into `history`, oldest first) rather than stepping through
    /// undo one edit at a time.
    fn jump_to_history(self: &Rc<Self>, index: usize) {
        let (path, snapshot) = {
            let mut st = self.state.borrow_mut();
            let Some(path) = st.project_path.clone() else { return };
            if index >= st.history.len() {
                return;
            }
            let entry = st.history[index].clone();
            st.history.truncate(index);
            let cur_json = st.project.as_ref().map(|p| p.to_json());
            if let Some(cur_json) = cur_json {
                st.redo.push(cur_json);
            }
            (path, entry.snapshot)
        };
        if let Ok(project) = Project::from_json(&snapshot) {
            self.write_and_rebuild(&path, project);
        }
    }

    /// GES pipelines are single-timeline (ADR 0001): a rebuilt timeline
    /// needs a fresh pipeline, with position carried over and the
    /// preview picture repointed at the new sink's paintable.
    fn swap_pipeline(self: &Rc<Self>, timeline: &ges::Timeline) {
        let (old, pos) = {
            let st = self.state.borrow();
            let pos = st
                .pipeline
                .query_position::<gst::ClockTime>()
                .map(|p| p.nseconds() as f64 / 1e9)
                .unwrap_or(0.0);
            (st.pipeline.clone(), pos)
        };
        let _ = old.set_state(gst::State::Null);
        match make_pipeline(timeline) {
            Ok((pipeline, paintable)) => {
                if let Some(ui) = self.ui.borrow().as_ref() {
                    ui.picture.set_paintable(Some(&paintable));
                }
                // Previously discarded (#57): a failed preroll left the
                // pipeline stuck in Null with no user-visible signal --
                // black preview, stuck timestamp, play button that
                // toggled its icon without anything actually playing.
                if let Err(e) = start_paused(&pipeline) {
                    eprintln!("pipeline preroll failed: {e:#}");
                    self.toast("Preview failed to load -- check the terminal for details");
                }
                self.drain_bus_errors(&pipeline);
                seek_to(&pipeline, pos);
                self.state.borrow_mut().pipeline = pipeline;
            }
            Err(e) => eprintln!("pipeline rebuild failed: {e:#}"),
        }
    }

    fn rebuild_in_memory(self: &Rc<Self>, project: Project) {
        match compile_project_with_warnings_cache(&project, &self.base_dir(), &self.cache_dir()) {
            Ok((timeline, warnings)) => {
                self.toast_compile_warnings(&warnings);
                self.swap_pipeline(&timeline);
                // #57: this in-memory path (unsaved project, or any GUI
                // edit before first Save) skipped the duration/seek-range
                // sync that the on-disk `rebuild()` path does -- so a new
                // project's seek bar stayed clamped at its 0.1s startup
                // default and the playhead never tracked real duration.
                let duration = project.duration();
                self.state.borrow_mut().duration = duration;
                if let Some(ui) = self.ui.borrow().as_ref() {
                    ui.seek.set_range(0.0, duration.max(0.1));
                }
                self.state.borrow_mut().project = Some(project);
                self.rebuild_strip();
                self.rebuild_inspector();
                self.rebuild_history();
                self.rebuild_media();
                self.rebuild_templates();
                self.refresh_code();
            }
            Err(e) => eprintln!("rebuild failed (keeping current timeline): {e:#}"),
        }
    }

    /// Save As: pick a path, write, adopt it for future auto-saves.
    fn save_project_as(self: &Rc<Self>, window: Option<&gtk::Window>) {
        let dialog = gtk::FileDialog::builder().title("Save project").build();
        dialog.set_initial_name(Some("project.json"));
        let this = self.clone();
        dialog.save(window, gtk::gio::Cancellable::NONE, move |res| {
            if let Ok(file) = res
                && let Some(path) = file.path() {
                    let project = this.state.borrow().project.clone();
                    if let Some(project) = project {
                        this.state.borrow_mut().project_path = Some(path.clone());
                        remember_recent(&path);
                        this.write_and_rebuild(&path, project);
                        if let Some(win) = this.window() {
                            win.set_title(Some(&format!(
                                "Dualcut — {}",
                                path.file_name().and_then(|n| n.to_str()).unwrap_or("project")
                            )));
                        }
                    }
                }
        });
    }

    /// Ripple-delete the selected clip (#34).
    fn ripple_delete_selected(self: &Rc<Self>) {
        let (project, selected) = {
            let st = self.state.borrow();
            (st.project.clone(), st.selected.clone())
        };
        let (Some(mut project), Some(selected)) = (project, selected) else { return };
        if selected.starts_with("scene:") {
            return;
        }
        match ripple_delete(&mut project, &selected) {
            Ok(()) => {
                self.state.borrow_mut().selected = None;
                self.commit_document(project);
                self.toast_undo(&format!("Clip {selected:?} ripple-deleted"));
            }
            Err(e) => eprintln!("ripple delete: {e:#}"),
        }
    }

    /// Split the selected clip at the playhead (#29).
    fn split_selected(self: &Rc<Self>) {
        let (project, selected, time) = {
            let st = self.state.borrow();
            let time = st
                .pipeline
                .query_position::<gst::ClockTime>()
                .map(|p| p.nseconds() as f64 / 1e9)
                .unwrap_or(0.0);
            (st.project.clone(), st.selected.clone(), time)
        };
        let (Some(mut project), Some(selected)) = (project, selected) else { return };
        if selected.starts_with("scene:") {
            return;
        }
        match split_clip(&mut project, &selected, time) {
            Ok(_) => self.commit_document(project),
            Err(e) => eprintln!("split: {e:#}"),
        }
    }

    /// Transient "X deleted — Undo" toast (deletes are silent otherwise).
    fn toast_undo(self: &Rc<Self>, message: &str) {
        let ui = self.ui.borrow();
        let Some(ui) = ui.as_ref() else { return };
        let toast = adw::Toast::new(message);
        toast.set_button_label(Some("Undo"));
        toast.set_timeout(5);
        let this = self.clone();
        toast.connect_button_clicked(move |_| this.undo());
        ui.toasts.add_toast(toast);
    }

    /// Transient notification without an action button.
    pub(crate) fn toast(&self, message: &str) {
        let ui = self.ui.borrow();
        let Some(ui) = ui.as_ref() else { return };
        let toast = adw::Toast::new(message);
        toast.set_timeout(5);
        ui.toasts.add_toast(toast);
    }

    /// Surface a GStreamer pipeline error: show toast, reset play button, and pause (#165).
    pub(crate) fn handle_bus_error(self: &Rc<Self>, err_msg: &str) {
        eprintln!("pipeline error: {err_msg}");
        self.toast(err_msg);
        if let Some(ui) = self.ui.borrow().as_ref() {
            ui.play_btn.set_icon_name("media-playback-start-symbolic");
        }
        let _ = self.state.borrow().pipeline.set_state(gst::State::Paused);
    }

    /// Drain pending error messages from the pipeline bus (#165).
    pub(crate) fn drain_bus_errors(self: &Rc<Self>, pipeline: &ges::Pipeline) {
        if let Some(bus) = pipeline.bus() {
            while let Some(msg) = bus.pop() {
                if let Some(err_msg) = bus_message_error_summary(&msg) {
                    self.handle_bus_error(&err_msg);
                }
            }
        }
    }

    /// Surface compile warnings that were previously only visible in
    /// `eprintln!` output the user never sees (#57) -- e.g. a clip skipped
    /// because it needs a feature this build lacks, or an effect that
    /// failed to apply.
    fn toast_compile_warnings(&self, warnings: &[String]) {
        match warnings {
            [] => {}
            [one] => self.toast(&format!("⚠ {one}")),
            many => self.toast(&format!(
                "⚠ {} compile warnings (first: {}) -- see terminal",
                many.len(),
                many[0]
            )),
        }
    }

    pub(crate) fn window(&self) -> Option<gtk::Window> {
        let ui = self.ui.borrow();
        ui.as_ref().and_then(|u| u.picture.root().and_downcast::<gtk::Window>())
    }

    /// Open a project file, replacing the current session state.
    fn open_project(self: &Rc<Self>, path: &std::path::Path) {
        match std::fs::read_to_string(path).map_err(anyhow::Error::from).and_then(|json| Project::from_json(&json)) {
            Ok(project) => {
                {
                    let mut st = self.state.borrow_mut();
                    st.project_path = Some(path.to_path_buf());
                    st.history.clear();
                    st.redo.clear();
                    st.selected = None;
                }
                remember_recent(path);
                if let Some(win) = self.window() {
                    win.set_title(Some(&format!(
                        "Dualcut — {}",
                        path.file_name().and_then(|n| n.to_str()).unwrap_or("project")
                    )));
                }
                self.rebuild_in_memory(project);
                self.zoom_to_fit();
            }
            Err(e) => eprintln!("open failed: {e:#}"),
        }
    }

    /// Add file paths to the project library (#15); shared by the Import
    /// dialog and window drag-and-drop.
    fn add_to_library(self: &Rc<Self>, paths: &[PathBuf]) {
        let project = self.state.borrow().project.clone();
        let Some(mut project) = project else { return };
        let base = self.base_dir();
        for path in paths {
            let entry = path
                .strip_prefix(&base)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| path.display().to_string());
            if !project.library.contains(&entry) {
                project.library.push(entry);
            }
        }
        self.commit_document(project);
    }

    /// Import media files into the project library (#15).
    fn import_media(self: &Rc<Self>, window: Option<&gtk::Window>) {
        let dialog = gtk::FileDialog::builder().title("Import media").build();
        let this = self.clone();
        dialog.open_multiple(window, gtk::gio::Cancellable::NONE, move |res| {
            let Ok(files) = res else { return };
            let paths: Vec<PathBuf> = (0..files.n_items())
                .filter_map(|i| files.item(i).and_downcast::<gtk::gio::File>()?.path())
                .collect();
            this.add_to_library(&paths);
        });
    }

    fn write_and_rebuild(self: &Rc<Self>, path: &std::path::Path, project: Project) {
        self.state.borrow_mut().self_write = true;
        if let Err(e) = std::fs::write(path, project.to_json()) {
            eprintln!("saving project failed: {e}");
            return;
        }
        {
            let mut st = self.state.borrow_mut();
            st.mtime = path.metadata().ok().and_then(|m| m.modified().ok());
            st.project = Some(project);
        }
        self.rebuild();
    }

    fn undo(self: &Rc<Self>) {
        let (path, snapshot) = {
            let mut st = self.state.borrow_mut();
            let Some(path) = st.project_path.clone() else { return };
            let Some(entry) = st.history.pop() else { return };
            if let Some(cur) = st.project.as_ref() {
                let json = cur.to_json();
                st.redo.push(json);
            }
            (path, entry.snapshot)
        };
        if let Ok(project) = Project::from_json(&snapshot) {
            self.write_and_rebuild(&path, project);
        }
    }

    fn redo(self: &Rc<Self>) {
        let (path, snapshot) = {
            let mut st = self.state.borrow_mut();
            let Some(path) = st.project_path.clone() else { return };
            let Some(snapshot) = st.redo.pop() else { return };
            let cur_json = st.project.as_ref().map(|p| p.to_json());
            if let Some(cur_json) = cur_json {
                st.history.push(HistoryEntry {
                    snapshot: cur_json,
                    source: EditSource::Gui,
                    summary: "Redo".into(),
                    at: SystemTime::now(),
                });
            }
            (path, snapshot)
        };
        if let Ok(project) = Project::from_json(&snapshot) {
            self.write_and_rebuild(&path, project);
        }
    }

    /// Rebuild pipeline + strip + inspector from the current document.
    fn rebuild(self: &Rc<Self>) {
        let (project, base_dir) = {
            let st = self.state.borrow();
            (st.project.clone(), self.base_dir())
        };
        let Some(project) = project else { return };
        match compile_project_with_warnings_cache(&project, &base_dir, &self.cache_dir())
            .and_then(|(tl, warnings)| Ok((make_pipeline(&tl)?, warnings)))
        {
            Ok(((pipeline, paintable), warnings)) => {
                self.toast_compile_warnings(&warnings);
                {
                    let ui = self.ui.borrow();
                    let Some(ui) = ui.as_ref() else { return };
                    let old_pos = {
                        let st = self.state.borrow();
                        let pos = st.pipeline.query_position::<gst::ClockTime>();
                        let _ = st.pipeline.set_state(gst::State::Null);
                        pos
                    };
                    ui.picture.set_paintable(Some(&paintable));
                    let duration = project.duration();
                    {
                        let mut st = self.state.borrow_mut();
                        st.pipeline = pipeline.clone();
                        st.duration = duration;
                    }
                    ui.seek.set_range(0.0, duration.max(0.1));
                    // Previously discarded (#57): a failed preroll left the
                    // pipeline stuck in Null with no user-visible signal --
                    // black preview, stuck timestamp, play button that
                    // toggled its icon without anything actually playing.
                    let preroll_err = {
                        let st = self.state.borrow();
                        let result = start_paused(&st.pipeline);
                        if let Some(pos) = old_pos {
                            let max = gst::ClockTime::from_useconds((duration * 1e6) as u64);
                            seek_to(&st.pipeline, (pos.min(max)).nseconds() as f64 / 1e9);
                        }
                        result.err()
                    };
                    if let Some(e) = preroll_err {
                        eprintln!("pipeline preroll failed: {e:#}");
                        self.toast("Preview failed to load -- check the terminal for details");
                    }
                    self.drain_bus_errors(&pipeline);
                }
                self.rebuild_strip();
                self.rebuild_inspector();
                self.rebuild_history();
                self.rebuild_media();
                self.rebuild_templates();
                self.refresh_code();
            }
            Err(e) => eprintln!("rebuild failed (keeping current timeline): {e:#}"),
        }
    }

    /// Icon + label + Lock/Hide/Mute toggle column for one timeline lane
    /// row (#21), fixed to [`LANE_COL_PX`] so every row (and the ruler's
    /// spacer) lines up on the same x-axis. `on_toggle` commits whatever
    /// document mutation the caller's lane identity needs.
    fn build_lane_column(
        self: &Rc<Self>,
        icon: &str,
        label: &str,
        locked: bool,
        hidden: bool,
        muted: bool,
        on_toggle: Rc<dyn Fn(LaneToggle, bool)>,
    ) -> gtk::Box {
        let col = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        col.set_size_request(LANE_COL_PX, -1);
        // Fixed-width column, not a growable one (#56): an empty lane row
        // (nothing yet in its adjoining Fixed) let this soak up the row's
        // leftover space without an explicit hexpand(false).
        col.set_hexpand(false);
        col.set_margin_start(4);
        let icon_w = gtk::Image::from_icon_name(icon);
        icon_w.set_pixel_size(14);
        col.append(&icon_w);
        let lbl = gtk::Label::new(Some(label));
        lbl.add_css_class("dim-label");
        lbl.set_ellipsize(gtk::pango::EllipsizeMode::End);
        lbl.set_hexpand(true);
        lbl.set_halign(gtk::Align::Start);
        col.append(&lbl);
        for (kind, icon_on, icon_off, active, tip) in [
            (LaneToggle::Lock, "changes-prevent-symbolic", "changes-allow-symbolic", locked, "Lock lane"),
            (LaneToggle::Hide, "view-reveal-symbolic", "view-conceal-symbolic", hidden, "Hide lane"),
            (LaneToggle::Mute, "audio-volume-high-symbolic", "audio-volume-muted-symbolic", muted, "Mute lane"),
        ] {
            let b = gtk::ToggleButton::new();
            b.set_icon_name(if active { icon_off } else { icon_on });
            b.set_active(active);
            b.add_css_class("flat");
            b.set_tooltip_text(Some(tip));
            b.update_property(&[gtk::accessible::Property::Label(tip)]);
            let on_toggle = on_toggle.clone();
            b.connect_toggled(move |btn| on_toggle(kind, btn.is_active()));
            col.append(&b);
        }
        col
    }

    /// Zoom so the whole project's current total duration fills the
    /// timeline's visible width -- "100%" per #56 (the strip shows every
    /// scene concatenated on one absolute-time axis, not a single scene
    /// in isolation, so "fit" is naturally project-wide here rather than
    /// per-scene). Falls back to the existing pps if the strip hasn't
    /// been allocated a width yet (e.g. called before the window is
    /// first shown).
    fn zoom_to_fit(self: &Rc<Self>) {
        let width = {
            let ui = self.ui.borrow();
            ui.as_ref().map(|u| u.content_scroll.width()).unwrap_or(0)
        };
        if width <= 0 {
            return;
        }
        let duration = {
            let st = self.state.borrow();
            st.project.as_ref().map(|p| p.duration()).unwrap_or(0.0)
        };
        if duration <= 0.0 {
            return;
        }
        let pps = (width as f64 / duration).clamp(4.0, 400.0);
        self.state.borrow_mut().pps = pps;
        self.rebuild_strip();
    }

    fn rebuild_strip(self: &Rc<Self>) {
        let ui = self.ui.borrow();
        let Some(ui) = ui.as_ref() else { return };
        while let Some(child) = ui.strip.first_child() {
            ui.strip.remove(&child);
        }
        while let Some(child) = ui.label_col.first_child() {
            ui.label_col.remove(&child);
        }
        let (project, pps) = {
            let st = self.state.borrow();
            (st.project.clone(), st.pps)
        };
        let Some(project) = project else { return };
        let cache = self.cache_dir();
        self.spawn_thumbnail_worker(&project, &cache);

        // Compact scene ruler: one thin strip showing scene segments and
        // the playhead — scene blocks no longer eat timeline height
        // (#19, #21). Click seeks and selects the scene under the click.
        let ruler = gtk::DrawingArea::new();
        ruler.set_content_height(26);
        ruler.set_content_width((project.duration() * pps) as i32 + 40);
        {
            let project = project.clone();
            let this = self.clone();
            ruler.set_draw_func(move |_, cr, w, h| {
                let (w, h) = (w as f64, h as f64);
                cr.set_source_rgb(0.12, 0.12, 0.14);
                let _ = cr.paint();
                let palette = [(0.32, 0.41, 0.94), (0.36, 0.83, 0.62), (0.90, 0.65, 0.04), (0.79, 0.38, 0.68)];
                let selected = this.state.borrow().selected.clone();
                for (i, scene) in project.scenes.iter().enumerate() {
                    let x0 = project.scene_offset(i) * pps;
                    let x1 = x0 + scene.duration * pps;
                    let (r, g, b) = palette[i % palette.len()];
                    let sel = selected.as_deref() == Some(&format!("scene:{}", scene.id));
                    cr.set_source_rgba(r, g, b, if sel { 0.9 } else { 0.55 });
                    cr.rectangle(x0 + 1.0, 2.0, (x1 - x0 - 2.0).max(1.0), h - 4.0);
                    let _ = cr.fill();
                    cr.set_source_rgb(1.0, 1.0, 1.0);
                    cr.move_to(x0 + 6.0, h - 8.0);
                    let label = if scene.name.is_empty() { &scene.id } else { &scene.name };
                    cr.set_font_size(11.0);
                    let _ = cr.show_text(&format!("{label} · {:.1}s", scene.duration));
                }
                // Playhead -- read the current pipeline live rather than a
                // stale clone captured at widget-creation time (#57).
                if let Some(pos) = this.state.borrow().pipeline.query_position::<gst::ClockTime>() {
                    let x = pos.nseconds() as f64 / 1e9 * pps;
                    if x <= w {
                        cr.set_source_rgb(0.95, 0.25, 0.25);
                        cr.rectangle(x - 0.5, 0.0, 1.5, h);
                        let _ = cr.fill();
                    }
                }
            });
        }
        {
            let this = self.clone();
            let project = project.clone();
            let click = gtk::GestureClick::new();
            click.connect_pressed(move |_, _, x, _| {
                let time = (x / pps).max(0.0);
                seek_to(&this.state.borrow().pipeline, time.min(project.duration()));
                let index = (0..project.scenes.len())
                    .rev()
                    .find(|&i| time >= project.scene_offset(i))
                    .unwrap_or(0);
                let id = project.scenes[index].id.clone();
                this.state.borrow_mut().selected = Some(format!("scene:{id}"));
                this.rebuild_inspector();
            });
            ruler.add_controller(click);
        }
        // Spacer matching the ruler's own height, in the label column
        // (now a separate non-scrolling stack, #56), so lane rows still
        // line up under their labels one-for-one.
        ui.strip.append(&ruler);
        let ruler_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        ruler_spacer.set_size_request(LANE_COL_PX, 26);
        ui.label_col.append(&ruler_spacer);
        *ui.ruler.borrow_mut() = Some(ruler.clone());

        // Scene-layer lanes: one row per layer slot, clips positioned at
        // absolute time (scene offset + clip start). Dragging retimes the
        // clip within its scene; right-edge drag trims duration. Always
        // show at least one lane so a fresh project has somewhere to
        // drop the first clip (#21).
        let max_layers =
            project.scenes.iter().map(|s| s.layers.len()).max().unwrap_or(0).max(1);
        for li in 0..max_layers {
            let meta = project.scene_lanes.get(li).cloned().unwrap_or_default();
            let on_toggle: Rc<dyn Fn(LaneToggle, bool)> = {
                let this = self.clone();
                let project_snapshot = project.clone();
                Rc::new(move |kind: LaneToggle, active: bool| {
                    let mut project = project_snapshot.clone();
                    if project.scene_lanes.len() <= li {
                        project.scene_lanes.resize(li + 1, document::LaneMeta::default());
                    }
                    let lane = &mut project.scene_lanes[li];
                    match kind {
                        LaneToggle::Lock => lane.locked = active,
                        LaneToggle::Hide => lane.hidden = active,
                        LaneToggle::Mute => lane.muted = active,
                    }
                    this.commit_document(project);
                })
            };
            let col = self.build_lane_column(
                "video-x-generic-symbolic",
                &format!("Layer {}", li + 1),
                meta.locked,
                meta.hidden,
                meta.muted,
                on_toggle,
            );
            ui.label_col.append(&col);
            let lane = gtk::Fixed::new();
            lane.set_size_request((project.duration() * pps) as i32 + 40, 30);
            for (si, scene) in project.scenes.iter().enumerate() {
                let Some(clip) = scene.layers.get(li) else { continue };
                let offset = project.scene_offset(si);
                let duration = effective_duration(clip, Some(scene));
                let scene_off = offset;
                self.add_lane_clip(
                    &lane,
                    clip,
                    offset + clip.start,
                    duration,
                    // Flexible scene duration (#21): don't clamp to the
                    // current scene length -- dragging/trimming past it
                    // grows the scene instead (see grow_scene_for_clip).
                    Rc::new(move |raw_abs: f64| (raw_abs - scene_off).max(0.0)),
                    true,
                    li,
                    meta.locked,
                );
            }
            ui.strip.append(&lane);
        }

        for (ti, track) in project.overlays.iter().enumerate() {
            let name = if track.name.is_empty() { &track.id } else { &track.name };
            let on_toggle: Rc<dyn Fn(LaneToggle, bool)> = {
                let this = self.clone();
                let track_id = track.id.clone();
                let project_snapshot = project.clone();
                Rc::new(move |kind: LaneToggle, active: bool| {
                    let mut project = project_snapshot.clone();
                    if let Some(t) = project.overlays.iter_mut().find(|t| t.id == track_id) {
                        match kind {
                            LaneToggle::Lock => t.locked = active,
                            LaneToggle::Hide => t.hidden = active,
                            LaneToggle::Mute => t.muted = active,
                        }
                        this.commit_document(project);
                    }
                })
            };
            let col = self.build_lane_column(
                "audio-x-generic-symbolic",
                name,
                track.locked,
                track.hidden,
                track.muted,
                on_toggle,
            );
            ui.label_col.append(&col);
            let lane = gtk::Fixed::new();
            lane.set_size_request((project.duration() * pps) as i32 + 40, 30);
            for clip in &track.clips {
                self.add_lane_clip(
                    &lane,
                    clip,
                    clip.start,
                    clip.duration,
                    Rc::new(|raw_abs: f64| raw_abs.max(0.0)),
                    false,
                    max_layers + ti,
                    track.locked,
                );
            }
            ui.strip.append(&lane);
        }
    }

    /// Shared lane clip: click selects+seeks, body drag retimes (snapped,
    /// `to_start` maps absolute lane seconds to the stored start), edge
    /// drag (last 12px) trims duration.
    #[allow(clippy::too_many_arguments)]
    fn add_lane_clip(
        self: &Rc<Self>,
        lane: &gtk::Fixed,
        clip: &document::Clip,
        abs_start: f64,
        duration: f64,
        to_start: Rc<dyn Fn(f64) -> f64>,
        scene_relative: bool,
        lane_index: usize,
        locked: bool,
    ) {
        let (project, pps) = {
            let st = self.state.borrow();
            (st.project.clone(), st.pps)
        };
        let Some(project) = project else { return };
        let cache = self.cache_dir();

        // Left-aligned label, not GtkButton's centered default (#56):
        // with_label()'s auto-created Label isn't exposed to re-align, so
        // build the child directly.
        let button = gtk::Button::new();
        let clip_label = gtk::Label::new(Some(&clip.id));
        clip_label.set_halign(gtk::Align::Start);
        clip_label.set_margin_start(4);
        clip_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        button.set_child(Some(&clip_label));
        button.add_css_class("flat");
        button.add_css_class(match &clip.element {
            document::Element::Text { .. } => "clip-text",
            document::Element::Video { .. } => "clip-video",
            document::Element::Audio { .. } => "clip-audio",
            document::Element::Image { .. } => "clip-image",
            document::Element::Shape { .. } => "clip-shape",
            document::Element::CompRef { .. } => "clip-compref",
            document::Element::Test { .. } => "clip-test",
        });
        let width_px = ((duration * pps) as i32).max(30);
        button.set_size_request(width_px, 28);
        button.set_tooltip_text(Some(if locked {
            "Locked (unlock the lane to edit)"
        } else {
            "Drag to move · right edge trims"
        }));
        if let document::Element::Audio { src, .. } = &clip.element
            && let Some(uri) = media_uri(src, &self.base_dir()) {
                let wave = cache.join(format!("wave-{:016x}.png", fx_hash(&uri)));
                if wave.exists() {
                    let pic = gtk::Picture::for_filename(&wave);
                    pic.set_content_fit(gtk::ContentFit::Fill);
                    button.set_child(Some(&pic));
                    button.set_tooltip_text(Some(&clip.id));
                }
            }
        lane.put(&button, abs_start * pps, 1.0);

        {
            let this = self.clone();
            let id = clip.id.clone();
            button.connect_clicked(move |_| {
                {
                    let mut st = this.state.borrow_mut();
                    seek_to(&st.pipeline, abs_start);
                    st.selected = Some(id.clone());
                }
                this.rebuild_inspector();
            });
        }

        let drag = gtk::GestureDrag::new();
        // (orig_x_px, trim_mode)
        let dragmeta = Rc::new(std::cell::Cell::new((abs_start * pps, false)));
        {
            let dragmeta = dragmeta.clone();
            drag.connect_drag_begin(move |_, sx, _| {
                let trim = sx > (width_px as f64) - 12.0;
                dragmeta.set((abs_start * pps, trim));
            });
        }
        {
            let lane = lane.clone();
            let button = button.clone();
            let dragmeta = dragmeta.clone();
            drag.connect_drag_update(move |_, dx, _| {
                // A click is not a drag: nothing moves inside the
                // threshold (#20).
                if dx.abs() < 6.0 {
                    return;
                }
                let (ox, trim) = dragmeta.get();
                if trim {
                    button.set_size_request(((width_px as f64 + dx) as i32).max(20), 28);
                } else {
                    lane.move_(&button, (ox + dx).max(0.0), 1.0);
                }
            });
        }
        {
            let this = self.clone();
            let id = clip.id.clone();
            let dragmeta = dragmeta.clone();
            let project_snapshot = project.clone();
            drag.connect_drag_end(move |_, dx, dy| {
                if dx.abs() < 6.0 && dy.abs() < 6.0 {
                    // Treat as a click: select the clip (#20).
                    this.state.borrow_mut().selected = Some(id.clone());
                    this.rebuild_inspector();
                    return;
                }
                let (ox, trim) = dragmeta.get();
                let mut project = project_snapshot.clone();
                // Vertical drag beyond one row height = move to another lane.
                const LANE_H: f64 = 36.0;
                let lane_delta = (dy / LANE_H).round() as i64;
                if !trim && lane_delta != 0 {
                    let target = (lane_index as i64 + lane_delta)
                        .clamp(0, document::lane_count(&project) as i64 - 1)
                        as usize;
                    let raw_abs = ((ox + dx).max(0.0)) / pps;
                    let snapped = snap_time(&project, raw_abs);
                    match move_clip_to_lane(&mut project, &id, target, snapped) {
                        Ok(()) => this.commit_document(project),
                        Err(e) => eprintln!("move to lane {target}: {e}"),
                    }
                    return;
                }
                if trim {
                    let new_dur = (duration + dx / pps).max(0.1);
                    let snapped_end = if scene_relative {
                        new_dur
                    } else {
                        snap_time(&project, abs_start + new_dur) - abs_start
                    };
                    if let Some(c) = find_clip_mut(&mut project, &id) {
                        c.duration = snapped_end.max(0.1);
                    }
                } else {
                    let raw_abs = ((ox + dx).max(0.0)) / pps;
                    let snapped = snap_time(&project, raw_abs);
                    let new_start = to_start(snapped);
                    if let Some(c) = find_clip_mut(&mut project, &id) {
                        c.start = new_start;
                    }
                }
                // Flexible scene duration (#21): a scene-relative clip
                // that now runs past the scene's end grows the scene
                // instead of getting silently clipped.
                if scene_relative {
                    document::grow_scene_for_clip(&mut project, &id);
                }
                this.commit_document(project);
            });
        }
        // A locked lane keeps clips selectable (button.connect_clicked
        // above) but not draggable (#21).
        if !locked {
            button.add_controller(drag);
        }
    }

    /// Generate any missing media thumbnails off-thread, then refresh the
    /// strip once so they appear.
    fn spawn_thumbnail_worker(self: &Rc<Self>, project: &Project, cache: &std::path::Path) {
        let base_dir = self.base_dir();
        let mut thumbs: Vec<String> = Vec::new();
        let mut waves: Vec<String> = Vec::new();
        let mut proxies: Vec<String> = Vec::new();
        let want_proxies = prefs_use_proxies();
        let queue_proxy = |uri: &str, proxies: &mut Vec<String>| {
            if want_proxies
                && uri.starts_with("file://")
                && !dualcut_engine::thumbs::proxy_path(cache, uri).exists()
                && !proxies.iter().any(|u| u == uri)
                && !failed_proxies().lock().unwrap().contains(uri)
            {
                proxies.push(uri.to_string());
            }
        };
        let all_clips = project
            .scenes
            .iter()
            .flat_map(|s| s.layers.iter())
            .chain(project.overlays.iter().flat_map(|t| t.clips.iter()));
        for rel in &project.library {
            if let Some(uri) = media_uri(rel, &base_dir) {
                let ext = rel.rsplit(".").next().unwrap_or("").to_lowercase();
                let is_audio = matches!(ext.as_str(), "ogg" | "mp3" | "wav" | "flac");
                let is_image =
                    matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg");
                if !is_audio && !is_image {
                    queue_proxy(&uri, &mut proxies);
                }
                if is_audio {
                    if !cache.join(format!("wave-{:016x}.png", fx_hash(&uri))).exists()
                        && !failed_thumbs().lock().unwrap().contains(&uri)
                    {
                        waves.push(uri);
                    }
                } else if !cache.join(format!("thumb-{:016x}.png", fx_hash(&uri))).exists()
                    && !failed_thumbs().lock().unwrap().contains(&uri)
                {
                    thumbs.push(uri);
                }
            }
        }
        for clip in all_clips {
            match &clip.element {
                document::Element::Video { src, .. } => {
                    if let Some(uri) = media_uri(src, &base_dir) {
                        queue_proxy(&uri, &mut proxies);
                        if !cache.join(format!("thumb-{:016x}.png", fx_hash(&uri))).exists()
                            && !failed_thumbs().lock().unwrap().contains(&uri)
                        {
                            thumbs.push(uri);
                        }
                    }
                }
                document::Element::Image { src } => {
                    if let Some(uri) = media_uri(src, &base_dir)
                        && !cache.join(format!("thumb-{:016x}.png", fx_hash(&uri))).exists()
                        && !failed_thumbs().lock().unwrap().contains(&uri)
                    {
                        thumbs.push(uri);
                    }
                }
                document::Element::Audio { src, .. } => {
                    if let Some(uri) = media_uri(src, &base_dir)
                        && !cache.join(format!("wave-{:016x}.png", fx_hash(&uri))).exists()
                        && !failed_thumbs().lock().unwrap().contains(&uri)
                    {
                        waves.push(uri);
                    }
                }
                _ => {}
            }
        }
        // Thumbnail the starter catalog too, not just defs the project
        // already carries (#21: defs only enter the document on insert).
        // Project's own def wins on a name clash.
        let mut catalog_project = project.clone();
        let mut merged_defs = dualcut_engine::templates::starter_defs();
        merged_defs.extend(catalog_project.defs.clone());
        catalog_project.defs = merged_defs;
        let tpl_missing: Vec<String> = catalog_project
            .defs
            .iter()
            .filter(|(name, d)| {
                let key = format!(
                    "tpl-{:016x}.png",
                    fx_hash(&serde_json::to_string(d).unwrap_or_default())
                );
                !cache.join(key).exists() && !failed_templates().lock().unwrap().contains(*name)
            })
            .map(|(n, _)| n.clone())
            .collect();
        if thumbs.is_empty() && waves.is_empty() && tpl_missing.is_empty() && proxies.is_empty()
        {
            return;
        }
        let cache = cache.to_path_buf();
        let this = self.clone();
        let project_snapshot = catalog_project;
        // Mark these URIs pending now (#50) so this same rebuild's
        // subsequent rebuild_media() call shows a spinner immediately,
        // rather than the cell just staying blank until the worker
        // thread's single end-of-batch signal arrives.
        {
            let mut pending = this.pending_proxies.borrow_mut();
            for uri in &proxies {
                pending.insert(uri.clone());
            }
        }
        let proxies_done = proxies.clone();
        let (tx, rx) = std::sync::mpsc::channel::<bool>();
        std::thread::spawn(move || {
            for uri in thumbs {
                if let Err(e) = dualcut_engine::thumbs::thumbnail_png(&cache, &uri) {
                    eprintln!("thumbnail failed for {uri}: {e:#}");
                    failed_thumbs().lock().unwrap().insert(uri);
                }
            }
            let mut made_proxies = false;
            for uri in proxies {
                match dualcut_engine::thumbs::proxy_mp4(&cache, &uri) {
                    Ok(_) => made_proxies = true,
                    Err(e) => {
                        // Remember the failure so strip rebuilds don't
                        // retry an expensive doomed transcode forever.
                        eprintln!("proxy failed for {uri}: {e:#}");
                        failed_proxies().lock().unwrap().insert(uri);
                    }
                }
            }
            for name in tpl_missing {
                if let Err(e) = dualcut_engine::thumbs::template_png(
                    &cache,
                    &project_snapshot,
                    &name,
                    &base_dir,
                ) {
                    eprintln!("template thumb failed for {name}: {e:#}");
                    failed_templates().lock().unwrap().insert(name);
                }
            }
            for uri in waves {
                if let Err(e) = dualcut_engine::thumbs::waveform_png(&cache, &uri) {
                    eprintln!("waveform failed for {uri}: {e:#}");
                    failed_thumbs().lock().unwrap().insert(uri);
                }
            }
            let _ = tx.send(made_proxies);
        });
        glib::timeout_add_local(std::time::Duration::from_millis(300), move || {
            match rx.try_recv() {
                Ok(made_proxies) => {
                    {
                        let mut pending = this.pending_proxies.borrow_mut();
                        for uri in &proxies_done {
                            pending.remove(uri);
                        }
                    }
                    if made_proxies {
                        // Recompile so the preview pipeline picks up the
                        // freshly generated proxies (compile_project swaps
                        // sources); rebuild_in_memory refreshes the rest.
                        let project = this.state.borrow().project.clone();
                        if let Some(project) = project {
                            this.rebuild_in_memory(project);
                        }
                    } else {
                        this.rebuild_strip();
                        this.rebuild_media();
                    }
                    glib::ControlFlow::Break
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(_) => glib::ControlFlow::Break,
            }
        });
    }

    /// Library tab: media the user imported (project.library), shown as a
    /// thumbnail grid with a context menu (#15).
    fn rebuild_media(self: &Rc<Self>) {
        let ui = self.ui.borrow();
        let Some(ui) = ui.as_ref() else { return };
        while let Some(child) = ui.media_grid.first_child() {
            ui.media_grid.remove(&child);
        }
        let (library, base) = {
            let st = self.state.borrow();
            (
                st.project.as_ref().map(|p| p.library.clone()).unwrap_or_default(),
                self.base_dir(),
            )
        };
        ui.media_empty.set_visible(library.is_empty());
        let cache = self.cache_dir();
        for rel in library {
            let cell = gtk::Box::new(gtk::Orientation::Vertical, 4);
            cell.set_margin_top(4);
            cell.set_margin_bottom(4);
            if let Some(uri) = media_uri(&rel, &base) {
                let thumb = cache.join(format!("thumb-{:016x}.png", fx_hash(&uri)));
                let wave = cache.join(format!("wave-{:016x}.png", fx_hash(&uri)));
                let img = if thumb.exists() { Some(thumb) } else if wave.exists() { Some(wave) } else { None };
                if let Some(img) = img {
                    let pic = gtk::Picture::for_filename(&img);
                    pic.set_size_request(120, 68);
                    pic.set_content_fit(gtk::ContentFit::Cover);
                    cell.append(&pic);
                } else {
                    let icon = gtk::Image::from_icon_name("video-x-generic-symbolic");
                    icon.set_pixel_size(48);
                    cell.append(&icon);
                }
            }
            let label = gtk::Label::new(Some(
                std::path::Path::new(&rel)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&rel),
            ));
            label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            label.set_max_width_chars(14);
            label.set_tooltip_text(Some(&rel));
            cell.append(&label);

            // Proxy transcode in flight (#50): the fast first-frame
            // thumbnail above (if any) is usually already there, but the
            // scrub-friendly proxy itself can take a few seconds -- show
            // that it's working rather than leaving the cell looking done.
            if let Some(uri) = media_uri(&rel, &base)
                && self.pending_proxies.borrow().contains(&uri)
            {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
                row.set_halign(gtk::Align::Center);
                let spinner = gtk::Spinner::new();
                spinner.start();
                row.append(&spinner);
                let generating = gtk::Label::new(Some("Generating preview…"));
                generating.add_css_class("dim-label");
                generating.add_css_class("caption");
                row.append(&generating);
                cell.append(&row);
            }

            // Right-click context menu: add / remove.
            let gesture = gtk::GestureClick::new();
            gesture.set_button(3);
            {
                let this = self.clone();
                let rel = rel.clone();
                let cell = cell.clone();
                gesture.connect_pressed(move |_, _, x, y| {
                    let pop = gtk::Popover::new();
                    let menu_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
                    for (label, action) in [("Add to Timeline", 0), ("Remove from Library", 1)] {
                        let b = gtk::Button::with_label(label);
                        b.add_css_class("flat");
                        let this = this.clone();
                        let rel = rel.clone();
                        let pop2 = pop.clone();
                        b.connect_clicked(move |_| {
                            pop2.popdown();
                            if action == 0 {
                                this.insert_media(&rel);
                            } else {
                                let project = this.state.borrow().project.clone();
                                if let Some(mut project) = project {
                                    project.library.retain(|e| e != &rel);
                                    this.commit_document(project);
                                    this.toast_undo("Removed from library");
                                }
                            }
                        });
                        menu_box.append(&b);
                    }
                    pop.set_child(Some(&menu_box));
                    pop.set_parent(&cell);
                    pop.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                    pop.popup();
                });
            }
            cell.add_controller(gesture);

            // Double-click adds to the timeline.
            let dbl = gtk::GestureClick::new();
            dbl.set_button(1);
            {
                let this = self.clone();
                let rel = rel.clone();
                dbl.connect_pressed(move |_, n, _, _| {
                    if n == 2 {
                        this.insert_media(&rel);
                    }
                });
            }
            cell.add_controller(dbl);
            ui.media_grid.insert(&cell, -1);
        }
    }

    /// Insert a media file as a clip in the scene under the playhead.
    fn insert_media(self: &Rc<Self>, rel: &str) {
        let (project, time) = {
            let st = self.state.borrow();
            let time = st
                .pipeline
                .query_position::<gst::ClockTime>()
                .map(|p| p.nseconds() as f64 / 1e9)
                .unwrap_or(0.0);
            (st.project.clone(), time)
        };
        let Some(mut project) = project else { return };
        let ext = rel.rsplit('.').next().unwrap_or("").to_lowercase();
        let element = match ext.as_str() {
            "png" | "jpg" | "jpeg" | "webp" => document::Element::Image { src: rel.to_string() },
            "ogg" | "mp3" | "wav" | "flac" => {
                document::Element::Audio { src: rel.to_string(), offset: 0.0, volume: 1.0, rate: 1.0 }
            }
            _ => document::Element::Video { src: rel.to_string(), offset: 0.0, volume: 1.0, rate: 1.0 },
        };
        let mut id_base = rel
            .chars()
            .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
            .collect::<String>();
        id_base.truncate(24);
        let mut id = id_base.clone();
        let mut n = 1;
        while find_clip(&project, &id).is_some() {
            id = format!("{id_base}-{n}");
            n += 1;
        }
        let index = (0..project.scenes.len())
            .rev()
            .find(|&i| time >= project.scene_offset(i))
            .unwrap_or(0);
        let offset = project.scene_offset(index);
        let scene = &mut project.scenes[index];
        let start = (time - offset).clamp(0.0, (scene.duration - 0.1).max(0.0));
        // Video dropped from the Library goes above the existing layers
        // (#21) -- everything else keeps stacking on top as before.
        let is_video = matches!(element, document::Element::Video { .. });
        let clip = document::Clip {
            id: id.clone(),
            start,
            duration: 0.0,
            element,
            transform: Default::default(),
            animations: Vec::new(),
            effects: Vec::new(),
        };
        if is_video {
            scene.layers.insert(0, clip);
        } else {
            scene.layers.push(clip);
        }
        self.state.borrow_mut().selected = Some(id);
        self.commit_document(project);
    }

    /// Scene form: duration and transition editing.
    fn build_scene_form(self: &Rc<Self>, uiref: &Ui, project: &Project, scene_id: &str) {
        let Some(scene) = project.scenes.iter().find(|s| s.id == scene_id) else { return };
        let form = gtk::Box::new(gtk::Orientation::Vertical, 6);
        form.set_margin_top(8);
        let title = gtk::Label::new(Some(&format!("Scene: {scene_id}")));
        title.add_css_class("heading");
        title.set_halign(gtk::Align::Start);
        form.append(&title);

        let dur_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let dl = gtk::Label::new(Some("Duration"));
        dl.set_width_chars(9);
        dl.set_halign(gtk::Align::Start);
        let dur = gtk::SpinButton::with_range(0.1, 3600.0, 0.1);
        dur.set_value(scene.duration);
        dur.set_hexpand(true);
        dur_row.append(&dl);
        dur_row.append(&dur);
        form.append(&dur_row);
        {
            let this = self.clone();
            let project = project.clone();
            let scene_id = scene_id.to_string();
            dur.connect_value_changed(move |s| {
                let mut project = project.clone();
                if let Some(sc) = project.scenes.iter_mut().find(|x| x.id == scene_id) {
                    sc.duration = s.value();
                }
                this.commit_document(project);
            });
        }

        const KINDS: [&str; 7] =
            ["(none)", "crossfade", "wipe-lr", "wipe-tb", "box-wipe", "iris", "clock"];
        let kind_index = |tr: &Option<document::Transition>| -> u32 {
            match tr {
                None => 0,
                Some(t) => match t.kind {
                    document::TransitionKind::Crossfade => 1,
                    document::TransitionKind::WipeLr => 2,
                    document::TransitionKind::WipeTb => 3,
                    document::TransitionKind::BoxWipe => 4,
                    document::TransitionKind::Iris => 5,
                    document::TransitionKind::Clock => 6,
                },
            }
        };
        let tr_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let tl = gtk::Label::new(Some("Transition"));
        tl.set_width_chars(9);
        tl.set_halign(gtk::Align::Start);
        let dd = gtk::DropDown::from_strings(&KINDS);
        dd.set_selected(kind_index(&scene.transition));
        dd.set_hexpand(true);
        let tdur = gtk::SpinButton::with_range(0.1, 10.0, 0.1);
        tdur.set_value(scene.transition.as_ref().map_or(0.5, |t| t.duration));
        tr_row.append(&tl);
        tr_row.append(&dd);
        tr_row.append(&tdur);
        form.append(&tr_row);
        let order = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let index = project.scenes.iter().position(|s| s.id == scene.id).unwrap_or(0);
        for (label, delta) in [("← Move earlier", -1i64), ("Move later →", 1i64)] {
            let target = index as i64 + delta;
            if target < 0 || target >= project.scenes.len() as i64 {
                continue;
            }
            let b = gtk::Button::with_label(label);
            let this = self.clone();
            let project = project.clone();
            b.connect_clicked(move |_| {
                let mut project = project.clone();
                project.scenes.swap(index, target as usize);
                this.commit_document(project);
            });
            order.append(&b);
        }
        form.append(&order);

        let is_first = project.scenes.first().map(|s| s.id == scene.id).unwrap_or(false);
        if is_first {
            let hint = gtk::Label::new(Some("(first scene has no incoming transition)"));
            hint.add_css_class("dim-label");
            hint.set_halign(gtk::Align::Start);
            form.append(&hint);
            dd.set_sensitive(false);
            tdur.set_sensitive(false);
        } else {
            let apply = {
                let this = self.clone();
                let project = project.clone();
                let scene_id = scene_id.to_string();
                let dd = dd.clone();
                let tdur = tdur.clone();
                move || {
                    let mut project = project.clone();
                    if let Some(sc) = project.scenes.iter_mut().find(|x| x.id == scene_id) {
                        sc.transition = match dd.selected() {
                            0 => None,
                            k => Some(document::Transition {
                                kind: match k {
                                    1 => document::TransitionKind::Crossfade,
                                    2 => document::TransitionKind::WipeLr,
                                    3 => document::TransitionKind::WipeTb,
                                    4 => document::TransitionKind::BoxWipe,
                                    5 => document::TransitionKind::Iris,
                                    _ => document::TransitionKind::Clock,
                                },
                                duration: tdur.value(),
                            }),
                        };
                    }
                    this.commit_document(project);
                }
            };
            let a2 = apply.clone();
            dd.connect_selected_notify(move |_| apply());
            tdur.connect_value_changed(move |_| a2());
        }
        uiref.inspector.append(&form);
    }

    /// Templates tab: every def, instantiable at the playhead.
    fn rebuild_templates(self: &Rc<Self>) {
        let ui = self.ui.borrow();
        let Some(ui) = ui.as_ref() else { return };
        while let Some(child) = ui.templates_list.first_child() {
            ui.templates_list.remove(&child);
        }
        // Catalog = starter templates ∪ the project's own saved defs
        // (project entries win on name clash, e.g. a locally-edited
        // "title-card"). A def only enters the document when actually
        // inserted (#21) -- the Templates tab itself doesn't require
        // starter defs to be pre-baked into every project.
        let catalog: std::collections::BTreeMap<String, document::CompDef> = {
            let st = self.state.borrow();
            let mut merged = dualcut_engine::templates::starter_defs();
            if let Some(project) = st.project.as_ref() {
                merged.extend(project.defs.clone());
            }
            merged
        };
        let defs: Vec<(String, Vec<String>)> =
            catalog.iter().map(|(n, d)| (n.clone(), d.params.clone())).collect();
        let cache = self.cache_dir();
        let def_hashes: std::collections::BTreeMap<String, String> = catalog
            .iter()
            .map(|(n, d)| {
                (n.clone(), format!(
                    "tpl-{:016x}.png",
                    fx_hash(&serde_json::to_string(d).unwrap_or_default())
                ))
            })
            .collect();
        for (name, params) in defs {
            let row = gtk::Box::new(gtk::Orientation::Vertical, 4);
            if let Some(key) = def_hashes.get(&name) {
                let path = cache.join(key);
                if path.exists() {
                    let pic = gtk::Picture::for_filename(&path);
                    pic.set_size_request(-1, 90);
                    pic.set_content_fit(gtk::ContentFit::Contain);
                    row.append(&pic);
                }
            }
            row.set_margin_top(4);
            row.set_margin_bottom(4);
            row.set_margin_start(6);
            row.set_margin_end(6);
            let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let label = gtk::Label::new(Some(&name));
            label.set_halign(gtk::Align::Start);
            label.set_hexpand(true);
            label.add_css_class("heading");
            head.append(&label);
            let insert = gtk::Button::with_label("Insert");
            insert.add_css_class("flat");
            head.append(&insert);
            row.append(&head);
            let mut entries: Vec<(String, gtk::Entry)> = Vec::new();
            for p in &params {
                let entry = gtk::Entry::new();
                entry.set_placeholder_text(Some(p));
                row.append(&entry);
                entries.push((p.clone(), entry));
            }
            {
                let this = self.clone();
                let name = name.clone();
                insert.connect_clicked(move |_| {
                    let args: std::collections::BTreeMap<String, String> = entries
                        .iter()
                        .map(|(p, e)| {
                            let text = e.text().to_string();
                            (p.clone(), if text.is_empty() { p.clone() } else { text })
                        })
                        .collect();
                    this.insert_template(&name, args);
                });
            }
            let lbrow = gtk::ListBoxRow::new();
            lbrow.set_child(Some(&row));
            ui.templates_list.append(&lbrow);
        }
    }

    /// Insert a def instance into the scene under the playhead.
    fn insert_template(
        self: &Rc<Self>,
        name: &str,
        args: std::collections::BTreeMap<String, String>,
    ) {
        let (project, time) = {
            let st = self.state.borrow();
            let time = st
                .pipeline
                .query_position::<gst::ClockTime>()
                .map(|p| p.nseconds() as f64 / 1e9)
                .unwrap_or(0.0);
            (st.project.clone(), time)
        };
        let Some(mut project) = project else { return };
        // Copy the def in from the starter catalog on first use (#21):
        // the project only carries the defs it actually uses.
        if !project.defs.contains_key(name)
            && let Some(mut def) = dualcut_engine::templates::starter_defs().remove(name)
        {
            for layer in &mut def.layers {
                if let document::Element::Text { font, .. } = &mut layer.element {
                    *font = apply_default_font_family(font);
                }
            }
            project.defs.insert(name.to_string(), def);
        }
        let mut id = name.to_string();
        let mut n = 1;
        while find_clip(&project, &id).is_some() {
            id = format!("{name}-{n}");
            n += 1;
        }
        let index = (0..project.scenes.len())
            .rev()
            .find(|&i| time >= project.scene_offset(i))
            .unwrap_or(0);
        let offset = project.scene_offset(index);
        let scene = &mut project.scenes[index];
        let start = (time - offset).clamp(0.0, (scene.duration - 0.1).max(0.0));
        scene.layers.insert(
            0,
            document::Clip {
                id: id.clone(),
                start,
                duration: 0.0,
                element: document::Element::CompRef { r#ref: name.to_string(), args },
                transform: Default::default(),
                animations: Vec::new(),
                effects: Vec::new(),
            },
        );
        self.state.borrow_mut().selected = Some(id);
        self.commit_document(project);
    }

    /// Code tab: the document as editable JSON.
    fn refresh_code(self: &Rc<Self>) {
        let ui = self.ui.borrow();
        let Some(ui) = ui.as_ref() else { return };
        let st = self.state.borrow();
        if let Some(project) = st.project.as_ref() {
            ui.code_buffer.set_text(&project.to_json());
        }
    }

    fn rebuild_inspector(self: &Rc<Self>) {
        let (project, selected) = {
            let st = self.state.borrow();
            (st.project.clone(), st.selected.clone())
        };
        let ui = self.ui.borrow();
        let Some(uiref) = ui.as_ref() else { return };
        while let Some(child) = uiref.inspector.first_child() {
            uiref.inspector.remove(&child);
        }
        while let Some(child) = uiref.clips_box.first_child() {
            uiref.clips_box.remove(&child);
        }
        let Some(project) = project else {
            let hint = gtk::Label::new(Some("No project loaded.\nOpen with: dualcut project.json"));
            hint.add_css_class("dim-label");
            uiref.inspector.append(&hint);
            return;
        };

        // Clip list: a two-level tree (scene/track -> clips), collapsible
        // per group (#39). GtkListBox doesn't nest, so the tree is flattened
        // into rows with a non-selectable header row per group followed by
        // its (optionally hidden) clip rows -- simpler than wiring up
        // GtkTreeListModel for a hierarchy this shallow, same collapse UX.
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::Multiple);
        // Row index -> clip id; None for group header rows.
        let mut ids: Vec<Option<String>> = Vec::new();
        struct ClipGroup {
            label: String,
            group_id: String,
            clips: Vec<(String, String)>,
        }
        let mut groups: Vec<ClipGroup> = Vec::new();
        for scene in &project.scenes {
            let clips = scene.layers.iter().map(|c| (c.id.clone(), c.id.clone())).collect();
            groups.push(ClipGroup { label: scene.id.clone(), group_id: scene.id.clone(), clips });
        }
        for track in &project.overlays {
            let clips = track.clips.iter().map(|c| (c.id.clone(), c.id.clone())).collect();
            groups.push(ClipGroup {
                label: format!("〜 {}", track.id),
                group_id: track.id.clone(),
                clips,
            });
        }
        for ClipGroup { label, group_id, clips } in &groups {
            let has_selection = clips.iter().any(|(_, id)| Some(id) == selected.as_ref());
            if has_selection {
                self.clips_collapsed.borrow_mut().remove(group_id);
            }
            let collapsed = self.clips_collapsed.borrow().contains(group_id);

            let head_row = gtk::ListBoxRow::new();
            head_row.set_selectable(false);
            head_row.set_activatable(true);
            let head = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            head.set_margin_top(4);
            head.set_margin_bottom(4);
            head.set_margin_start(4);
            let icon = gtk::Image::from_icon_name(if collapsed {
                "pan-end-symbolic"
            } else {
                "pan-down-symbolic"
            });
            head.append(&icon);
            let l = gtk::Label::new(Some(&format!("{label} ({})", clips.len())));
            l.set_halign(gtk::Align::Start);
            l.add_css_class("heading");
            head.append(&l);
            head_row.set_child(Some(&head));
            list.append(&head_row);
            ids.push(None);
            {
                let this = self.clone();
                let group_id = group_id.clone();
                head_row.connect_activate(move |_| {
                    let mut c = this.clips_collapsed.borrow_mut();
                    if !c.insert(group_id.clone()) {
                        c.remove(&group_id);
                    }
                    drop(c);
                    this.rebuild_inspector();
                });
            }

            if collapsed {
                continue;
            }
            for (label, id) in clips {
                let row = gtk::ListBoxRow::new();
                let l = gtk::Label::new(Some(label));
                l.set_halign(gtk::Align::Start);
                l.set_margin_top(4);
                l.set_margin_bottom(4);
                l.set_margin_start(24);
                row.set_child(Some(&l));
                list.append(&row);
                ids.push(Some(id.clone()));
                if Some(id) == selected.as_ref() {
                    list.select_row(Some(&row));
                }
            }
        }
        {
            let this = self.clone();
            let ids = ids.clone();
            list.connect_selected_rows_changed(move |list| {
                let rows = list.selected_rows();
                let Some(last) = rows.last() else { return };
                let Some(id) = ids.get(last.index() as usize).cloned().flatten() else { return };
                let changed = {
                    let mut st = this.state.borrow_mut();
                    let changed = st.selected.as_ref() != Some(&id) && rows.len() == 1;
                    if rows.len() == 1 {
                        st.selected = Some(id);
                    }
                    changed
                };
                if changed {
                    glib::idle_add_local_once({
                        let this = this.clone();
                        move || this.rebuild_inspector()
                    });
                }
            });
        }
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_child(Some(&list));
        scroll.set_vexpand(true);
        scroll.set_min_content_height(160);
        uiref.clips_box.append(&scroll);

        // Multi-select ops (Ctrl/Shift-click rows first).
        let sel_ops = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let del_sel = gtk::Button::with_label("Delete selected");
        {
            let this = self.clone();
            let list = list.clone();
            let ids = ids.clone();
            let project_snapshot = project.clone();
            del_sel.connect_clicked(move |_| {
                let rows = list.selected_rows();
                if rows.is_empty() {
                    return;
                }
                let mut project = project_snapshot.clone();
                let count = rows.len();
                for row in rows {
                    // Header rows are non-selectable, so selected rows
                    // always carry an id.
                    if let Some(id) = ids.get(row.index() as usize).and_then(|o| o.as_ref()) {
                        remove_clip(&mut project, id);
                    }
                }
                this.state.borrow_mut().selected = None;
                this.commit_document(project);
                this.toast_undo(&format!(
                    "{count} clip{} deleted",
                    if count == 1 { "" } else { "s" }
                ));
            });
        }
        sel_ops.append(&del_sel);

        let tpl_name = gtk::Entry::new();
        tpl_name.set_placeholder_text(Some("template name"));
        tpl_name.set_hexpand(true);
        let save_tpl = gtk::Button::with_label("Save as template");
        {
            let this = self.clone();
            let list = list.clone();
            let ids = ids.clone();
            let project_snapshot = project.clone();
            let tpl_name = tpl_name.clone();
            save_tpl.connect_clicked(move |_| {
                let selected: Vec<String> = list
                    .selected_rows()
                    .iter()
                    .filter_map(|r| ids.get(r.index() as usize).cloned().flatten())
                    .collect();
                let name = tpl_name.text().to_string();
                let mut project = project_snapshot.clone();
                match save_as_def(&mut project, &selected, &name) {
                    Ok(()) => {
                        println!("saved template {name:?} ({} clips)", selected.len());
                        this.commit_document(project);
                    }
                    Err(e) => eprintln!("save template: {e}"),
                }
            });
        }
        sel_ops.append(&tpl_name);
        sel_ops.append(&save_tpl);
        uiref.clips_box.append(&sel_ops);

        // Editor form for the selected clip (or scene).
        let Some(selected) = selected else {
            let hint = gtk::Label::new(Some(
                "Select a clip (timeline, preview, or Clips tab)\nor a scene (ruler) to edit its parameters.",
            ));
            hint.add_css_class("dim-label");
            hint.set_wrap(true);
            hint.set_margin_top(24);
            uiref.inspector.append(&hint);
            return;
        };
        if let Some(scene_id) = selected.strip_prefix("scene:") {
            self.build_scene_form(uiref, &project, scene_id);
            return;
        }
        let Some(clip) = find_clip(&project, &selected).cloned() else { return };

        let form = gtk::Box::new(gtk::Orientation::Vertical, 6);
        form.set_margin_top(8);
        let title = gtk::Label::new(Some(&format!("Clip: {}", clip.id)));
        title.add_css_class("heading");
        title.set_halign(gtk::Align::Start);
        form.append(&title);

        let spin = |label: &str, value: f64, max: f64| -> (gtk::Box, gtk::SpinButton) {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let l = gtk::Label::new(Some(label));
            l.set_width_chars(9);
            l.set_halign(gtk::Align::Start);
            let s = gtk::SpinButton::with_range(0.0, max, 0.1);
            s.set_value(value);
            s.set_hexpand(true);
            row.append(&l);
            row.append(&s);
            (row, s)
        };

        let parent_scene = project.scenes.iter().find(|s| s.layers.iter().any(|c| c.id == clip.id));
        let eff_dur = effective_duration(&clip, parent_scene);
        let is_fill = clip.duration <= 0.0 && parent_scene.is_some();

        let (row_start, spin_start) = spin("Start", clip.start, 3600.0);
        let (row_dur, spin_dur) = spin("Duration", eff_dur, 3600.0);
        let fill_hint = if is_fill {
            let h = gtk::Label::new(Some("fills scene"));
            h.add_css_class("dim-label");
            h.add_css_class("caption");
            row_dur.append(&h);
            Some(h)
        } else {
            None
        };
        let dur_edited = Rc::new(Cell::new(false));
        {
            let dur_edited = dur_edited.clone();
            let fill_hint = fill_hint.clone();
            spin_dur.connect_value_changed(move |_| {
                dur_edited.set(true);
                if let Some(h) = fill_hint.as_ref() {
                    h.set_visible(false);
                }
            });
        }
        let (row_op, spin_op) = spin("Opacity", clip.transform.opacity, 1.0);
        form.append(&row_start);
        form.append(&row_dur);
        form.append(&row_op);

        let rate_widgets: Option<gtk::SpinButton> = match &clip.element {
            document::Element::Video { rate, .. } | document::Element::Audio { rate, .. } => {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                let l = gtk::Label::new(Some("Speed"));
                l.set_width_chars(9);
                l.set_halign(gtk::Align::Start);
                let s = gtk::SpinButton::with_range(0.1, 10.0, 0.1);
                s.set_value(*rate);
                s.set_hexpand(true);
                row.append(&l);
                row.append(&s);
                form.append(&row);
                Some(s)
            }
            _ => None,
        };

        type TextWidgets = (gtk::Entry, gtk::DropDown, gtk::Entry, gtk::Switch);
        let text_widgets: Option<TextWidgets> = match &clip.element {
            document::Element::Text { text, align, outline, shadow, .. } => {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                let l = gtk::Label::new(Some("Text"));
                l.set_width_chars(9);
                l.set_halign(gtk::Align::Start);
                let entry = gtk::Entry::new();
                entry.set_text(text);
                entry.set_hexpand(true);
                row.append(&l);
                row.append(&entry);
                form.append(&row);

                // Rich text styling (#38).
                let style_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                let al = gtk::Label::new(Some("Align"));
                al.set_width_chars(9);
                al.set_halign(gtk::Align::Start);
                style_row.append(&al);
                let align_dd = gtk::DropDown::from_strings(&["(auto)", "left", "center", "right"]);
                align_dd.set_selected(match align {
                    None => 0,
                    Some(document::TextAlign::Left) => 1,
                    Some(document::TextAlign::Center) => 2,
                    Some(document::TextAlign::Right) => 3,
                });
                style_row.append(&align_dd);
                let outline_entry = gtk::Entry::new();
                outline_entry.set_placeholder_text(Some("outline #rrggbb"));
                outline_entry.set_max_width_chars(10);
                if let Some(o) = outline {
                    outline_entry.set_text(o);
                }
                style_row.append(&outline_entry);
                let shadow_sw = gtk::Switch::new();
                shadow_sw.set_active(*shadow);
                shadow_sw.set_valign(gtk::Align::Center);
                shadow_sw.set_tooltip_text(Some("Drop shadow"));
                style_row.append(&gtk::Label::new(Some("Shadow")));
                style_row.append(&shadow_sw);
                form.append(&style_row);
                Some((entry, align_dd, outline_entry, shadow_sw))
            }
            _ => None,
        };

        let apply = gtk::Button::with_label("Apply");
        apply.add_css_class("suggested-action");
        {
            let this = self.clone();
            let id = clip.id.clone();
            let project = project.clone();
            let dur_edited = dur_edited.clone();
            apply.connect_clicked(move |_| {
                let mut project = project.clone();
                if let Some(clip) = find_clip_mut(&mut project, &id) {
                    clip.start = spin_start.value();
                    clip.duration = if is_fill && !dur_edited.get() {
                        0.0
                    } else {
                        spin_dur.value()
                    };
                    clip.transform.opacity = spin_op.value();
                    if let (
                        document::Element::Video { rate, .. } | document::Element::Audio { rate, .. },
                        Some(s),
                    ) = (&mut clip.element, rate_widgets.as_ref())
                    {
                        *rate = s.value();
                    }
                    if let (
                        document::Element::Text { text, align, outline, shadow, .. },
                        Some((entry, align_dd, outline_entry, shadow_sw)),
                    ) = (&mut clip.element, text_widgets.as_ref())
                    {
                        *text = entry.text().to_string();
                        *align = match align_dd.selected() {
                            1 => Some(document::TextAlign::Left),
                            2 => Some(document::TextAlign::Center),
                            3 => Some(document::TextAlign::Right),
                            _ => None,
                        };
                        let o = outline_entry.text().to_string();
                        *outline = if o.trim().is_empty() { None } else { Some(o) };
                        *shadow = shadow_sw.is_active();
                    }
                }
                this.commit_document(project);
            });
        }
        form.append(&apply);

        // ── Animations ─────────────────────────────────────────
        if !clip.animations.is_empty() {
            let anim_head = gtk::Label::new(Some("Keyframes"));
            anim_head.add_css_class("heading");
            anim_head.set_halign(gtk::Align::Start);
            anim_head.set_margin_top(8);
            form.append(&anim_head);
        }

        const PROPS: [&str; 6] = ["x", "y", "width", "height", "opacity", "volume"];
        const EASINGS: [&str; 4] = ["linear", "easeIn", "easeOut", "easeInOut"];
        let prop_of = |a: &document::AnimProperty| match a {
            document::AnimProperty::X => 0,
            document::AnimProperty::Y => 1,
            document::AnimProperty::Width => 2,
            document::AnimProperty::Height => 3,
            document::AnimProperty::Opacity => 4,
            document::AnimProperty::Volume => 5,
            // Rate animation isn't GUI-selectable yet (unsafe to
            // live-bind, see document::Effect validate()); shown as
            // Volume's slot if a project already has one (shouldn't
            // pass validate, but keep the match exhaustive).
            document::AnimProperty::Rate => 5,
        };
        let ease_of = |e: &document::Easing| match e {
            document::Easing::Linear => 0,
            document::Easing::EaseIn => 1,
            document::Easing::EaseOut => 2,
            document::Easing::EaseInOut => 3,
        };

        for (ai, anim) in clip.animations.iter().enumerate() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            let commit_anim = {
                let this = self.clone();
                let project = project.clone();
                let clip_id = clip.id.clone();
                Rc::new(move |mutate: Box<dyn Fn(&mut document::Anim)>| {
                    let mut project = project.clone();
                    if let Some(c) = find_clip_mut(&mut project, &clip_id)
                        && let Some(a) = c.animations.get_mut(ai) {
                            mutate(a);
                        }
                    this.commit_document(project);
                })
            };

            let prop = gtk::DropDown::from_strings(&PROPS);
            prop.set_selected(prop_of(&anim.property) as u32);
            {
                let commit = commit_anim.clone();
                prop.connect_selected_notify(move |dd| {
                    let value = match dd.selected() {
                        0 => document::AnimProperty::X,
                        1 => document::AnimProperty::Y,
                        2 => document::AnimProperty::Width,
                        3 => document::AnimProperty::Height,
                        4 => document::AnimProperty::Opacity,
                        _ => document::AnimProperty::Volume,
                    };
                    commit(Box::new(move |a| a.property = value));
                });
            }
            row.append(&prop);

            let add_spin = |value: f64, lo: f64, hi: f64, set: fn(&mut document::Anim, f64)| {
                let s = gtk::SpinButton::with_range(lo, hi, 0.1);
                s.set_value(value);
                s.set_width_chars(5);
                let commit = commit_anim.clone();
                s.connect_value_changed(move |s| {
                    let v = s.value();
                    commit(Box::new(move |a| set(a, v)));
                });
                row.append(&s);
            };
            add_spin(anim.from, -10000.0, 10000.0, |a, v| a.from = v);
            add_spin(anim.to, -10000.0, 10000.0, |a, v| a.to = v);
            add_spin(anim.start, 0.0, 3600.0, |a, v| a.start = v);
            add_spin(anim.end, 0.0, 3600.0, |a, v| a.end = v);

            let ease = gtk::DropDown::from_strings(&EASINGS);
            ease.set_selected(ease_of(&anim.easing) as u32);
            {
                let commit = commit_anim.clone();
                ease.connect_selected_notify(move |dd| {
                    let value = match dd.selected() {
                        1 => document::Easing::EaseIn,
                        2 => document::Easing::EaseOut,
                        3 => document::Easing::EaseInOut,
                        _ => document::Easing::Linear,
                    };
                    commit(Box::new(move |a| a.easing = value));
                });
            }
            row.append(&ease);

            let del = gtk::Button::from_icon_name("edit-delete-symbolic");
            del.update_property(&[gtk::accessible::Property::Label("Delete animation")]);
            del.add_css_class("flat");
            {
                let this = self.clone();
                let project = project.clone();
                let clip_id = clip.id.clone();
                del.connect_clicked(move |_| {
                    let mut project = project.clone();
                    if let Some(c) = find_clip_mut(&mut project, &clip_id)
                        && ai < c.animations.len() {
                            c.animations.remove(ai);
                        }
                    this.commit_document(project);
                });
            }
            row.append(&del);
            form.append(&row);
        }

        // Presets.
        let tr_head = gtk::Label::new(Some("Animations"));
        tr_head.add_css_class("heading");
        tr_head.set_halign(gtk::Align::Start);
        tr_head.set_margin_top(8);
        form.append(&tr_head);
        let presets = gtk::FlowBox::new();
        presets.set_selection_mode(gtk::SelectionMode::None);
        presets.set_max_children_per_line(3);
        presets.set_column_spacing(6);
        presets.set_row_spacing(6);
        let add_preset = |label: &str, make: fn(&document::Clip) -> document::Anim| {
            let b = gtk::Button::with_label(label);
            let this = self.clone();
            let project = project.clone();
            let clip_id = clip.id.clone();
            b.connect_clicked(move |_| {
                let mut project = project.clone();
                if let Some(c) = find_clip_mut(&mut project, &clip_id) {
                    let anim = make(c);
                    c.animations.push(anim);
                }
                this.commit_document(project);
            });
            presets.insert(&b, -1);
        };
        add_preset("+ Fade in", |_| document::Anim {
            property: document::AnimProperty::Opacity,
            from: 0.0, to: 1.0, start: 0.0, end: 0.5,
            easing: document::Easing::EaseOut,
            keyframes: Vec::new(),
        });
        add_preset("+ Fade out", |c| document::Anim {
            property: document::AnimProperty::Opacity,
            from: 1.0, to: 0.0,
            start: (c.duration - 0.5).max(0.0), end: c.duration.max(0.5),
            easing: document::Easing::EaseIn,
            keyframes: Vec::new(),
        });
        add_preset("+ Slide in", |c| document::Anim {
            property: document::AnimProperty::X,
            from: c.transform.x - 300.0, to: c.transform.x,
            start: 0.0, end: 0.6,
            easing: document::Easing::EaseOut,
            keyframes: Vec::new(),
        });
        if matches!(
            clip.element,
            document::Element::Audio { .. } | document::Element::Video { .. }
        ) {
            add_preset("+ Audio in", |_| document::Anim {
                property: document::AnimProperty::Volume,
                from: 0.0, to: 1.0, start: 0.0, end: 1.0,
                easing: document::Easing::EaseOut,
                keyframes: Vec::new(),
            });
            add_preset("+ Audio out", |c| document::Anim {
                property: document::AnimProperty::Volume,
                from: 1.0, to: 0.0,
                start: (c.duration - 1.0).max(0.0), end: c.duration.max(1.0),
                easing: document::Easing::EaseIn,
                keyframes: Vec::new(),
            });
        }
        form.append(&presets);

        // Effects.
        let fx_head = gtk::Label::new(Some("Effects"));
        fx_head.add_css_class("heading");
        fx_head.set_halign(gtk::Align::Start);
        fx_head.set_margin_top(8);
        form.append(&fx_head);
        for (fi, effect) in clip.effects.iter().enumerate() {
            // FlowBox instead of a plain Box (#44): wide effects (Crop, Color,
            // ChromaKey) pack several label+spinbutton pairs, which used to
            // force the whole right pane wider to fit one unbroken row.
            // Wrapping bounds each row's minimum width to one pair and lets
            // the rest flow onto additional lines instead.
            let row = gtk::FlowBox::new();
            row.set_selection_mode(gtk::SelectionMode::None);
            row.set_max_children_per_line(4);
            row.set_min_children_per_line(1);
            row.set_row_spacing(2);
            row.set_column_spacing(4);
            let pair = |a: &gtk::Widget, b: &gtk::Widget| {
                let p = gtk::Box::new(gtk::Orientation::Horizontal, 4);
                p.append(a);
                p.append(b);
                p
            };
            let commit_fx = {
                let this = self.clone();
                let project = project.clone();
                let clip_id = clip.id.clone();
                Rc::new(move |f: Box<dyn Fn(&mut document::Effect)>| {
                    let mut project = project.clone();
                    if let Some(c) = find_clip_mut(&mut project, &clip_id)
                        && let Some(e) = c.effects.get_mut(fi) {
                            f(e);
                        }
                    this.commit_document(project);
                })
            };
            let fx_spin = |label: &str, value: f64, min: f64, max: f64, step: f64| {
                let l = gtk::Label::new(Some(label));
                l.add_css_class("dim-label");
                let s = gtk::SpinButton::with_range(min, max, step);
                s.set_value(value);
                (l, s)
            };
            match effect {
                document::Effect::Blur { amount } => {
                    let (l, s) = fx_spin("Blur", *amount, 0.0, 50.0, 0.5);
                    row.append(&pair(l.upcast_ref(), s.upcast_ref()));
                    let commit_fx = commit_fx.clone();
                    s.connect_value_changed(move |s| {
                        let v = s.value();
                        commit_fx(Box::new(move |e| {
                            if let document::Effect::Blur { amount } = e {
                                *amount = v;
                            }
                        }));
                    });
                }
                document::Effect::ChromaKey { color, angle, noise } => {
                    let l = gtk::Label::new(Some("Key"));
                    l.add_css_class("dim-label");
                    let ce = gtk::Entry::new();
                    ce.set_text(color);
                    ce.set_max_width_chars(9);
                    row.append(&pair(l.upcast_ref(), ce.upcast_ref()));
                    {
                        let commit_fx = commit_fx.clone();
                        ce.connect_activate(move |e| {
                            let v = e.text().to_string();
                            commit_fx(Box::new(move |fx| {
                                if let document::Effect::ChromaKey { color, .. } = fx {
                                    *color = v.clone();
                                }
                            }));
                        });
                    }
                    for (name, val, min, max) in
                        [("Angle", *angle, 1.0, 90.0), ("Noise", *noise, 0.0, 64.0)]
                    {
                        let (l, s) = fx_spin(name, val, min, max, 1.0);
                        row.append(&pair(l.upcast_ref(), s.upcast_ref()));
                        let commit_fx = commit_fx.clone();
                        s.connect_value_changed(move |s| {
                            let v = s.value();
                            commit_fx(Box::new(move |fx| {
                                if let document::Effect::ChromaKey { angle, noise, .. } = fx {
                                    if name == "Angle" { *angle = v } else { *noise = v }
                                }
                            }));
                        });
                    }
                }
                document::Effect::Crop { left, right, top, bottom } => {
                    for (name, val) in
                        [("L", *left), ("R", *right), ("T", *top), ("B", *bottom)]
                    {
                        let (l, s) = fx_spin(name, val as f64, 0.0, 4000.0, 2.0);
                        row.append(&pair(l.upcast_ref(), s.upcast_ref()));
                        let commit_fx = commit_fx.clone();
                        s.connect_value_changed(move |s| {
                            let v = s.value() as i32;
                            commit_fx(Box::new(move |fx| {
                                if let document::Effect::Crop { left, right, top, bottom } = fx {
                                    match name {
                                        "L" => *left = v,
                                        "R" => *right = v,
                                        "T" => *top = v,
                                        _ => *bottom = v,
                                    }
                                }
                            }));
                        });
                    }
                }
                document::Effect::Eq { low, mid, high } => {
                    for (name, val) in [("Low", *low), ("Mid", *mid), ("High", *high)] {
                        let (l, s) = fx_spin(name, val, -24.0, 12.0, 0.5);
                        row.append(&pair(l.upcast_ref(), s.upcast_ref()));
                        let commit_fx = commit_fx.clone();
                        s.connect_value_changed(move |s| {
                            let v = s.value();
                            commit_fx(Box::new(move |fx| {
                                if let document::Effect::Eq { low, mid, high } = fx {
                                    match name {
                                        "Low" => *low = v,
                                        "Mid" => *mid = v,
                                        _ => *high = v,
                                    }
                                }
                            }));
                        });
                    }
                }
                document::Effect::Denoise { level } => {
                    let (l, s) = fx_spin("Level", *level as f64, 0.0, 3.0, 1.0);
                    row.append(&pair(l.upcast_ref(), s.upcast_ref()));
                    let commit_fx = commit_fx.clone();
                    s.connect_value_changed(move |s| {
                        let v = s.value() as u32;
                        commit_fx(Box::new(move |fx| {
                            if let document::Effect::Denoise { level } = fx {
                                *level = v;
                            }
                        }));
                    });
                }
                document::Effect::Compressor { threshold, ratio } => {
                    for (name, val, min, max, step) in [
                        ("Thresh", *threshold, 0.0, 1.0, 0.05),
                        ("Ratio", *ratio, 1.0, 4.0, 0.1),
                    ] {
                        let (l, s) = fx_spin(name, val, min, max, step);
                        row.append(&pair(l.upcast_ref(), s.upcast_ref()));
                        let commit_fx = commit_fx.clone();
                        s.connect_value_changed(move |s| {
                            let v = s.value();
                            commit_fx(Box::new(move |fx| {
                                if let document::Effect::Compressor { threshold, ratio } = fx {
                                    if name == "Thresh" { *threshold = v } else { *ratio = v }
                                }
                            }));
                        });
                    }
                }
                document::Effect::Color { brightness, contrast, saturation, hue } => {
                    for (name, val, min, max) in [
                        ("Bri", *brightness, -1.0, 1.0),
                        ("Con", *contrast, 0.0, 2.0),
                        ("Sat", *saturation, 0.0, 2.0),
                        ("Hue", *hue, -1.0, 1.0),
                    ] {
                        let (l, s) = fx_spin(name, val, min, max, 0.05);
                        row.append(&pair(l.upcast_ref(), s.upcast_ref()));
                        let commit_fx = commit_fx.clone();
                        s.connect_value_changed(move |s| {
                            let v = s.value();
                            commit_fx(Box::new(move |e| {
                                if let document::Effect::Color {
                                    brightness, contrast, saturation, hue,
                                } = e
                                {
                                    match name {
                                        "Bri" => *brightness = v,
                                        "Con" => *contrast = v,
                                        "Sat" => *saturation = v,
                                        _ => *hue = v,
                                    }
                                }
                            }));
                        });
                    }
                }
                document::Effect::Mask { shape, feather, invert } => {
                    const SHAPES: [&str; 7] =
                        ["rect", "circle", "ellipse", "star", "polygon", "line", "arrow"];
                    let shape_index = |s: document::ShapeKind| -> u32 {
                        match s {
                            document::ShapeKind::Rect => 0,
                            document::ShapeKind::Circle => 1,
                            document::ShapeKind::Ellipse => 2,
                            document::ShapeKind::Star => 3,
                            document::ShapeKind::Polygon => 4,
                            document::ShapeKind::Line => 5,
                            document::ShapeKind::Arrow => 6,
                        }
                    };
                    let l = gtk::Label::new(Some("Shape"));
                    l.add_css_class("dim-label");
                    let dd = gtk::DropDown::from_strings(&SHAPES);
                    dd.set_selected(shape_index(*shape));
                    row.append(&pair(l.upcast_ref(), dd.upcast_ref()));
                    {
                        let commit_fx = commit_fx.clone();
                        dd.connect_selected_notify(move |dd| {
                            let v = match dd.selected() {
                                0 => document::ShapeKind::Rect,
                                1 => document::ShapeKind::Circle,
                                2 => document::ShapeKind::Ellipse,
                                3 => document::ShapeKind::Star,
                                4 => document::ShapeKind::Polygon,
                                5 => document::ShapeKind::Line,
                                _ => document::ShapeKind::Arrow,
                            };
                            commit_fx(Box::new(move |fx| {
                                if let document::Effect::Mask { shape, .. } = fx {
                                    *shape = v;
                                }
                            }));
                        });
                    }
                    let (l, s) = fx_spin("Feather", *feather, 0.0, 50.0, 0.5);
                    row.append(&pair(l.upcast_ref(), s.upcast_ref()));
                    {
                        let commit_fx = commit_fx.clone();
                        s.connect_value_changed(move |s| {
                            let v = s.value();
                            commit_fx(Box::new(move |fx| {
                                if let document::Effect::Mask { feather, .. } = fx {
                                    *feather = v;
                                }
                            }));
                        });
                    }
                    let inv_check = gtk::CheckButton::with_label("Invert");
                    inv_check.set_active(*invert);
                    row.append(&inv_check);
                    inv_check.connect_toggled(move |c| {
                        let v = c.is_active();
                        commit_fx(Box::new(move |fx| {
                            if let document::Effect::Mask { invert, .. } = fx {
                                *invert = v;
                            }
                        }));
                    });
                }
            }
            let rm = gtk::Button::from_icon_name("window-close-symbolic");
            rm.update_property(&[gtk::accessible::Property::Label("Remove effect")]);
            rm.add_css_class("flat");
            {
                let this = self.clone();
                let project = project.clone();
                let clip_id = clip.id.clone();
                rm.connect_clicked(move |_| {
                    let mut project = project.clone();
                    if let Some(c) = find_clip_mut(&mut project, &clip_id)
                        && fi < c.effects.len() {
                            c.effects.remove(fi);
                        }
                    this.commit_document(project);
                });
            }
            row.append(&rm);
            form.append(&row);
        }
        // FlowBox, not a plain Box (#55, same fix as #44's effect param
        // rows): 7 buttons in one unbroken row forced the sidebar wider
        // to fit them all; wrapping bounds the row's minimum width.
        let fx_add = gtk::FlowBox::new();
        fx_add.set_selection_mode(gtk::SelectionMode::None);
        fx_add.set_max_children_per_line(3);
        fx_add.set_min_children_per_line(1);
        fx_add.set_row_spacing(4);
        fx_add.set_column_spacing(4);
        for (label, make) in [
            ("+ Blur", document::Effect::Blur { amount: 4.0 }),
            ("+ Color", document::Effect::Color {
                brightness: 0.0, contrast: 1.0, saturation: 1.0, hue: 0.0,
            }),
            ("+ Chroma key", document::Effect::ChromaKey {
                color: "#00ff00".into(), angle: 20.0, noise: 2.0,
            }),
            ("+ Crop", document::Effect::Crop { left: 0, right: 0, top: 0, bottom: 0 }),
            ("+ EQ", document::Effect::Eq { low: 0.0, mid: 0.0, high: 0.0 }),
            ("+ Compressor", document::Effect::Compressor { threshold: 0.25, ratio: 2.0 }),
            ("+ Denoise", document::Effect::Denoise { level: 1 }),
            ("+ Mask", document::Effect::Mask {
                shape: document::ShapeKind::Circle, feather: 0.0, invert: false,
            }),
        ] {
            let b = gtk::Button::with_label(label);
            let this = self.clone();
            let project = project.clone();
            let clip_id = clip.id.clone();
            let make = make.clone();
            b.connect_clicked(move |_| {
                let mut project = project.clone();
                if let Some(c) = find_clip_mut(&mut project, &clip_id) {
                    c.effects.push(make.clone());
                }
                this.commit_document(project);
            });
            fx_add.append(&b);
        }
        form.append(&fx_add);

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        if matches!(clip.element, document::Element::Video { .. }) {
            let detach = gtk::Button::with_label("Detach audio");
            {
                let this = self.clone();
                let id = clip.id.clone();
                let project = project.clone();
                detach.connect_clicked(move |_| {
                    let mut project = project.clone();
                    if let Some(new) = detach_audio(&mut project, &id) {
                        println!("detached audio -> clip {new:?}");
                        this.commit_document(project);
                    }
                });
            }
            actions.append(&detach);
        }
        if matches!(
            clip.element,
            document::Element::Video { .. } | document::Element::Audio { .. }
        ) {
            let remove_silence_btn = gtk::Button::with_label("Remove silences…");
            {
                let this = self.clone();
                let id = clip.id.clone();
                let project_snapshot = project.clone();
                let base_dir = self.base_dir();
                remove_silence_btn.connect_clicked(move |btn| {
                    let Some(clip) = find_clip(&project_snapshot, &id) else { return };
                    let src = match &clip.element {
                        document::Element::Video { src, .. } | document::Element::Audio { src, .. } => {
                            src.clone()
                        }
                        _ => return,
                    };
                    let Some(uri) = media_uri(&src, &base_dir) else { return };
                    btn.set_sensitive(false);
                    btn.set_label("Detecting…");
                    let (tx, rx) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        let result = dualcut_engine::silence::detect_silence_in_uri(&uri, -40.0, 0.5);
                        let _ = tx.send(result);
                    });
                    let this = this.clone();
                    let id = id.clone();
                    let project_snapshot = project_snapshot.clone();
                    let btn = btn.clone();
                    glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
                        let reset = |btn: &gtk::Button| {
                            btn.set_sensitive(true);
                            btn.set_label("Remove silences…");
                        };
                        match rx.try_recv() {
                            Ok(Ok(ranges)) => {
                                let mut project = project_snapshot.clone();
                                match document::remove_silence(&mut project, &id, &ranges) {
                                    Ok(0) => {
                                        reset(&btn);
                                        this.toast("No silence detected");
                                    }
                                    Ok(n) => {
                                        this.commit_document(project);
                                        this.toast_undo(&format!(
                                            "Removed {n} silent range{}",
                                            if n == 1 { "" } else { "s" }
                                        ));
                                    }
                                    Err(e) => {
                                        reset(&btn);
                                        this.toast(&format!("✗ {e}"));
                                    }
                                }
                                glib::ControlFlow::Break
                            }
                            Ok(Err(e)) => {
                                reset(&btn);
                                this.toast(&format!("✗ silence detection failed: {e:#}"));
                                glib::ControlFlow::Break
                            }
                            Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                            Err(_) => glib::ControlFlow::Break,
                        }
                    });
                });
            }
            actions.append(&remove_silence_btn);
        }
        let delete = gtk::Button::with_label("Delete clip");
        delete.add_css_class("destructive-action");
        {
            let this = self.clone();
            let id = clip.id.clone();
            let project = project.clone();
            delete.connect_clicked(move |_| {
                let mut project = project.clone();
                remove_clip(&mut project, &id);
                this.state.borrow_mut().selected = None;
                this.commit_document(project);
                this.toast_undo(&format!("Clip {id:?} deleted"));
            });
        }
        actions.append(&delete);
        form.append(&actions);
        uiref.inspector.append(&form);
    }

    /// Edit History panel (newest first): who made each change (GUI /
    /// agent HTTP request / external file edit) and a coarse summary.
    /// Clicking an entry jumps the document straight to that state.
    fn rebuild_history(self: &Rc<Self>) {
        let entries = self.state.borrow().history.clone();
        let ui = self.ui.borrow();
        let Some(uiref) = ui.as_ref() else { return };
        while let Some(child) = uiref.history_box.first_child() {
            uiref.history_box.remove(&child);
        }
        if entries.is_empty() {
            let hint = gtk::Label::new(Some("No edits yet."));
            hint.add_css_class("dim-label");
            hint.set_margin_top(24);
            uiref.history_box.append(&hint);
            return;
        }
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::None);
        for (i, entry) in entries.iter().enumerate().rev() {
            let row = gtk::ListBoxRow::new();
            row.set_activatable(true);
            let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            hbox.set_margin_top(6);
            hbox.set_margin_bottom(6);
            hbox.set_margin_start(8);
            hbox.set_margin_end(8);
            let (icon_name, tooltip) = match entry.source {
                EditSource::Gui => ("avatar-default-symbolic", "Edited in the app"),
                EditSource::Agent => ("network-server-symbolic", "Edited by an agent (HTTP API)"),
                EditSource::ExternalFile => ("text-x-generic-symbolic", "Edited outside the app"),
            };
            let icon = gtk::Image::from_icon_name(icon_name);
            icon.set_tooltip_text(Some(tooltip));
            hbox.append(&icon);
            let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let summary_l = gtk::Label::new(Some(&entry.summary));
            summary_l.set_halign(gtk::Align::Start);
            vbox.append(&summary_l);
            let secs = entry.at.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let time_l = gtk::Label::new(Some(
                &glib::DateTime::from_unix_local(secs as i64)
                    .and_then(|d| d.format("%H:%M:%S"))
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
            ));
            time_l.add_css_class("dim-label");
            time_l.add_css_class("caption");
            time_l.set_halign(gtk::Align::Start);
            vbox.append(&time_l);
            hbox.append(&vbox);
            row.set_child(Some(&hbox));
            list.append(&row);
            {
                let this = self.clone();
                let idx = i;
                let click = gtk::GestureClick::new();
                click.connect_released(move |_, _, _, _| this.jump_to_history(idx));
                row.add_controller(click);
            }
        }
        uiref.history_box.append(&list);
    }
}


fn show_about(window: Option<&gtk::Window>) {
    let about = adw::AboutDialog::builder()
        .application_name("Dualcut")
        .application_icon("org.tunaos.dualcut")
        .version(env!("CARGO_PKG_VERSION"))
        .developer_name("James Reilly")
        .developers(["James Reilly (hanthor)", "KiKaraage"])
        .website("https://github.com/tuna-os/dualcut")
        .issue_url("https://github.com/tuna-os/dualcut/issues")
        .comments("Dual-mode video editor: a GUI for humans and a JSON/TypeScript surface for agents, on one GStreamer Editing Services engine.")
        .build();
    about.present(window);
}


#[cfg(feature = "scripting")]
fn build_script_panel(editor: &Rc<Editor>) -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 6);
    page.set_margin_top(8);
    page.set_margin_start(8);
    page.set_margin_end(8);
    page.set_margin_bottom(8);

    let hint = gtk::Label::new(Some(
        "Automate edits with TypeScript. The Code tab is the document \
itself (JSON); a script here transforms it: it receives the current \
project and the returned project becomes the new document (undoable). \
Must export edit(project: Project): Project. \
Types: engine/schema/dualcut.d.ts",
    ));
    hint.add_css_class("dim-label");
    hint.set_halign(gtk::Align::Start);
    hint.set_wrap(true);
    page.append(&hint);

    let buffer = gtk::TextBuffer::new(None);
    buffer.set_text(
        "export function edit(project) {\n  // e.g. retitle every scene:\n  // project.scenes.forEach((s, i) => s.name = `Scene ${i + 1}`);\n  return project;\n}\n",
    );
    let view = gtk::TextView::with_buffer(&buffer);
    view.set_monospace(true);
    view.set_vexpand(true);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_child(Some(&view));
    scroll.set_vexpand(true);
    page.append(&scroll);

    let status = gtk::Label::new(None);
    status.set_halign(gtk::Align::Start);
    status.set_wrap(true);
    let run = gtk::Button::with_label("Run script");
    run.add_css_class("suggested-action");
    {
        let editor = editor.clone();
        let status = status.clone();
        run.connect_clicked(move |_| {
            let source = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string();
            let project = {
                let st = editor.state.borrow();
                st.project.clone()
            };
            let Some(project) = project else {
                status.set_text("no project loaded");
                return;
            };
            match dualcut_engine::scripting::run_script(&source, &project) {
                Ok(edited) => {
                    status.set_text("✓ applied");
                    editor.commit_document(edited);
                }
                Err(e) => status.set_text(&format!("✗ {e:#}")),
            }
        });
    }
    page.append(&run);
    page.append(&status);
    page
}


fn apply_captions(editor: &Rc<Editor>, segments: &[(f64, f64, String)]) {
    let project = editor.state.borrow().project.clone();
    let Some(mut project) = project else { return };
    let mut clips = dualcut_engine::captions_to_clips(segments);
    if clips.is_empty() {
        editor.toast("No speech found");
        return;
    }
    // Clip ids are unique document-wide: skip past any existing sub-N.
    let used: std::collections::HashSet<String> = project
        .scenes
        .iter()
        .flat_map(|s| s.layers.iter())
        .chain(project.overlays.iter().flat_map(|t| t.clips.iter()))
        .map(|c| c.id.clone())
        .collect();
    let mut next = 0usize;
    for clip in &mut clips {
        while used.contains(&format!("sub-{next}")) {
            next += 1;
        }
        clip.id = format!("sub-{next}");
        next += 1;
    }
    let count = clips.len();
    match project.overlays.iter_mut().find(|t| t.id == "subtitles") {
        Some(track) => track.clips.extend(clips),
        None => project.overlays.push(document::OverlayTrack {
            id: "subtitles".to_string(),
            muted: false,
            hidden: false,
            locked: false,
            name: "Subtitles".to_string(),
            clips,
        }),
    }
    editor.commit_document(project);
    editor.toast(&format!("✓ {count} captions added"));
}

/// Karaoke-style captions (#53): word-level segments in, rasterized
/// highlight-state clips out (`karaoke::karaoke_captions_to_clips`).
/// Rasterizing is real work (one Pango/Cairo render per word), so it runs
/// on a worker thread just like the transcription step it follows, not
/// inline on the GTK main thread.
fn apply_karaoke_captions(editor: &Rc<Editor>, segments: &[(f64, f64, String)]) {
    let (project, cache_dir) = {
        let st = editor.state.borrow();
        let Some(project) = st.project.clone() else { return };
        (project, editor.cache_dir())
    };
    if segments.is_empty() {
        editor.toast("No speech found");
        return;
    }
    let words = segments.to_vec();
    let (w, h) = (project.meta.width as u32, project.meta.height as u32);
    let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<Vec<document::Clip>>>();
    std::thread::spawn(move || {
        let cache = cache_dir;
        let _ = tx.send(dualcut_engine::karaoke::karaoke_captions_to_clips(&words, &cache, w, h));
    });
    let editor = editor.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
        match rx.try_recv() {
            Ok(Ok(mut clips)) => {
                let project = editor.state.borrow().project.clone();
                let Some(mut project) = project else { return glib::ControlFlow::Break };
                let used: std::collections::HashSet<String> = project
                    .scenes
                    .iter()
                    .flat_map(|s| s.layers.iter())
                    .chain(project.overlays.iter().flat_map(|t| t.clips.iter()))
                    .map(|c| c.id.clone())
                    .collect();
                for (i, clip) in clips.iter_mut().enumerate() {
                    let mut id = format!("sub-k{i}");
                    let mut n = i;
                    while used.contains(&id) {
                        n += 1;
                        id = format!("sub-k{n}");
                    }
                    clip.id = id;
                }
                let count = clips.len();
                match project.overlays.iter_mut().find(|t| t.id == "subtitles") {
                    Some(track) => track.clips.extend(clips),
                    None => project.overlays.push(document::OverlayTrack {
                        id: "subtitles".to_string(),
                        muted: false,
                        hidden: false,
                        locked: false,
                        name: "Subtitles".to_string(),
                        clips,
                    }),
                }
                editor.commit_document(project);
                editor.toast(&format!("✓ {count} karaoke captions added"));
                glib::ControlFlow::Break
            }
            Ok(Err(e)) => {
                eprintln!("karaoke captions: {e:#}");
                editor.toast(&format!("✗ karaoke captions failed: {e}"));
                glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(_) => glib::ControlFlow::Break,
        }
    });
}

/// Auto-captions GUI flow (#37): explain, confirm, transcribe on a worker
/// thread, then commit the subtitle clips as one undoable mutation.
fn show_captions_dialog(editor: &Rc<Editor>) {
    let Some(whisper) = find_whisper() else {
        editor.toast("No whisper-cli or whisper-cpp found on PATH");
        return;
    };
    let (project_json, base_dir) = {
        let st = editor.state.borrow();
        let Some(project) = st.project.as_ref() else { return };
        (project.to_json(), editor.base_dir())
    };
    let whisper_name = whisper
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("whisper")
        .to_string();
    let dialog = adw::AlertDialog::new(
        Some("Generate captions?"),
        Some(&format!(
            "Dualcut exports the project audio, transcribes it locally with \
             {whisper_name}, and adds the segments as text clips on a \
             Subtitles overlay track.\n\nUses the bundled speech model by \
             default; set the DUALCUT_WHISPER_MODEL environment variable \
             to a ggml model file to use a different one."
        )),
    );
    let style = gtk::DropDown::from_strings(&["Phrases", "Pop-on-word", "Karaoke (word highlight)"]);
    style.set_selected(0);
    style.set_tooltip_text(Some(
        "Phrases: whole sentences at once.\n\
         Pop-on-word: each word appears on its own, tightly timed \
         (TikTok/CapCut-style).\n\
         Karaoke: the whole line stays visible with the active word \
         highlighted, like YouTube/CapCut auto-captions.",
    ));
    dialog.set_extra_child(Some(&style));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("generate", "Generate");
    dialog.set_response_appearance("generate", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("generate"));
    let this = editor.clone();
    dialog.connect_response(Some("generate"), move |_, _| {
        let Some(model) = find_whisper_model() else {
            this.toast(
                "No whisper model found; set DUALCUT_WHISPER_MODEL to a ggml model path",
            );
            return;
        };
        this.toast("Transcribing… captions will appear when ready");
        let (tx, rx) =
            std::sync::mpsc::channel::<std::result::Result<Vec<(f64, f64, String)>, String>>();
        // Pop-on-word and karaoke both need word-level timestamps; only
        // "Phrases" (index 0) uses whisper's own sentence segmentation.
        let selected = style.selected();
        let word_level = selected != 0;
        {
            let project_json = project_json.clone();
            let base_dir = base_dir.clone();
            let whisper = whisper.clone();
            std::thread::spawn(move || {
                let _ = tx.send(run_captions_job(project_json, base_dir, whisper, model, word_level));
            });
        }
        let this = this.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
            match rx.try_recv() {
                Ok(Ok(segments)) => {
                    if selected == 2 {
                        apply_karaoke_captions(&this, &segments);
                    } else {
                        apply_captions(&this, &segments);
                    }
                    glib::ControlFlow::Break
                }
                Ok(Err(e)) => {
                    eprintln!("captions: {e}");
                    this.toast(&format!("✗ captions failed: {e}"));
                    glib::ControlFlow::Break
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(_) => glib::ControlFlow::Break,
            }
        });
    });
    dialog.present(editor.window().as_ref());
}


fn build_ui(app: &adw::Application) -> Result<()> {
    init()?;
    gstgtk4::plugin_register_static().context("registering gtk4paintablesink")?;

    // Show a loading window while the project file is being parsed and
    // compiled — large projects can take 30s–1min with no feedback.
    let loading = adw::Window::builder()
        .title("Dualcut")
        .default_width(360)
        .default_height(180)
        .resizable(false)
        .deletable(false)
        .build();
    let spinner = gtk::Spinner::new();
    spinner.start();
    spinner.set_size_request(48, 48);
    let label = gtk::Label::new(Some("Opening project\u{2026}"));
    label.add_css_class("title-4");
    let loading_content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    loading_content.set_valign(gtk::Align::Center);
    loading_content.set_halign(gtk::Align::Center);
    loading_content.append(&spinner);
    loading_content.append(&label);
    loading.set_content(Some(&loading_content));
    loading.present();

    let arg = std::env::args().nth(1);
    let (timeline, project, project_path, duration) = match &arg {
        Some(path) if path.ends_with(".json") => {
            let path = PathBuf::from(path);
            let json = std::fs::read_to_string(&path)?;
            let project = Project::from_json(&json)?;
            let base_dir = path.parent().unwrap_or(std::path::Path::new(".")).to_path_buf();
            let timeline = compile_project(&project, &base_dir)?;
            let duration = project.duration();
            (timeline, Some(project), Some(path), duration)
        }
        Some(other) => (build_demo_timeline(Some(other.as_str()))?, None, None, 8.0),
        None => {
            // No file argument: start an unsaved "New Project" (#15).
            let project = dualcut_engine::templates::new_project("New Project");
            let timeline = compile_project(&project, std::path::Path::new("."))?;
            let duration = project.duration();
            (timeline, Some(project), None, duration)
        }
    };

    let (pipeline, paintable) = make_pipeline(&timeline)?;
    let window_title = match &project_path {
        Some(p) => format!(
            "Dualcut — {}",
            p.file_name().and_then(|n| n.to_str()).unwrap_or("project")
        ),
        None => "Dualcut — New Project (unsaved)".to_string(),
    };
    let mtime = project_path.as_ref().and_then(|p| p.metadata().ok()?.modified().ok());

    // Agent surface: HTTP API over the project file (the mtime watcher
    // makes agent edits appear live in the UI). DUALCUT_API_PORT=0 disables.
    if let Some(path) = project_path.clone() {
        let port: u16 = std::env::var("DUALCUT_API_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(7357);
        if port != 0 {
            std::thread::spawn(move || {
                if let Err(e) = dualcut_engine::api::serve_file_api(path, port) {
                    eprintln!("agent API not available: {e:#}");
                }
            });
        }
    }

    let editor = Rc::new(Editor {
        state: Rc::new(RefCell::new(AppState {
            pipeline: pipeline.clone(),
            project,
            project_path,
            mtime,
            duration,
            selected: None,
            pps: DEFAULT_PPS,
            history: Vec::new(),
            redo: Vec::new(),
            self_write: false,
        })),
        ui: RefCell::new(None),
        exporting: Cell::new(false),
        export_queue: RefCell::new(VecDeque::new()),
        clips_collapsed: RefCell::new(std::collections::HashSet::new()),
        pending_proxies: RefCell::new(std::collections::HashSet::new()),
    });

    let picture = gtk::Picture::builder()
        .paintable(&paintable)
        .content_fit(gtk::ContentFit::Contain)
        .hexpand(true)
        .vexpand(true)
        .build();

    // Selection overlay: draws the selected clip's box; gestures select and
    // move clips directly in the preview.
    let sel_canvas = gtk::DrawingArea::new();
    sel_canvas.set_hexpand(true);
    sel_canvas.set_vexpand(true);
    let preview_overlay = gtk::Overlay::new();
    preview_overlay.set_child(Some(&picture));
    preview_overlay.add_overlay(&sel_canvas);
    {
        let editor = editor.clone();
        sel_canvas.set_draw_func(move |area, cr, w, h| {
            let st = editor.state.borrow();
            let Some(project) = st.project.as_ref() else { return };
            let Some(selected) = st.selected.as_ref() else { return };
            let time = st
                .pipeline
                .query_position::<gst::ClockTime>()
                .map(|p| p.nseconds() as f64 / 1e9)
                .unwrap_or(0.0);
            let _ = area;
            for (id, x, y, bw, bh) in active_clips_at(project, time) {
                if &id != selected {
                    continue;
                }
                let (cw, ch) = (project.meta.width as f64, project.meta.height as f64);
                let scale = (w as f64 / cw).min(h as f64 / ch);
                let (ox, oy) = ((w as f64 - cw * scale) / 2.0, (h as f64 - ch * scale) / 2.0);
                cr.set_source_rgba(0.35, 0.41, 1.0, 0.95);
                cr.set_line_width(2.0);
                cr.rectangle(ox + x * scale, oy + y * scale, bw * scale, bh * scale);
                let _ = cr.stroke();
                // corner handles
                cr.set_source_rgba(0.35, 0.41, 1.0, 1.0);
                for (hx, hy) in [
                    (x, y), (x + bw, y), (x, y + bh), (x + bw, y + bh),
                ] {
                    cr.rectangle(ox + hx * scale - 4.0, oy + hy * scale - 4.0, 8.0, 8.0);
                    let _ = cr.fill();
                }
            }
        });
    }
    {
        // Click to select the topmost/smallest clip under the pointer.
        let editor = editor.clone();
        let canvas = sel_canvas.clone();
        let click = gtk::GestureClick::new();
        click.connect_pressed(move |g, _, wx, wy| {
            let widget = g.widget().unwrap();
            let (w, h) = (widget.width() as f64, widget.height() as f64);
            let hit = {
                let st = editor.state.borrow();
                let Some(project) = st.project.as_ref() else { return };
                let Some((cx, cy, _)) = widget_to_comp(project, w, h, wx, wy) else { return };
                let time = st
                    .pipeline
                    .query_position::<gst::ClockTime>()
                    .map(|p| p.nseconds() as f64 / 1e9)
                    .unwrap_or(0.0);
                active_clips_at(project, time)
                    .into_iter()
                    .filter(|(_, x, y, bw, bh)| cx >= *x && cx <= x + bw && cy >= *y && cy <= y + bh)
                    .min_by(|a, b| (a.3 * a.4).total_cmp(&(b.3 * b.4)))
                    .map(|(id, ..)| id)
            };
            if let Some(id) = hit {
                editor.state.borrow_mut().selected = Some(id);
                editor.rebuild_inspector();
                canvas.queue_draw();
            }
        });
        sel_canvas.add_controller(click);
    }
    {
        // Drag the selected clip to move it (commits on release).
        let editor = editor.clone();
        let canvas = sel_canvas.clone();
        let drag = gtk::GestureDrag::new();
        let orig: Rc<std::cell::Cell<(f64, f64, f64)>> = Rc::new(std::cell::Cell::new((0.0, 0.0, 1.0)));
        {
            let editor = editor.clone();
            let orig = orig.clone();
            drag.connect_drag_begin(move |g, _, _| {
                let widget = g.widget().unwrap();
                let (w, h) = (widget.width() as f64, widget.height() as f64);
                let st = editor.state.borrow();
                let (Some(project), Some(selected)) = (st.project.as_ref(), st.selected.as_ref())
                else {
                    return;
                };
                let scale = {
                    let (cw, ch) = (project.meta.width as f64, project.meta.height as f64);
                    (w / cw).min(h / ch)
                };
                if let Some(clip) = find_clip(project, selected) {
                    orig.set((clip.transform.x, clip.transform.y, scale));
                }
            });
        }
        {
            let editor = editor.clone();
            let orig = orig.clone();
            drag.connect_drag_end(move |_, dx, dy| {
                if dx.abs() < 6.0 && dy.abs() < 6.0 {
                    return;
                }
                let (ox, oy, scale) = orig.get();
                if scale <= 0.0 {
                    return;
                }
                let (project, selected) = {
                    let st = editor.state.borrow();
                    (st.project.clone(), st.selected.clone())
                };
                let (Some(project), Some(selected)) = (project, selected) else { return };
                let mut project = project;
                if let Some(clip) = find_clip_mut(&mut project, &selected) {
                    clip.transform.x = (ox + dx / scale).round();
                    clip.transform.y = (oy + dy / scale).round();
                }
                editor.commit_document(project);
                canvas.queue_draw();
            });
        }
        sel_canvas.add_controller(drag);
    }

    let play = gtk::Button::from_icon_name("media-playback-start-symbolic");
    play.update_property(&[gtk::accessible::Property::Label("Play/Pause")]);
    let time_label = gtk::Label::new(Some("0:00.0"));
    time_label.add_css_class("numeric");
    let seek = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, duration.max(0.1), 0.05);
    seek.set_hexpand(true);
    seek.set_draw_value(false);

    {
        let editor = editor.clone();
        play.connect_clicked(move |btn| {
            let pipeline = editor.state.borrow().pipeline.clone();
            let playing = pipeline.current_state() == gst::State::Playing;
            let next = if playing { gst::State::Paused } else { gst::State::Playing };
            // Only flip the icon (and thus the perceived play/pause state)
            // if the transition actually succeeds -- previously this ran
            // unconditionally, so a failed set_state (bad proxy, missing
            // plugin, etc.) left playback silently dead while the button
            // kept toggling as if it worked (#57).
            match pipeline.set_state(next) {
                Ok(_) => {
                    btn.set_icon_name(if playing {
                        "media-playback-start-symbolic"
                    } else {
                        "media-playback-pause-symbolic"
                    });
                }
                Err(e) => {
                    eprintln!("playback: set_state({next:?}) failed: {e}");
                    editor.toast("Playback failed to start -- check the terminal for details");
                }
            }
        });
    }
    {
        let editor = editor.clone();
        seek.connect_change_value(move |_, _, value| {
            seek_to(&editor.state.borrow().pipeline, value);
            glib::Propagation::Proceed
        });
    }

    // Position updates + external live reload.
    {
        let editor = editor.clone();
        let seek = seek.clone();
        let time_label = time_label.clone();
        let sel_canvas = sel_canvas.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
            // Drain GStreamer bus errors so async pipeline failures surface to the user (#165).
            {
                let pipeline = editor.state.borrow().pipeline.clone();
                editor.drain_bus_errors(&pipeline);
            }

            // Redraw the selection overlay only while something is selected.
            if editor.state.borrow().selected.is_some() {
                sel_canvas.queue_draw();
            }
            // Keep the ruler playhead moving.
            if let Some(ui) = editor.ui.borrow().as_ref()
                && let Some(ruler) = ui.ruler.borrow().as_ref() {
                    ruler.queue_draw();
                }
            {
                let st = editor.state.borrow();
                if let Some(pos) = st.pipeline.query_position::<gst::ClockTime>() {
                    let secs = pos.nseconds() as f64 / 1e9;
                    seek.set_value(secs);
                    time_label.set_text(&format!(
                        "{}:{:04.1} / {}:{:04.1}",
                        (secs / 60.0) as u32,
                        secs % 60.0,
                        (st.duration / 60.0) as u32,
                        st.duration % 60.0
                    ));
                }
            }
            let reload = {
                let mut st = editor.state.borrow_mut();
                match st.project_path.clone() {
                    Some(path) => {
                        let new_mtime = path.metadata().ok().and_then(|m| m.modified().ok());
                        if new_mtime.is_some() && new_mtime != st.mtime {
                            st.mtime = new_mtime;
                            if st.self_write {
                                st.self_write = false;
                                None
                            } else {
                                Some(path)
                            }
                        } else {
                            None
                        }
                    }
                    None => None,
                }
            };
            if let Some(path) = reload {
                match std::fs::read_to_string(&path)
                    .map_err(anyhow::Error::from)
                    .and_then(|j| Project::from_json(&j))
                {
                    Ok(project) => {
                        let (source, summary) = take_agent_marker(&path)
                            .unwrap_or((EditSource::ExternalFile, "Edited outside the app".into()));
                        let prev = editor.state.borrow().project.clone();
                        if let Some(prev) = prev {
                            let mut st = editor.state.borrow_mut();
                            st.history.push(HistoryEntry {
                                snapshot: prev.to_json(),
                                source,
                                summary,
                                at: SystemTime::now(),
                            });
                            st.redo.clear();
                        }
                        editor.state.borrow_mut().project = Some(project);
                        editor.rebuild();
                        println!("project reloaded from disk");
                    }
                    Err(e) => eprintln!("reload failed (keeping current timeline): {e:#}"),
                }
            }
            glib::ControlFlow::Continue
        });
    }

    let bar = adw::HeaderBar::new();

    let left_toggle = gtk::ToggleButton::new();
    left_toggle.set_icon_name("sidebar-show-symbolic");
    left_toggle.update_property(&[gtk::accessible::Property::Label("Toggle left panel")]);
    left_toggle.set_tooltip_text(Some("Toggle left panel (Library / Templates / Code / Script)"));
    left_toggle.set_active(true);
    bar.pack_start(&left_toggle);

    // Open (split button with recents) + Import (#15).
    let open_btn = adw::SplitButton::new();
    open_btn.set_label("Open");
    {
        let editor = editor.clone();
        open_btn.connect_clicked(move |btn| {
            let window = btn.root().and_downcast::<gtk::Window>();
            let dialog = gtk::FileDialog::builder().title("Open project").build();
            let filter = gtk::FileFilter::new();
            filter.add_suffix("json");
            let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
            filters.append(&filter);
            dialog.set_filters(Some(&filters));
            let editor = editor.clone();
            dialog.open(window.as_ref(), gtk::gio::Cancellable::NONE, move |res| {
                if let Ok(file) = res
                    && let Some(path) = file.path() {
                        editor.open_project(&path);
                    }
            });
        });
    }
    {
        // Recents popover; placeholder row when there is no history yet.
        let pop = gtk::Popover::new();
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        list.add_css_class("boxed-list");
        let recents = load_recents();
        if recents.is_empty() {
            let empty = gtk::Label::new(Some("No recent projects"));
            empty.add_css_class("dim-label");
            empty.set_margin_top(8);
            empty.set_margin_bottom(8);
            empty.set_margin_start(12);
            empty.set_margin_end(12);
            let lbrow = gtk::ListBoxRow::new();
            lbrow.set_activatable(false);
            lbrow.set_child(Some(&empty));
            list.append(&lbrow);
        }
        for path in recents {
            let row = gtk::Button::with_label(
                path.file_name().and_then(|n| n.to_str()).unwrap_or("project"),
            );
            row.add_css_class("flat");
            row.set_tooltip_text(path.to_str());
            let editor = editor.clone();
            let pop2 = pop.clone();
            row.connect_clicked(move |_| {
                pop2.popdown();
                editor.open_project(&path);
            });
            let lbrow = gtk::ListBoxRow::new();
            lbrow.set_child(Some(&row));
            list.append(&lbrow);
        }
        pop.set_child(Some(&list));
        open_btn.set_popover(Some(&pop));
    }
    bar.pack_start(&open_btn);
    let import_btn = gtk::Button::with_label("Import");
    import_btn.add_css_class("flat");
    import_btn.set_tooltip_text(Some("Import media into the library"));
    {
        let editor = editor.clone();
        import_btn.connect_clicked(move |btn| {
            let window = btn.root().and_downcast::<gtk::Window>();
            editor.import_media(window.as_ref());
        });
    }
    bar.pack_start(&import_btn);
    let timeline_toggle = gtk::ToggleButton::new();
    timeline_toggle.set_icon_name("view-continuous-symbolic");
    timeline_toggle.update_property(&[gtk::accessible::Property::Label("Toggle timeline pane")]);
    timeline_toggle.set_tooltip_text(Some("Toggle timeline pane"));
    timeline_toggle.set_active(true);
    bar.pack_start(&timeline_toggle);
    let export = gtk::Button::new();
    let export_content = adw::ButtonContent::builder()
        .icon_name("document-save-symbolic")
        .label("Export")
        .build();
    export.set_child(Some(&export_content));
    export.add_css_class("flat");
    export.update_property(&[gtk::accessible::Property::Label("Export video")]);
    export.set_tooltip_text(Some("Export video"));
    {
        let editor = editor.clone();
        export.connect_clicked(move |btn| {
            let window = btn.root().and_downcast::<gtk::Window>();
            show_export_dialog(&editor, window.as_ref());
        });
    }

    let menu = gtk::gio::Menu::new();
    menu.append(Some("New Project"), Some("app.new-project"));
    menu.append(Some("New Vertical Project (9:16)"), Some("app.new-vertical-project"));
    menu.append(Some("Save Project As…"), Some("app.save-as"));
    menu.append(Some("Generate Captions…"), Some("app.captions"));
    menu.append(Some("Install Agent Skills…"), Some("app.install-skills"));
    menu.append(Some("Preferences"), Some("app.preferences"));
    let sec2 = gtk::gio::Menu::new();
    sec2.append(Some("Keyboard Shortcuts"), Some("app.shortcuts"));
    sec2.append(Some("About Dualcut"), Some("app.about"));
    menu.append_section(None, &sec2);
    let burger = gtk::MenuButton::new();
    burger.set_icon_name("open-menu-symbolic");
    burger.update_property(&[gtk::accessible::Property::Label("Main menu")]);
    burger.set_menu_model(Some(&menu));
    bar.pack_end(&burger);
    let right_toggle = gtk::ToggleButton::new();
    right_toggle.set_icon_name("sidebar-show-right-symbolic");
    right_toggle.update_property(&[gtk::accessible::Property::Label("Toggle inspector panel")]);
    right_toggle.set_tooltip_text(Some("Toggle inspector panel"));
    right_toggle.set_active(true);
    bar.pack_end(&right_toggle);
    bar.pack_end(&export);

    let transport = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    transport.set_margin_start(12);
    transport.set_margin_end(12);
    transport.append(&play);
    transport.append(&seek);
    transport.append(&time_label);

    // Bottom pane: the multitrack timeline, toggleable (GNOME Builder style).
    // Lane labels live in their own non-scrolling column beside the
    // clips (#56) rather than inline in the same horizontally-scrolling
    // strip, so "Layer n" / track names stay visible while scrolling.
    let label_col = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let strip = gtk::Box::new(gtk::Orientation::Vertical, 4);
    strip.set_margin_end(12);
    let content_scroll = gtk::ScrolledWindow::new();
    content_scroll.set_child(Some(&strip));
    content_scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Never);
    content_scroll.set_hexpand(true);
    let strip_hbox = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    strip_hbox.set_margin_start(12);
    strip_hbox.set_margin_bottom(8);
    strip_hbox.append(&label_col);
    strip_hbox.append(&content_scroll);
    let strip_scroll = gtk::ScrolledWindow::new();
    strip_scroll.set_child(Some(&strip_hbox));
    strip_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    strip_scroll.set_min_content_height(150);
    // Bottom pane: zoom toolbar + timeline, resizable; the toggle hides it.
    let bottom_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    {
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        toolbar.set_margin_start(12);
        toolbar.set_margin_end(12);
        toolbar.set_margin_top(4);
        let zl = gtk::Label::new(Some("Zoom"));
        zl.add_css_class("dim-label");
        toolbar.append(&zl);
        let zoom = gtk::Scale::with_range(gtk::Orientation::Horizontal, 8.0, 240.0, 2.0);
        zoom.set_value(DEFAULT_PPS);
        zoom.set_size_request(180, -1);
        zoom.update_property(&[gtk::accessible::Property::Label("Timeline zoom")]);
        {
            let editor = editor.clone();
            zoom.connect_change_value(move |_, _, value| {
                editor.state.borrow_mut().pps = value.clamp(8.0, 240.0);
                editor.rebuild_strip();
                glib::Propagation::Proceed
            });
        }
        toolbar.append(&zoom);
        let fit_btn = gtk::Button::from_icon_name("zoom-fit-best-symbolic");
        fit_btn.add_css_class("flat");
        fit_btn.set_tooltip_text(Some("Zoom to fit the whole timeline (#56)"));
        fit_btn.update_property(&[gtk::accessible::Property::Label("Zoom to fit")]);
        {
            let editor = editor.clone();
            let zoom = zoom.clone();
            fit_btn.connect_clicked(move |_| {
                editor.zoom_to_fit();
                zoom.set_value(editor.state.borrow().pps);
            });
        }
        toolbar.append(&fit_btn);
        let split_btn = gtk::Button::new();
        let split_content = adw::ButtonContent::builder()
            .icon_name("edit-cut-symbolic")
            .label("Split")
            .build();
        split_btn.set_child(Some(&split_content));
        split_btn.add_css_class("flat");
        split_btn.set_tooltip_text(Some("Split selected clip at playhead (S)"));
        split_btn.update_property(&[gtk::accessible::Property::Label("Split clip at playhead")]);
        {
            let editor = editor.clone();
            split_btn.connect_clicked(move |_| editor.split_selected());
        }
        toolbar.append(&split_btn);
        bottom_box.append(&toolbar);
    }
    bottom_box.append(&strip_scroll);
    {
        let bottom_box = bottom_box.clone();
        timeline_toggle.connect_toggled(move |b| bottom_box.set_visible(b.is_active()));
    }

    // Center: preview + transport.
    let center = gtk::Box::new(gtk::Orientation::Vertical, 4);
    center.append(&preview_overlay);
    center.append(&transport);

    // Left: Media | Code tabs.
    let media_grid = gtk::FlowBox::new();
    media_grid.set_selection_mode(gtk::SelectionMode::None);
    media_grid.set_min_children_per_line(2);
    media_grid.set_max_children_per_line(3);
    media_grid.set_homogeneous(true);
    let media_empty = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let empty_status = adw::StatusPage::builder()
        .icon_name("video-x-generic-symbolic")
        .title("No Media Imported")
        .description("Import files to build your library — or drop them anywhere on the window")
        .build();
    empty_status.set_vexpand(true);
    let empty_import = gtk::Button::with_label("Import…");
    empty_import.add_css_class("pill");
    empty_import.add_css_class("suggested-action");
    empty_import.set_halign(gtk::Align::Center);
    {
        let editor = editor.clone();
        empty_import.connect_clicked(move |btn| {
            let window = btn.root().and_downcast::<gtk::Window>();
            editor.import_media(window.as_ref());
        });
    }
    empty_status.set_child(Some(&empty_import));
    media_empty.append(&empty_status);
    let media_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    media_page.append(&media_empty);
    media_page.append(&media_grid);
    let media_scroll = gtk::ScrolledWindow::new();
    media_scroll.set_child(Some(&media_page));
    media_scroll.set_vexpand(true);

    let code_buffer = gtk::TextBuffer::new(None);
    let code_view = gtk::TextView::with_buffer(&code_buffer);
    code_view.set_monospace(true);
    let code_scroll = gtk::ScrolledWindow::new();
    code_scroll.set_child(Some(&code_view));
    code_scroll.set_vexpand(true);
    let code_apply = gtk::Button::with_label("Apply JSON");
    code_apply.add_css_class("suggested-action");
    let code_status = gtk::Label::new(None);
    code_status.set_halign(gtk::Align::Start);
    code_status.set_wrap(true);
    {
        let editor = editor.clone();
        let code_buffer = code_buffer.clone();
        let code_status = code_status.clone();
        code_apply.connect_clicked(move |_| {
            let text = code_buffer
                .text(&code_buffer.start_iter(), &code_buffer.end_iter(), false)
                .to_string();
            match Project::from_json(&text) {
                Ok(project) => {
                    code_status.set_text("✓ applied");
                    editor.commit_document(project);
                }
                Err(e) => code_status.set_text(&format!("✗ {e:#}")),
            }
        });
    }
    let code_page = gtk::Box::new(gtk::Orientation::Vertical, 6);
    code_page.append(&code_scroll);
    code_page.append(&code_apply);
    code_page.append(&code_status);

    let templates_list = gtk::ListBox::new();
    templates_list.add_css_class("boxed-list");
    templates_list.set_selection_mode(gtk::SelectionMode::None);
    let templates_scroll = gtk::ScrolledWindow::new();
    templates_scroll.set_child(Some(&templates_list));
    templates_scroll.set_vexpand(true);

    let clips_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    clips_box.set_margin_top(6);
    clips_box.set_margin_start(6);
    clips_box.set_margin_end(6);
    let left_stack = adw::ViewStack::new();
    left_stack.add_titled(&media_scroll, Some("library"), "Library");
    left_stack.add_titled(&clips_box, Some("clips"), "Clips");
    left_stack.add_titled(&templates_scroll, Some("templates"), "Templates");
    left_stack.add_titled(&code_page, Some("code"), "Code");
    #[cfg(feature = "scripting")]
    {
        // Progressive disclosure (#22): Script is power-user surface,
        // hidden unless enabled in Preferences.
        let script_page = build_script_panel(&editor);
        let script_stack_page = left_stack.add_titled(&script_page, Some("script"), "Script");
        script_stack_page.set_visible(prefs_show_script());
        let a = gtk::gio::SimpleAction::new("preferences", None);
        {
            let editor = editor.clone();
            let script_stack_page = script_stack_page.clone();
            a.connect_activate(move |_, _| {
                let dialog = adw::PreferencesDialog::new();
                let page = adw::PreferencesPage::new();
                let group = adw::PreferencesGroup::builder().title("Interface").build();
                let row = adw::SwitchRow::builder()
                    .title("Show Script tab")
                    .subtitle("TypeScript transforms of the project — for power users and agents")
                    .build();
                row.set_active(prefs_show_script());
                {
                    let script_stack_page = script_stack_page.clone();
                    row.connect_active_notify(move |r| {
                        script_stack_page.set_visible(r.is_active());
                        prefs_set_show_script(r.is_active());
                    });
                }
                group.add(&row);
                let quality = adw::ComboRow::builder()
                    .title("Preview quality")
                    .subtitle("Lower is faster; exports always render at full quality")
                    .build();
                let opts = gtk::StringList::new(&["Full", "Half", "Quarter"]);
                quality.set_model(Some(&opts));
                quality.set_selected(match preview_scale() {
                    s if s >= 0.99 => 0,
                    s if s >= 0.49 => 1,
                    _ => 2,
                });
                {
                    let editor = editor.clone();
                    quality.connect_selected_notify(move |q| {
                        let scale = match q.selected() {
                            0 => 1.0,
                            2 => 0.25,
                            _ => 0.5,
                        };
                        prefs_set_preview_scale(scale);
                        // Recompile the preview pipeline at the new scale.
                        let project = editor.state.borrow().project.clone();
                        if let Some(project) = project {
                            editor.rebuild_in_memory(project);
                        }
                    });
                }
                group.add(&quality);
                let proxies = adw::SwitchRow::builder()
                    .title("Use proxy media")
                    .subtitle(
                        "Preview with lightweight 960p transcodes for smooth \
                         scrubbing; exports always use the originals",
                    )
                    .build();
                proxies.set_active(prefs_use_proxies());
                {
                    let editor = editor.clone();
                    proxies.connect_active_notify(move |r| {
                        prefs_set_use_proxies(r.is_active());
                        // Recompile the preview pipeline with/without proxies.
                        let project = editor.state.borrow().project.clone();
                        if let Some(project) = project {
                            editor.rebuild_in_memory(project);
                        }
                    });
                }
                group.add(&proxies);

                // #61: family used for new text objects and built-in
                // templates (starter defs are copied fresh on first
                // insert, so this only affects future insertions -- it
                // never rewrites text already on the timeline).
                let font_row = adw::ActionRow::builder()
                    .title("Default font")
                    .subtitle("Used for new text objects and built-in templates")
                    .build();
                let font_dialog = gtk::FontDialog::new();
                let font_button = gtk::FontDialogButton::builder()
                    .dialog(&font_dialog)
                    .level(gtk::FontLevel::Family)
                    .valign(gtk::Align::Center)
                    .build();
                font_button
                    .set_font_desc(&gtk::pango::FontDescription::from_string(
                        &prefs_default_font_family(),
                    ));
                font_button.connect_font_desc_notify(move |b| {
                    if let Some(desc) = b.font_desc()
                        && let Some(family) = desc.family()
                    {
                        prefs_set_default_font_family(family.as_str());
                    }
                });
                font_row.add_suffix(&font_button);
                font_row.set_activatable_widget(Some(&font_button));
                group.add(&font_row);
                page.add(&group);

                // Keyboard shortcuts: a preset picker (muscle memory from
                // other editors) plus per-action rebinding.
                let kb_group = adw::PreferencesGroup::builder()
                    .title("Keyboard Shortcuts")
                    .description("Pick a preset to match another editor, or click any shortcut to set your own")
                    .build();
                let preset_names: Vec<&str> = keys::PRESETS.iter().map(|p| p.name).collect();
                let preset_row = adw::ComboRow::builder().title("Preset").build();
                preset_row.set_model(Some(&gtk::StringList::new(&preset_names)));
                // Action rows are rebuilt after a preset switch to reflect
                // the newly-applied bindings; declared here so the preset
                // handler and the initial build below can both reach it.
                let action_rows: Rc<RefCell<Vec<(keys::Action, adw::ActionRow, gtk::Button)>>> =
                    Rc::new(RefCell::new(Vec::new()));
                {
                    let action_rows = action_rows.clone();
                    preset_row.connect_selected_notify(move |row| {
                        let Some(preset) = keys::PRESETS.get(row.selected() as usize) else { return };
                        keys::apply_preset(preset);
                        for (action, arow, btn) in action_rows.borrow().iter() {
                            let combo = keys::binding(*action);
                            arow.set_subtitle(&combo.display());
                            btn.set_label(&combo.display());
                        }
                    });
                }
                kb_group.add(&preset_row);

                for action in keys::Action::ALL {
                    let row = adw::ActionRow::builder().title(action.label()).build();
                    let combo = keys::binding(action);
                    row.set_subtitle(&combo.display());
                    let btn = gtk::Button::with_label(&combo.display());
                    btn.set_valign(gtk::Align::Center);
                    btn.add_css_class("flat");
                    {
                        let row = row.clone();
                        let btn2 = btn.clone();
                        btn.connect_clicked(move |btn| {
                            btn.set_label("Press a key…");
                            // One-shot capture: a temporary key controller
                            // on the button itself, torn down after the
                            // first keypress (Escape cancels without
                            // changing the binding).
                            let capture = gtk::EventControllerKey::new();
                            let row = row.clone();
                            let btn2 = btn2.clone();
                            let btn_for_remove = btn.clone();
                            capture.connect_key_pressed(move |controller, key, _, modifier| {
                                if key != gtk::gdk::Key::Escape {
                                    let ctrl = modifier.contains(gtk::gdk::ModifierType::CONTROL_MASK);
                                    let shift = modifier.contains(gtk::gdk::ModifierType::SHIFT_MASK);
                                    if let Some(combo) = keys::Combo::parse(&format!(
                                        "{}{}{}",
                                        if ctrl { "ctrl+" } else { "" },
                                        if shift { "shift+" } else { "" },
                                        key.name().as_deref().unwrap_or("")
                                    )) {
                                        keys::set_binding(action, combo);
                                        row.set_subtitle(&combo.display());
                                        btn2.set_label(&combo.display());
                                    }
                                } else {
                                    btn2.set_label(&keys::binding(action).display());
                                }
                                btn_for_remove.remove_controller(controller);
                                glib::Propagation::Stop
                            });
                            btn.add_controller(capture);
                        });
                    }
                    row.add_suffix(&btn);
                    row.set_activatable_widget(Some(&btn));
                    kb_group.add(&row);
                    action_rows.borrow_mut().push((action, row, btn));
                }
                page.add(&kb_group);

                dialog.add(&page);
                dialog.present(editor.window().as_ref());
            });
        }
        app.add_action(&a);
    }
    let left_switcher = adw::InlineViewSwitcher::builder().stack(&left_stack).build();
    left_switcher.set_margin_top(6);
    left_switcher.set_margin_start(6);
    left_switcher.set_margin_end(6);
    let left_tabs = gtk::Box::new(gtk::Orientation::Vertical, 6);
    left_tabs.append(&left_switcher);
    left_stack.set_vexpand(true);
    left_tabs.append(&left_stack);
    left_tabs.set_size_request(260, -1);

    // Right: parameters (Inspect | Script), as before.
    let inspector = gtk::Box::new(gtk::Orientation::Vertical, 6);
    inspector.set_margin_top(8);
    inspector.set_margin_start(8);
    inspector.set_margin_end(8);
    inspector.set_size_request(300, -1);

    // Right sidebar: Inspect | History (scripting lives with the other
    // code views in the left notebook). The existing sidebar-toggle button
    // hides/shows this whole area, tabs included.
    let history_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    history_box.set_margin_top(8);
    history_box.set_margin_start(8);
    history_box.set_margin_end(8);

    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let inspector_scroll = gtk::ScrolledWindow::new();
    inspector_scroll.set_child(Some(&inspector));
    inspector_scroll.set_vexpand(true);
    inspector_scroll.set_min_content_width(280);
    inspector_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    let history_scroll = gtk::ScrolledWindow::new();
    history_scroll.set_child(Some(&history_box));
    history_scroll.set_vexpand(true);
    history_scroll.set_min_content_width(280);
    history_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);

    let right_stack = adw::ViewStack::new();
    right_stack.add_titled(&inspector_scroll, Some("inspect"), "Inspect");
    right_stack.add_titled(&history_scroll, Some("history"), "History");
    let right_switcher = adw::InlineViewSwitcher::builder().stack(&right_stack).build();
    right_switcher.set_margin_top(6);
    right_switcher.set_margin_bottom(6);
    sidebar.append(&right_switcher);
    right_stack.set_vexpand(true);
    sidebar.append(&right_stack);

    let inner = gtk::Paned::new(gtk::Orientation::Horizontal);
    inner.set_start_child(Some(&center));
    inner.set_end_child(Some(&sidebar));
    inner.set_resize_start_child(true);
    // Allow the sidebar to be shrunk rather than forcing the window wider
    // whenever a wide effect-param row's minimum size grows (#44); effect
    // rows themselves wrap via FlowBox below, so this is now a rare
    // fallback rather than the everyday case.
    inner.set_shrink_end_child(true);
    inner.set_position(620);
    // #62: shrink_end_child(true) above (kept for #44 -- it stops a wide
    // effect-param row from forcing the whole window wider) also let the
    // user drag the handle all the way to 0, losing the sidebar entirely.
    // Clamp the *manual* drag to a floor without reintroducing #44: this
    // only rejects positions past the limit, it never demands extra width.
    // Floor bumped to 400px per maintainer request (see #62 follow-up).
    {
        const MIN_SIDEBAR_WIDTH: i32 = 400;
        // Re-entrancy guard: set_position() below re-triggers this same
        // notify::position handler synchronously (observed hanging the
        // #44 regression-check walkthrough step, which stacks wide effect
        // rows onto this exact pane -- that resize storm kept re-entering
        // the handler faster than the clamp could settle).
        let clamping = Rc::new(std::cell::Cell::new(false));
        inner.connect_notify_local(Some("position"), move |paned, _| {
            if clamping.get() {
                return;
            }
            let width = paned.width();
            if width <= 0 {
                return;
            }
            let max_pos = (width - MIN_SIDEBAR_WIDTH).max(0);
            if paned.position() > max_pos {
                clamping.set(true);
                paned.set_position(max_pos);
                clamping.set(false);
            }
        });
    }

    let outer = gtk::Paned::new(gtk::Orientation::Horizontal);
    outer.set_start_child(Some(&left_tabs));
    outer.set_end_child(Some(&inner));
    outer.set_resize_end_child(true);
    outer.set_shrink_start_child(false);
    outer.set_position(260);
    outer.set_vexpand(true);

    {
        let left_tabs = left_tabs.clone();
        left_toggle.connect_toggled(move |b| left_tabs.set_visible(b.is_active()));
    }
    {
        let sidebar = sidebar.clone();
        right_toggle.connect_toggled(move |b| sidebar.set_visible(b.is_active()));
    }

    let vpaned = gtk::Paned::new(gtk::Orientation::Vertical);
    vpaned.set_start_child(Some(&outer));
    vpaned.set_end_child(Some(&bottom_box));
    vpaned.set_resize_start_child(true);
    vpaned.set_shrink_start_child(false);
    vpaned.set_shrink_end_child(false);
    vpaned.set_position(520);
    let skill_banner = adw::Banner::new(
        "A newer dualcut agent skill is bundled with this app version",
    );
    skill_banner.set_button_label(Some("Update"));
    if let Some(target_root) = skill_update_available() {
        skill_banner.set_revealed(true);
        skill_banner.connect_button_clicked(move |b| match install_skill_to(&target_root) {
            Ok(dest) => {
                println!("skill updated at {}", dest.display());
                b.set_revealed(false);
            }
            Err(e) => eprintln!("skill update failed: {e:#}"),
        });
    }

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&bar);
    content.append(&skill_banner);
    content.append(&vpaned);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&content));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(&window_title)
        .default_width(1120)
        .default_height(700)
        .content(&toasts)
        .build();
    // Headless walkthrough runs without a window manager, so maximize()
    // would be ignored; fill the virtual screen explicitly so the guide
    // screenshots have no dead space.
    if std::env::var("DUALCUT_WALKTHROUGH").is_ok() {
        window.set_default_size(1280, 800);
    }

    {
        let controller = gtk::EventControllerKey::new();
        let editor = editor.clone();
        let play_btn = play.clone();
        controller.connect_key_pressed(move |_, key, _, modifier| {
            // Customizable: dispatches through the live
            // key.<action> prefs bindings (dualcut defaults or a chosen
            // preset -- see Preferences), not a fixed match on the key.
            let Some(action) = keys::action_for(key, modifier) else {
                return glib::Propagation::Proceed;
            };
            let pipeline = editor.state.borrow().pipeline.clone();
            match action {
                keys::Action::PlayPause => {
                    let playing = pipeline.current_state() == gst::State::Playing;
                    let next = if playing { gst::State::Paused } else { gst::State::Playing };
                    // Same fix as the play button (#57): only flip the
                    // icon if the transition actually succeeded.
                    match pipeline.set_state(next) {
                        Ok(_) => {
                            play_btn.set_icon_name(if playing {
                                "media-playback-start-symbolic"
                            } else {
                                "media-playback-pause-symbolic"
                            });
                        }
                        Err(e) => {
                            eprintln!("playback: set_state({next:?}) failed: {e}");
                            editor.toast("Playback failed to start -- check the terminal for details");
                        }
                    }
                }
                keys::Action::StepBack | keys::Action::StepForward => {
                    let fps =
                        editor.state.borrow().project.as_ref().map_or(30.0, |p| p.meta.fps as f64);
                    let pos = pipeline
                        .query_position::<gst::ClockTime>()
                        .map(|p| p.nseconds() as f64 / 1e9)
                        .unwrap_or(0.0);
                    let step = if action == keys::Action::StepBack { -1.0 } else { 1.0 } / fps;
                    if let Err(e) = pipeline.set_state(gst::State::Paused) {
                        eprintln!("playback: step pause failed: {e}");
                    }
                    seek_to(&pipeline, (pos + step).max(0.0));
                }
                keys::Action::GoStart => seek_to(&pipeline, 0.0),
                keys::Action::GoEnd => {
                    let end = editor.state.borrow().project.as_ref().map_or(0.0, |p| p.duration());
                    seek_to(&pipeline, (end - 0.05).max(0.0));
                }
                keys::Action::Split => editor.split_selected(),
                keys::Action::RippleDelete => editor.ripple_delete_selected(),
                keys::Action::Undo => editor.undo(),
                keys::Action::Redo => editor.redo(),
            }
            glib::Propagation::Stop
        });
        window.add_controller(controller);
    }

    let css = gtk::CssProvider::new();
    css.load_from_data(
        "button.clip-text { background: alpha(#e5a50a, .25); }
         button.clip-video { background: alpha(#3584e4, .25); }
         button.clip-audio { background: alpha(#33d17a, .25); }
         button.clip-image { background: alpha(#9141ac, .25); }
         button.clip-shape { background: alpha(#e66100, .25); }
         button.clip-compref { background: alpha(#986a44, .25); }
         button.clip-test { background: alpha(#77767b, .3); }",
    );
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().unwrap(),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // App actions for the hamburger menu (#13, #14).
    {
        let make = |name: &str| gtk::gio::SimpleAction::new(name, None);
        let a = make("new-project");
        {
            let editor = editor.clone();
            a.connect_activate(move |_, _| {
                let project = dualcut_engine::templates::new_project("New Project");
                {
                    let mut st = editor.state.borrow_mut();
                    st.project_path = None;
                    st.history.clear();
                    st.redo.clear();
                    st.selected = None;
                }
                if let Some(win) = editor.window() {
                    win.set_title(Some("Dualcut — New Project (unsaved)"));
                }
                editor.rebuild_in_memory(project);
                editor.zoom_to_fit();
            });
        }
        app.add_action(&a);
        // Portrait canvas for short-form/social export (#48); pairs with
        // the vertical-center-crop / vertical-top-bottom-split starter
        // defs, which assume a 16:9 source and this exact 1080x1920 frame.
        let a = make("new-vertical-project");
        {
            let editor = editor.clone();
            a.connect_activate(move |_, _| {
                let project =
                    dualcut_engine::templates::new_project_sized("New Vertical Project", 1080, 1920);
                {
                    let mut st = editor.state.borrow_mut();
                    st.project_path = None;
                    st.history.clear();
                    st.redo.clear();
                    st.selected = None;
                }
                if let Some(win) = editor.window() {
                    win.set_title(Some("Dualcut — New Vertical Project (unsaved)"));
                }
                editor.rebuild_in_memory(project);
                editor.zoom_to_fit();
            });
        }
        app.add_action(&a);
        let a = make("save-as");
        {
            let editor = editor.clone();
            a.connect_activate(move |_, _| {
                let win = editor.window();
                editor.save_project_as(win.as_ref());
            });
        }
        app.add_action(&a);
        // Auto-captions (#37): present but greyed out without a local
        // whisper.cpp binary — the menu documents the feature either way.
        let a = make("captions");
        a.set_enabled(find_whisper().is_some());
        {
            let editor = editor.clone();
            a.connect_activate(move |_, _| show_captions_dialog(&editor));
        }
        app.add_action(&a);
        let a = make("install-skills");
        {
            let editor = editor.clone();
            a.connect_activate(move |_, _| {
                let win = editor.window();
                show_skills_dialog(&editor, win.as_ref());
            });
        }
        app.add_action(&a);
        let a = make("about");
        {
            let editor = editor.clone();
            a.connect_activate(move |_, _| {
                show_about(editor.window().as_ref());
            });
        }
        app.add_action(&a);
        let a = make("shortcuts");
        {
            let editor = editor.clone();
            a.connect_activate(move |_, _| {
                // Reflects the live bindings, not fixed text --
                // whatever preset or custom keys are active in
                // Preferences show up here too.
                let dialog = adw::ShortcutsDialog::new();
                let playback = adw::ShortcutsSection::new(Some("Playback"));
                for action in [keys::Action::PlayPause, keys::Action::StepBack, keys::Action::StepForward, keys::Action::GoStart, keys::Action::GoEnd] {
                    playback.add(adw::ShortcutsItem::new(action.label(), &keys::binding(action).gtk_accel()));
                }
                dialog.add(playback);
                let editing = adw::ShortcutsSection::new(Some("Editing"));
                for action in [keys::Action::Undo, keys::Action::Redo, keys::Action::Split, keys::Action::RippleDelete] {
                    editing.add(adw::ShortcutsItem::new(action.label(), &keys::binding(action).gtk_accel()));
                }
                dialog.add(editing);
                let mouse = adw::ShortcutsSection::new(Some("Mouse"));
                for (title, accel) in [
                    ("Move clip (vertical: change lane)", ""),
                    ("Trim: drag clip's right edge", ""),
                    ("Import: drop files on the window", ""),
                ] {
                    mouse.add(adw::ShortcutsItem::new(title, accel));
                }
                dialog.add(mouse);
                dialog.present(editor.window().as_ref());
            });
        }
        app.add_action(&a);

    }

    // UI smoke-test hook: DUALCUT_TEST_ACTION=<name> activates an app
    // action shortly after startup (headless CI has no reliable pointer).
    if let Ok(name) = std::env::var("DUALCUT_TEST_ACTION") {
        let app = app.clone();
        glib::timeout_add_seconds_local_once(2, move || {
            gtk::prelude::ActionGroupExt::activate_action(&app, &name, None);
        });
    }

    // Walkthrough mode: step through UI states, printing a shot marker
    // after each settles; scripts/walkthrough.sh screenshots per marker.
    // Drives the guide images regenerated on every release.
    if std::env::var("DUALCUT_WALKTHROUGH").is_ok() {
        let untitled = editor.state.borrow().project_path.is_none();
        type Step = (&'static str, Box<dyn Fn()>);
        let mut steps: Vec<Step> = Vec::new();
        if untitled {
            steps.push(("new-project", Box::new(|| {})));
            {
                let editor = editor.clone();
                steps.push(("shorts-mode", Box::new(move || {
                    // Portrait canvas (#48). Captured in this pass (no
                    // project file, so nothing in this document ever
                    // references the demo video asset) rather than the
                    // demo-project pass: spawn_thumbnail_worker() scans
                    // every clip for thumbnails on the very first
                    // rebuild_strip(), and the demo's own media-ball clip
                    // can crash that background worker on hosts missing
                    // its codec profile (#45) before any step in that
                    // pass gets a reliable chance to capture.
                    let mut project =
                        dualcut_engine::templates::new_project_sized("Shorts Demo", 1080, 1920);
                    project.scenes[0].duration = 6.0;
                    project.scenes[0].layers.push(document::Clip {
                        id: "hook".into(),
                        start: 0.0,
                        duration: 0.0,
                        element: document::Element::Text {
                            text: "Wait for it…".into(),
                            font: "Sans Bold 44".into(),
                            color: "#ffffff".into(),
                            align: Some(document::TextAlign::Center),
                            outline: Some("#000000".into()),
                            shadow: false,
                        },
                        transform: Default::default(),
                        animations: vec![],
                        effects: vec![],
                    });
                    {
                        let mut st = editor.state.borrow_mut();
                        st.project_path = None;
                        st.history.clear();
                        st.redo.clear();
                        st.selected = None;
                    }
                    if let Some(win) = editor.window() {
                        win.set_title(Some("Dualcut — Shorts Demo (unsaved)"));
                    }
                    editor.rebuild_in_memory(project);
                    editor.zoom_to_fit();
                })));
            }
        } else {
            steps.push(("editor-overview", Box::new(|| {})));
            {
                let editor = editor.clone();
                let tabs2 = left_stack.clone();
                steps.push(("library", Box::new(move || {
                    tabs2.set_visible_child_name("library");
                    let project = editor.state.borrow().project.clone();
                    if let Some(mut project) = project {
                        project.library =
                            vec!["assets/ball.mp4".into(), "assets/ticks.ogg".into()];
                        editor.commit_document(project);
                    }
                })));
            }
            {
                let tabs = left_stack.clone();
                steps.push(("templates", Box::new(move || tabs.set_visible_child_name("templates"))));
            }
            {
                let tabs = left_stack.clone();
                steps.push(("code-view", Box::new(move || tabs.set_visible_child_name("code"))));
            }
            {
                let editor = editor.clone();
                let tabs = left_stack.clone();
                steps.push(("clip-inspector", Box::new(move || {
                    tabs.set_visible_child_name("clips");
                    editor.state.borrow_mut().selected = Some("media-ball".into());
                    // Stack a couple of wide effects (Crop, Color) so this
                    // shot doubles as a regression check for #44 -- the
                    // right pane must wrap them, not grow to fit.
                    let mut project = editor.state.borrow().project.clone().unwrap();
                    if let Some(c) = find_clip_mut(&mut project, "media-ball") {
                        c.effects.push(document::Effect::Crop { left: 0, right: 0, top: 0, bottom: 0 });
                        c.effects.push(document::Effect::Color {
                            brightness: 0.0, contrast: 1.0, saturation: 1.0, hue: 0.0,
                        });
                    }
                    editor.commit_document(project);
                })));
            }
            {
                let editor = editor.clone();
                steps.push(("scene-form", Box::new(move || {
                    editor.state.borrow_mut().selected = Some("scene:scene-media".into());
                    editor.rebuild_inspector();
                })));
            }
            {
                let app = app.clone();
                steps.push(("about", Box::new(move || {
                    gtk::prelude::ActionGroupExt::activate_action(&app, "about", None);
                })));
            }
        }
        let steps = Rc::new(steps);
        let index = Rc::new(std::cell::Cell::new(0usize));
        let app2 = app.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(3000), move || {
            let i = index.get();
            if i >= steps.len() {
                // Give the driver time to grab the last frame, then exit.
                let app3 = app2.clone();
                glib::timeout_add_local_once(
                    std::time::Duration::from_millis(1500),
                    move || app3.quit(),
                );
                return glib::ControlFlow::Break;
            }
            let (name, run) = &steps[i];
            run();
            let name = *name;
            glib::timeout_add_local_once(std::time::Duration::from_millis(1800), move || {
                use std::io::Write;
                println!("WALKTHROUGH-SHOT {name}");
                let _ = std::io::stdout().flush();
            });
            index.set(i + 1);
            glib::ControlFlow::Continue
        });
    }

    // Drag-and-drop media import: dropping files anywhere on the window
    // adds them to the library.
    {
        let drop = gtk::DropTarget::new(
            gtk::gdk::FileList::static_type(),
            gtk::gdk::DragAction::COPY,
        );
        let editor = editor.clone();
        drop.connect_drop(move |_, value, _, _| {
            let Ok(list) = value.get::<gtk::gdk::FileList>() else { return false };
            let paths: Vec<PathBuf> =
                list.files().iter().filter_map(|f| f.path()).collect();
            if paths.is_empty() {
                return false;
            }
            editor.add_to_library(&paths);
            true
        });
        window.add_controller(drop);
    }

    loading.close();

    window.present();
    start_paused(&pipeline)?;

    *editor.ui.borrow_mut() =
        Some(Ui {
            picture,
            seek,
            strip,
            label_col,
            content_scroll,
            inspector,
            media_grid,
            media_empty,
            clips_box,
            history_box,
            toasts: toasts.clone(),
            ruler: std::cell::RefCell::new(None),
            templates_list,
            code_buffer,
            play_btn: play,
        });
    editor.drain_bus_errors(&pipeline);
    editor.rebuild_strip();
    editor.rebuild_inspector();
    editor.rebuild_media();
    editor.rebuild_templates();
    editor.refresh_code();
    // Deferred (#56): the strip hasn't been allocated a real width yet
    // at this point in startup (window.present() ran before Ui existed),
    // so zoom_to_fit() would see width 0 and no-op if called eagerly.
    {
        let editor = editor.clone();
        glib::idle_add_local_once(move || editor.zoom_to_fit());
    }
    Ok(())
}

