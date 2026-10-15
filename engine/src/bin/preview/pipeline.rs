//! GStreamer Editing Services (GES) pipeline creation, preroll, proxy swapping,
//! compilation, and seek utilities.
//!
//! Extracted from `preview.rs` (dualcut#78): `make_pipeline` attaches timelines
//! to gtk4paintablesink, `start_paused` and `seek_to` manage playback state,
//! `with_proxies` swaps in proxy media, and `compile_project*` compiles documents
//! into timelines.

use super::*;

pub(crate) fn make_pipeline(timeline: &ges::Timeline) -> Result<(ges::Pipeline, gtk::gdk::Paintable)> {
    let pipeline = ges::Pipeline::new();
    pipeline.set_timeline(timeline).context("attaching timeline")?;
    // Keep the sink bare, with no glsinkbin wrapper. A wrapper looks like a
    // zero-copy GL optimization, but it breaks preroll in a GTK process. GES
    // sends system-memory caps, and the glupload in glsinkbin does not
    // negotiate with the sink. The bin then errors at preroll with "Failed to
    // link sink element", and preview_set_video_sink rejects it. The pipeline
    // never reaches Paused.
    //
    // Symptoms: black preview, no position, a play button that toggles
    // without playback. A bisect on GStreamer 1.26.11 isolates the wrapper.
    // With GES caps, the wrapped sink fails every time; the bare sink
    // prerolls every time. The paintablesink does its own GL upload, so the
    // wrapper adds nothing.
    let sink = gst::ElementFactory::make("gtk4paintablesink")
        .build()
        .context("creating gtk4paintablesink")?;
    let paintable = sink.property::<gtk::gdk::Paintable>("paintable");
    pipeline.preview_set_video_sink(Some(&sink));
    Ok((pipeline, paintable))
}

pub(crate) fn start_paused(pipeline: &ges::Pipeline) -> Result<()> {
    if pipeline.set_state(gst::State::Paused).is_err() {
        if let Err(e) = pipeline.set_state(gst::State::Null) {
            eprintln!("start_paused: pipeline Null reset failed: {e}");
        }
        if let Ok(fake) = gst::ElementFactory::make("fakesink").build() {
            pipeline.preview_set_audio_sink(Some(&fake));
        }
        pipeline.set_state(gst::State::Paused).context("pausing pipeline")?;
    }
    Ok(())
}

/// Preview-only: swap video clip sources for their cached 960px proxies
/// where one exists. Exports go through render_project on the untouched
/// document, so originals are never affected.
pub(crate) fn with_proxies(project: &Project, base_dir: &std::path::Path, cache_dir: &std::path::Path) -> Project {
    let mut swapped = project.clone();
    let clips = swapped
        .scenes
        .iter_mut()
        .flat_map(|s| s.layers.iter_mut())
        .chain(swapped.overlays.iter_mut().flat_map(|t| t.clips.iter_mut()));
    for clip in clips {
        if let document::Element::Video { src, .. } = &mut clip.element
            && let Some(uri) = media_uri(src, base_dir)
            && uri.starts_with("file://")
        {
            let proxy = dualcut_engine::thumbs::proxy_path(cache_dir, &uri);
            if proxy.exists() {
                *src = proxy.display().to_string();
            }
        }
    }
    swapped
}

pub(crate) fn compile_project(project: &Project, base_dir: &std::path::Path) -> Result<ges::Timeline> {
    Ok(compile_project_with_warnings(project, base_dir)?.0)
}

/// As [`compile_project`], but also returns compile warnings so callers
/// with a live `editor` can surface them instead of leaving them only in
/// `eprintln!` output the user never sees (#57).
pub(crate) fn compile_project_with_warnings(
    project: &Project,
    base_dir: &std::path::Path,
) -> Result<(ges::Timeline, Vec<String>)> {
    compile_project_with_warnings_inner(project, base_dir, &base_dir.join(".dualcut-cache"))
}

/// As [`compile_project_with_warnings`] but with an explicit cache
/// directory (editor passes `cache_dir()` for Flatpak safety, #63).
pub(crate) fn compile_project_with_warnings_cache(
    project: &Project,
    base_dir: &std::path::Path,
    cache_dir: &std::path::Path,
) -> Result<(ges::Timeline, Vec<String>)> {
    compile_project_with_warnings_inner(project, base_dir, cache_dir)
}

