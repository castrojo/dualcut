//! Headless GES render.
//!
//! Usage:
//!   render <project.json> <output.mp4>     render a document (M1 path)
//!   render <output.mp4> [media-uri]        render the built-in M0 demo

use anyhow::{Context, Result};
use dualcut_engine::{build_demo_timeline, encoding_profile, init, run_to_eos};
use dualcut_engine::{document::Project, mapping};
use ges::prelude::*;
use gstreamer as gst;
use gstreamer_editing_services as ges;

fn main() -> Result<()> {
    init()?;

    let mut args = std::env::args().skip(1);
    let first = args.next().unwrap_or_else(|| "out.mp4".into());

    // `render vellotest` proves the live vector source end-to-end.
    #[cfg(feature = "vector")]
    if first == "vellotest" {
        use gst::prelude::*;
        dualcut_engine::vellosrc::register().context("registering vellosrc")?;

        // 1. Direct pipeline: spinning star, 10 frames to PNGs.
        let src = gst::Element::make_from_uri(
            gst::URIType::Src,
            "vello://star?fill=%23ffd700&w=220&h=220&spin=1",
            None,
        )
        .context("making vellosrc from uri")?;
        src.set_property("num-buffers", 10i32);
        let convert = gst::ElementFactory::make("videoconvert").build()?;
        let enc = gst::ElementFactory::make("pngenc").build()?;
        let sink = gst::ElementFactory::make("multifilesink")
            .property("location", "out/vs-%02d.png")
            .build()?;
        let pipeline = gst::Pipeline::new();
        pipeline.add_many([&src, &convert, &enc, &sink])?;
        gst::Element::link_many([&src, &convert, &enc, &sink])?;
        pipeline.set_state(gst::State::Playing)?;
        let bus = pipeline.bus().unwrap();
        for msg in bus.iter_timed(gst::ClockTime::from_seconds(30)) {
            use gst::MessageView;
            match msg.view() {
                MessageView::Eos(_) => break,
                MessageView::Error(e) => anyhow::bail!("pipeline error: {}", e.error()),
                _ => {}
            }
        }
        pipeline.set_state(gst::State::Null)?;
        println!("vellosrc via URI handler: 10 frames -> out/vs-*.png");

        // 2. GES UriClip with a vello:// URI.
        let timeline = ges::Timeline::new_audio_video();
        let layer = timeline.append_layer();
        match ges::UriClip::new("vello://circle?fill=%235dd39e&w=300&h=300") {
            Ok(clip) => {
                clip.set_start(gst::ClockTime::ZERO);
                clip.set_duration(gst::ClockTime::from_seconds(2));
                layer.add_clip(&clip)?;
                println!("GES UriClip(vello://) accepted");
            }
            Err(e) => println!("GES UriClip(vello://) rejected: {e} (bridge documented on #7)"),
        }
        return Ok(());
    }

    // `render new <project.json> [title]` scaffolds a starter project.
    if first == "new" {
        let path = args.next().unwrap_or_else(|| "project.json".into());
        let title = args.next().unwrap_or_else(|| "Untitled".into());
        let project = dualcut_engine::templates::new_project(&title);
        std::fs::write(&path, project.to_json()).with_context(|| format!("writing {path}"))?;
        println!("scaffolded {path} ({title:?}, {} starter templates)", project.defs.len());
        return Ok(());
    }

    // `render tpl <project.json> <def>` renders a template preview PNG.
    #[cfg(feature = "preview")]
    if first == "tpl" {
        let path = args.next().context("usage: render tpl <project.json> <def>")?;
        let name = args.next().context("usage: render tpl <project.json> <def>")?;
        let json = std::fs::read_to_string(&path)?;
        let project = Project::from_json(&json)?;
        let base = std::path::Path::new(&path).parent().unwrap_or(std::path::Path::new(".")).to_path_buf();
        let out = dualcut_engine::thumbs::template_png(&base.join(".dualcut-cache"), &project, &name, &base)?;
        println!("template preview -> {}", out.display());
        return Ok(());
    }

    // `render proxy <media>` builds the preview proxy for a media file
    // into .dualcut-cache next to it (debug probe for issue #30).
    #[cfg(feature = "preview")]
    if first == "proxy" {
        let media = args.next().context("usage: render proxy <media>")?;
        let abs = std::path::absolute(&media)?;
        let uri = dualcut_engine::media::uri_from_path(&abs);
        let base = abs.parent().unwrap_or(std::path::Path::new(".")).to_path_buf();
        let out = dualcut_engine::thumbs::proxy_mp4(&base.join(".dualcut-cache"), &uri)?;
        println!("proxy -> {}", out.display());
        return Ok(());
    }

    // Optional third argument overrides the encoding profile
    // (mp4|webm|h265|vp9|av1|prores); default derives from the extension.
    let mut profile_override: Option<String> = None;
    let (timeline, out) = if first.ends_with(".json") {
        let out = args.next().unwrap_or_else(|| "out.mp4".into());
        let json = std::fs::read_to_string(&first).with_context(|| format!("reading {first}"))?;
        let project = Project::from_json(&json)?;
        let base_dir = std::path::Path::new(&first)
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .to_path_buf();
        let compiled = mapping::compile(&project, &base_dir)?;
        for warning in &compiled.warnings {
            eprintln!("warning: {warning}");
        }
        println!(
            "project {:?}: {} scene(s), {} overlay track(s), {:.1}s",
            project.meta.title,
            project.scenes.len(),
            project.overlays.len(),
            project.duration()
        );
        profile_override = args.next();
        (compiled.timeline, out)
    } else {
        let media_uri = args.next();
        (build_demo_timeline(media_uri.as_deref())?, first)
    };

    let pipeline = ges::Pipeline::new();
    pipeline.set_timeline(&timeline).context("attaching timeline")?;

    let out_abs = std::path::absolute(&out)?;
    // Render to a sibling .part file and rename on success (#43): a
    // failed or interrupted export must never leave a broken/empty file
    // at the name the user asked for.
    let part = out_abs.with_extension(format!(
        "{}.part",
        out_abs.extension().and_then(|e| e.to_str()).unwrap_or("out")
    ));
    let _ = std::fs::remove_file(&part);
    let uri = dualcut_engine::media::uri_from_path(&part);
    pipeline
        .set_render_settings(&uri, &encoding_profile(profile_override.as_deref().unwrap_or(&out))?)
        .context("setting render settings")?;
    pipeline
        .set_mode(ges::PipelineFlags::RENDER)
        .context("setting render mode")?;

    println!("rendering -> {}", out_abs.display());
    let start = std::time::Instant::now();
    if let Err(e) = run_to_eos(&pipeline) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&part, &out_abs) {
        let _ = std::fs::remove_file(&part);
        anyhow::bail!("render finished but couldn't move into place: {e}");
    }
    println!("done in {:.1?}", start.elapsed());
    Ok(())
}
