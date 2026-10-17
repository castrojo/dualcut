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

/// UI state management and widget rebuilding.
/// Organizes all GTK widget construction, rebuilding, and state
/// management for the editor's UI components.
mod ui_manager;

/// Document and project lifecycle management.
/// Organizes document persistence, history tracking, undo/redo,
/// and project-level operations.
mod document_manager;

/// Media library management.
/// Organizes media file operations: importing, adding to library,
/// thumbnail generation, proxy transcoding, and templates.
mod library_manager;

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