pub(crate) fn compile_project_with_warnings_inner(
    project: &Project,
    base_dir: &std::path::Path,
    cache_dir: &std::path::Path,
) -> Result<(ges::Timeline, Vec<String>)> {
    // Preview pipelines render at reduced resolution (Preferences) and
    // read proxy media where available; exports go through render_project
    // at full quality from the original sources.
    let project = if prefs_use_proxies() {
        std::borrow::Cow::Owned(with_proxies(project, base_dir, cache_dir))
    } else {
        std::borrow::Cow::Borrowed(project)
    };
    let compiled = mapping::compile_scaled(&project, base_dir, preview_scale())?;
    for warning in &compiled.warnings {
        eprintln!("warning: {warning}");
    }
    Ok((compiled.timeline, compiled.warnings))
}

pub(crate) fn seek_to(pipeline: &ges::Pipeline, secs: f64) {
    let _ = pipeline.seek_simple(
        gst::SeekFlags::FLUSH,
        gst::ClockTime::from_useconds((secs.max(0.0) * 1e6) as u64),
    );
}

/// Coverage for the failure paths (#57) that used to be silently
/// discarded with `let _ = ...` at every live-playback call site: the
/// pipeline/compile plumbing itself is plain functions with no GTK
/// window needed, so these run under ordinary `cargo test`, not just
/// manual GUI verification.
#[cfg(test)]
mod tests {
    use super::*;

