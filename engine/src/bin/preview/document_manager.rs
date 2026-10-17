//! Document and project lifecycle management.
//!
//! This module encapsulates document persistence, history tracking, undo/redo,
//! and project-level operations (open, save, commit edits).

use super::*;

impl Editor {
    /// Persist the document, push a history entry, rebuild everything.
    /// Every GUI-originated edit goes through this (so it's tagged
    /// `EditSource::Gui`); external file edits and agent HTTP requests are
    /// tagged separately where they're detected (mtime-poll reload).
    pub(crate) fn commit_document(self: &Rc<Self>, project: Project) {
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
    pub(crate) fn jump_to_history(self: &Rc<Self>, index: usize) {
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

    /// Open a project file, replacing the current session state.
    pub(crate) fn open_project(self: &Rc<Self>, path: &std::path::Path) {
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

    /// Save As: pick a path, write, adopt it for future auto-saves.
    pub(crate) fn save_project_as(self: &Rc<Self>, window: Option<&gtk::Window>) {
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

    pub(crate) fn write_and_rebuild(self: &Rc<Self>, path: &std::path::Path, project: Project) {
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

    pub(crate) fn rebuild_in_memory(self: &Rc<Self>, project: Project) {
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

    pub(crate) fn undo(self: &Rc<Self>) {
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

    pub(crate) fn redo(self: &Rc<Self>) {
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
    pub(crate) fn rebuild(self: &Rc<Self>) {
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
}
