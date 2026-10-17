//! Media library management.
//!
//! This module encapsulates media file operations: importing, adding to library,
//! thumbnail generation, proxy transcoding, and template management.

use super::*;

impl Editor {
    /// Add file paths to the project library (#15); shared by the Import
    /// dialog and window drag-and-drop.
    pub(crate) fn add_to_library(self: &Rc<Self>, paths: &[PathBuf]) {
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
    pub(crate) fn import_media(self: &Rc<Self>, window: Option<&gtk::Window>) {
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

    /// Insert a media file as a clip in the scene under the playhead.
    pub(crate) fn insert_media(self: &Rc<Self>, rel: &str) {
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

    /// Generate any missing media thumbnails off-thread, then refresh the
    /// strip once so they appear.
    pub(crate) fn spawn_thumbnail_worker(self: &Rc<Self>, project: &Project, cache: &std::path::Path) {
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

    /// Insert a def instance into the scene under the playhead.
    pub(crate) fn insert_template(
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

    /// Scene form: duration and transition editing.
    pub(crate) fn build_scene_form(self: &Rc<Self>, uiref: &Ui, project: &Project, scene_id: &str) {
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
}
