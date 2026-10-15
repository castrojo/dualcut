//! M0 scripting spike: user TypeScript running in-process (deno_core via
//! rustyscript), driving a GES timeline through a minimal `editor.*` API —
//! the seed of the M4 scripting surface.
//!
//! Usage: script [file.ts]   (defaults to a built-in demo script)

use anyhow::{Context, Result};
use dualcut_engine::{init, mp4_profile, run_to_eos};
use ges::prelude::*;
use gstreamer as gst;
use gstreamer_editing_services as ges;
use rustyscript::{Module, Runtime, RuntimeOptions};
use std::sync::Mutex;

const DEMO_SCRIPT: &str = r#"
// TypeScript, executed in-process by the engine.
interface Clip { kind: "test" | "title"; start: number; duration: number; text?: string }

const clips: Clip[] = [
    { kind: "test", start: 0, duration: 4 },
    { kind: "title", start: 0.5, duration: 3, text: "Hello from TypeScript" },
];

for (const c of clips) {
    editor.addClip(c);
}
export const total: number = clips.length;
"#;

// GES objects are not Send and V8 callbacks may run off the main thread,
// so the editor ops only collect plain clip specs; the timeline is built
// on the main thread after the script completes.
static CLIPS: Mutex<Vec<serde_json::Value>> = Mutex::new(Vec::new());

fn add_clip(args: &[serde_json::Value]) -> Result<serde_json::Value, rustyscript::Error> {
    if let Some(arg) = args.first() {
        CLIPS.lock().unwrap().push(arg.clone());
    }
    Ok(serde_json::Value::Null)
}

fn build_timeline(specs: &[serde_json::Value]) -> Result<ges::Timeline> {
    let timeline = ges::Timeline::new_audio_video();
    let title_layer = timeline.append_layer();
    let base_layer = timeline.append_layer();
    for arg in specs {
        let start =
            gst::ClockTime::from_mseconds((arg["start"].as_f64().unwrap_or(0.0) * 1000.0) as u64);
        let duration =
            gst::ClockTime::from_mseconds((arg["duration"].as_f64().unwrap_or(1.0) * 1000.0) as u64);
        match arg["kind"].as_str().unwrap_or("test") {
            "title" => {
                let clip = ges::TitleClip::new().context("title clip")?;
                clip.set_start(start);
                clip.set_duration(duration);
                title_layer.add_clip(&clip).context("add title")?;
                let text = arg["text"].as_str().unwrap_or("").to_value();
                clip.set_child_property("text", &text)?;
                clip.set_child_property("background", 0x00000000u32.to_value())?;
            }
            _ => {
                let clip = ges::TestClip::new().context("test clip")?;
                clip.set_start(start);
                clip.set_duration(duration);
                clip.set_vpattern(ges::VideoTestPattern::Smpte);
                base_layer.add_clip(&clip).context("add test clip")?;
            }
        }
    }
    timeline.commit_sync();
    Ok(timeline)
}

fn main() -> Result<()> {
    init()?;

    let source = match std::env::args().nth(1) {
        Some(path) => std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?,
        None => DEMO_SCRIPT.to_string(),
    };

    let mut runtime = Runtime::new(RuntimeOptions::default())?;
    runtime.register_function("editor_addClip", add_clip)?;
    // Shim the ergonomic `editor.*` namespace over the registered op.
    runtime.eval::<()>(
        "globalThis.editor = { addClip: (c) => rustyscript.functions.editor_addClip(c) };",
    )?;

    let module = Module::new("user-script.ts", &source);
    let handle = runtime.load_module(&module)?;
    let total: i64 = runtime
        .get_value(Some(&handle), "total")
        .unwrap_or_default();
    println!("script ran; clips declared: {total}");

    let specs = CLIPS.lock().unwrap().clone();
    let timeline = build_timeline(&specs)?;

    let pipeline = ges::Pipeline::new();
    pipeline.set_timeline(&timeline).context("attaching timeline")?;
    let out = std::path::absolute("out/scripted.mp4")?;
    pipeline
        .set_render_settings(&dualcut_engine::media::uri_from_path(&out), &mp4_profile())
        .context("render settings")?;
    pipeline.set_mode(ges::PipelineFlags::RENDER)?;
    println!("rendering scripted timeline -> {}", out.display());
    run_to_eos(&pipeline)?;
    println!("done");
    Ok(())
}