    /// Real `ges::Pipeline` + `gtk4paintablesink` construction isn't safe
    /// to run concurrently across threads without a live GLib main loop --
    /// two of these tests running in parallel (the `cargo test` default)
    /// deadlock instead of failing, which silently hung CI for 6+ hours
    /// until it was auto-cancelled (see the "Test coverage for the last
    /// untested engine modules" CI run). Serialize them instead.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn init_once() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            init().expect("gst/ges init");
            // Real registration path build_ui uses -- make_pipeline needs
            // "gtk4paintablesink" to exist at all (H4 in #57: what happens
            // if the sink can't be created is exactly what's untested
            // without this).
            gstgtk4::plugin_register_static().expect("registering gtk4paintablesink");
        });
    }

    fn empty_project(base_dir: &std::path::Path) -> (Project, PathBuf) {
        let project = dualcut_engine::templates::new_project("pipeline-test");
        (project, base_dir.to_path_buf())
    }

    /// H4: a normal, empty project compiles and its pipeline reaches
    /// Paused -- the baseline `start_paused` is supposed to hit on every
    /// healthy edit, so a regression here means *nothing* previews.
    #[test]
    fn make_pipeline_and_start_paused_succeed_for_a_normal_project() {
        let _guard = lock();
        init_once();
        let dir = std::env::temp_dir().join("dualcut-pipeline-test-normal");
        std::fs::create_dir_all(&dir).unwrap();
        let (project, base_dir) = empty_project(&dir);
        let timeline = compile_project(&project, &base_dir).expect("compiles");
        let (pipeline, _paintable) = make_pipeline(&timeline).expect("pipeline builds");
        start_paused(&pipeline).expect("a normal empty timeline should preroll cleanly");
        let _ = pipeline.set_state(gst::State::Null);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// H1/#37 test-table row 3: a project referencing media that doesn't
    /// exist on disk at all (not just an undecodable codec -- #45 covers
    /// that) must still compile with a non-empty warning, not error out
    /// and leave the caller with nothing to show the user.
    #[test]
    fn compile_project_with_warnings_reports_missing_media_instead_of_failing() {
        init_once();
        let dir = std::env::temp_dir().join("dualcut-pipeline-test-missing");
        std::fs::create_dir_all(&dir).unwrap();
        let mut project = dualcut_engine::templates::new_project("missing-media-test");
        project.scenes[0].layers.push(document::Clip {
            id: "gone".into(),
            start: 0.0,
            duration: 1.0,
            element: document::Element::Video {
                src: "does-not-exist.mp4".into(),
                offset: 0.0,
                volume: 1.0,
                rate: 1.0,
            },
            transform: Default::default(),
            animations: Vec::new(),
            effects: Vec::new(),
        });
        let result = compile_project_with_warnings(&project, &dir);
        let (_timeline, warnings) = result.expect("missing media should degrade, not error");
        assert!(!warnings.is_empty(), "expected a warning about the missing file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Test-table row 4: before a pipeline has ever prerolled,
    /// `query_position()` must return `None`, not a stale/zero value that
    /// could be mistaken for a real position -- this is the exact
    /// condition the stuck "0:00.0" timestamp and missing playhead in the
    /// bug report trace back to.
    #[test]
    fn query_position_is_none_before_preroll() {
        let _guard = lock();
        init_once();
        let dir = std::env::temp_dir().join("dualcut-pipeline-test-position");
        std::fs::create_dir_all(&dir).unwrap();
        let (project, base_dir) = empty_project(&dir);
        let timeline = compile_project(&project, &base_dir).expect("compiles");
        let (pipeline, _paintable) = make_pipeline(&timeline).expect("pipeline builds");
        assert!(
            pipeline.query_position::<gst::ClockTime>().is_none(),
            "a never-started pipeline should report no position"
        );
        let _ = pipeline.set_state(gst::State::Null);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Test-table row 6: `with_proxies`/proxy-swap compilation must not
    /// choke when the proxy cache is empty (the common case right after
    /// opening a project, before the background thumbnail worker has run)
    /// -- it should fall back to the original media, not fail the whole
    /// compile.
    #[test]
    fn compile_project_works_with_no_proxy_cache_present() {
        init_once();
        let dir = std::env::temp_dir().join("dualcut-pipeline-test-noproxy");
        std::fs::create_dir_all(&dir).unwrap();
        // No .dualcut-cache directory at all -- proxy_path() checks will
        // all miss, exercising the "proxies not built yet" fallback.
        let (project, base_dir) = empty_project(&dir);
        assert!(compile_project(&project, &base_dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Test-table row 1 (#57): transitioning a properly-prerolled
    /// pipeline from Paused to Playing and back must succeed and return
    /// the correct state-change result -- this is the path the play
    /// button, spacebar, and arrow-key handlers exercise hundreds of
    /// times per editing session.
    #[test]
    fn set_state_playing_from_paused_succeeds_for_normal_pipeline() {
        let _guard = lock();
        init_once();
        let dir = std::env::temp_dir().join("dualcut-pipeline-test-playing");
        std::fs::create_dir_all(&dir).unwrap();
        let (project, base_dir) = empty_project(&dir);
        let timeline = compile_project(&project, &base_dir).expect("compiles");
        let (pipeline, _paintable) = make_pipeline(&timeline).expect("pipeline builds");
        start_paused(&pipeline).expect("normal project preroolls");
        // Transition Paused → Playing must succeed.
        assert!(pipeline.set_state(gst::State::Playing).is_ok());
        // Transition Playing → Paused must also succeed.
        assert!(pipeline.set_state(gst::State::Paused).is_ok());
        let _ = pipeline.set_state(gst::State::Null);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Test-table row 2 (#57): `start_paused`, teardown via
    /// `set_state(Null)`, and a second `start_paused` must all succeed
    /// without error -- this is the cycle that `swap_pipeline` and
    /// `rebuild` perform on every document edit. A regression here means
    /// the preview silently dies after the first edit.
    #[test]
    fn start_paused_null_start_paused_cycle_succeeds() {
        let _guard = lock();
        init_once();
        let dir = std::env::temp_dir().join("dualcut-pipeline-test-cycle");
        std::fs::create_dir_all(&dir).unwrap();
        let (project, base_dir) = empty_project(&dir);
        let timeline = compile_project(&project, &base_dir).expect("compiles");
        let (pipeline, _paintable) = make_pipeline(&timeline).expect("pipeline builds");
        start_paused(&pipeline).expect("first preroll");
        let _ = pipeline.set_state(gst::State::Null);
        // Position must vanish after Null reset (the stuck-timestamp bug
        // in #57 depended on this invariant holding).
        assert!(pipeline.query_position::<gst::ClockTime>().is_none());
        start_paused(&pipeline).expect("second preroll after Null");
        let _ = pipeline.set_state(gst::State::Null);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
