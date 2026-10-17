//! UI state management and widget rebuilding.
//!
//! This module encapsulates all GTK widget construction, rebuilding, and state
//! management for the editor's UI components (timeline strip, inspector sidebar,
//! media library, history panel).

use super::*;

impl Editor {
    /// Icon + label + Lock/Hide/Mute toggle column for one timeline lane
    /// row (#21), fixed to [`LANE_COL_PX`] so every row (and the ruler's
    /// spacer) lines up on the same x-axis. `on_toggle` commits whatever
    /// document mutation the caller's lane identity needs.
    pub(crate) fn build_lane_column(
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

    pub(crate) fn rebuild_strip(self: &Rc<Self>) {
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


    pub(crate) fn rebuild_inspector(self: &Rc<Self>) {
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

    /// Library tab: media the user imported (project.library), shown as a
    /// thumbnail grid with a context menu (#15).
    pub(crate) fn rebuild_media(self: &Rc<Self>) {
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

    /// Edit History panel (newest first): who made each change (GUI /
    /// agent HTTP request / external file edit) and a coarse summary.
    /// Clicking an entry jumps the document straight to that state.
    pub(crate) fn rebuild_history(self: &Rc<Self>) {
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

    /// Templates tab: every def, instantiable at the playhead.
    pub(crate) fn rebuild_templates(self: &Rc<Self>) {
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

    /// Zoom so the whole project's current total duration fills the
    /// timeline's visible width -- "100%" per #56 (the strip shows every
    /// scene concatenated on one absolute-time axis, not a single scene
    /// in isolation, so "fit" is naturally project-wide here rather than
    /// per-scene). Falls back to the existing pps if the strip hasn't
    /// been allocated a width yet (e.g. called before the window is
    /// first shown).
    pub(crate) fn zoom_to_fit(self: &Rc<Self>) {
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

    /// Code tab: the document as editable JSON.
    pub(crate) fn refresh_code(self: &Rc<Self>) {
        let ui = self.ui.borrow();
        let Some(ui) = ui.as_ref() else { return };
        let st = self.state.borrow();
        if let Some(project) = st.project.as_ref() {
            ui.code_buffer.set_text(&project.to_json());
        }
    }
}
