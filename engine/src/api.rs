//! File-backed agent API. The project file on disk is the bus between the
//! HTTP surface and whatever holds the file open (the editor app watches
//! mtime, so agent writes appear live). Serving and editing share no
//! memory — every request reads/writes the file.

use crate::document::Project;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tiny_http::{Header, Method, Response, Server};

fn json_response(status: u16, body: String) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body)
        .with_status_code(status)
        .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
}

/// Leave a short-lived marker next to the project's cache dir so the GUI's
/// mtime-poll reload can tell "an agent just wrote this via the HTTP API"
/// apart from "a human edited the file directly" for the Edit History
/// panel. Best-effort: a write that fails here still succeeded at its
/// actual job (the project file itself), so errors are swallowed.
fn touch_agent_marker(path: &Path, summary: &str) {
    let Some(dir) = path.parent() else { return };
    let cache = dir.join(".dualcut-cache");
    if std::fs::create_dir_all(&cache).is_err() {
        return;
    }
    let at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let body = serde_json::json!({"summary": summary, "at_unix_ms": at_unix_ms}).to_string();
    let _ = std::fs::write(cache.join("agent-edit.json"), body);
}

/// Serve the agent API for `path` on 127.0.0.1:`port`, blocking forever.
/// Spawn on a thread to run alongside a UI.
pub fn serve_file_api(path: PathBuf, port: u16) -> Result<()> {
    let server = Server::http(("127.0.0.1", port))
        .map_err(|e| anyhow::anyhow!("binding 127.0.0.1:{port}: {e}"))?;
    println!("agent API on http://127.0.0.1:{port} (project: {})", path.display());

    for mut request in server.incoming_requests() {
        let url = request.url().to_string();
        let method = request.method().clone();
        let mut body = String::new();
        let _ = request.as_reader().read_to_string(&mut body);

        let load = || -> Result<Project> {
            Project::from_json(&std::fs::read_to_string(&path)?)
                .with_context(|| format!("loading {}", path.display()))
        };

        let response = match (&method, url.as_str()) {
            (Method::Get, "/project") => match load() {
                Ok(p) => json_response(200, p.to_json()),
                Err(e) => json_response(500, format!(r#"{{"error":{:?}}}"#, e.to_string())),
            },
            (Method::Post, "/project") => match Project::from_json(&body) {
                Ok(project) => match std::fs::write(&path, project.to_json()) {
                    Ok(()) => {
                        touch_agent_marker(&path, "Replaced project (agent)");
                        json_response(200, r#"{"ok":true}"#.into())
                    }
                    Err(e) => json_response(500, format!(r#"{{"error":{:?}}}"#, e.to_string())),
                },
                Err(e) => json_response(400, format!(r#"{{"error":{:?}}}"#, e.to_string())),
            },
            #[cfg(feature = "scripting")]
            (Method::Post, "/script") => match load()
                .and_then(|p| crate::scripting::run_script(&body, &p))
            {
                Ok(edited) => match std::fs::write(&path, edited.to_json()) {
                    Ok(()) => {
                        touch_agent_marker(&path, "Ran script (agent)");
                        json_response(200, r#"{"ok":true}"#.into())
                    }
                    Err(e) => json_response(500, format!(r#"{{"error":{:?}}}"#, e.to_string())),
                },
                Err(e) => json_response(400, format!(r#"{{"error":{:?}}}"#, e.to_string())),
            },
            // Editing ops with nontrivial math, so agents don't have to
            // reimplement offset/animation splitting. Body: {"op": ...}.
            (Method::Post, "/op") => match load().and_then(|mut p| {
                let req: serde_json::Value = serde_json::from_str(&body)?;
                let op = req["op"].as_str().unwrap_or_default().to_string();
                let id = req["id"].as_str().unwrap_or_default().to_string();
                let result = match op.as_str() {
                    "split" => {
                        let at = req["at"].as_f64().context("'at' (seconds) required")?;
                        let new_id = crate::document::split_clip(&mut p, &id, at)?;
                        serde_json::json!({"ok": true, "new_id": new_id})
                    }
                    "ripple_delete" => {
                        crate::document::ripple_delete(&mut p, &id)?;
                        serde_json::json!({"ok": true})
                    }
                    "detach_audio" => {
                        let new_id = crate::document::detach_audio(&mut p, &id)
                            .context("clip is not a video clip")?;
                        serde_json::json!({"ok": true, "new_id": new_id})
                    }
                    "move_to_lane" => {
                        let lane = req["lane"].as_u64().context("'lane' required")? as usize;
                        let at = req["at"].as_f64().context("'at' (seconds) required")?;
                        crate::document::move_clip_to_lane(&mut p, &id, lane, at)?;
                        serde_json::json!({"ok": true})
                    }
                    // Detect + splice out silent stretches of the clip's
                    // own media (#46). threshold_db/min_duration optional
                    // (default -40.0 dBFS / 0.5s).
                    #[cfg(feature = "preview")]
                    "remove_silence" => {
                        let threshold_db = req["threshold_db"].as_f64().unwrap_or(-40.0);
                        let min_duration = req["min_duration"].as_f64().unwrap_or(0.5);
                        let clip = crate::document::find_clip(&p, &id).context("no such clip")?;
                        let src = match &clip.element {
                            crate::document::Element::Video { src, .. }
                            | crate::document::Element::Audio { src, .. } => src.clone(),
                            _ => anyhow::bail!("clip {id:?} has no media"),
                        };
                        let base_dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
                        let uri = crate::media::resolve(&src, base_dir)
                            .context("resolving media path")?;
                        let ranges = crate::silence::detect_silence_in_uri(&uri, threshold_db, min_duration)?;
                        let removed = crate::document::remove_silence(&mut p, &id, &ranges)?;
                        serde_json::json!({"ok": true, "removed": removed})
                    }
                    other => anyhow::bail!(
                        "unknown op {other:?} (split, ripple_delete, detach_audio, move_to_lane, remove_silence)"
                    ),
                };
                p.validate()?;
                std::fs::write(&path, p.to_json())?;
                touch_agent_marker(&path, &format!("{op} {id} (agent)"));
                Ok(result)
            }) {
                Ok(v) => json_response(200, v.to_string()),
                Err(e) => json_response(400, format!(r#"{{"error":{:?}}}"#, e.to_string())),
            },
            (Method::Get, "/status") => match load() {
                Ok(p) => json_response(
                    200,
                    serde_json::json!({
                        "engine": "dualcut",
                        "version": env!("CARGO_PKG_VERSION"),
                        "project": p.meta.title,
                        "duration": p.duration(),
                        "scenes": p.scenes.len(),
                    })
                    .to_string(),
                ),
                Err(e) => json_response(500, format!(r#"{{"error":{:?}}}"#, e.to_string())),
            },
            _ => json_response(404, r#"{"error":"not found"}"#.into()),
        };
        let _ = request.respond(response);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Clip, Element, Meta, Project, Scene};
    use std::io::{Read, Write};
    use std::net::TcpStream;

    fn test_project() -> Project {
        Project {
            meta: Meta { title: "api-test".into(), width: 320, height: 180, fps: 25 },
            library: Vec::new(),
            defs: Default::default(),
            scenes: vec![Scene {
                id: "s1".into(),
                name: String::new(),
                duration: 2.0,
                transition: None,
                layers: vec![Clip {
                    id: "c1".into(),
                    start: 0.0,
                    duration: 2.0,
                    element: Element::Test {},
                    transform: Default::default(),
                    animations: Vec::new(),
                    effects: Vec::new(),
                }],
            }],
            overlays: Vec::new(),
            scene_lanes: Vec::new(),
        }
    }

    /// Spawn the server against a fresh temp project file on a dedicated
    /// port (one per test to avoid cross-test collisions -- each test
    /// keeps its server thread running for the process lifetime, harmless
    /// since the whole binary exits once `cargo test` finishes) and wait
    /// until it's actually accepting connections before returning.
    fn spawn_server(port: u16) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dualcut-api-test-{port}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("project.json");
        std::fs::write(&path, test_project().to_json()).unwrap();
        let server_path = path.clone();
        std::thread::spawn(move || {
            let _ = serve_file_api(server_path, port);
        });
        for _ in 0..50 {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        path
    }

    /// Minimal hand-rolled HTTP/1.1 client -- no test-only dependency
    /// needed for requests this simple. Returns (status, body).
    fn http(port: u16, method: &str, path: &str, body: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        let (head, rest) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
        let status = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        (status, rest.to_string())
    }

    #[test]
    fn status_reports_project_metadata() {
        let port = 18391;
        spawn_server(port);
        let (status, body) = http(port, "GET", "/status", "");
        assert_eq!(status, 200);
        assert!(body.contains(r#""engine":"dualcut""#));
        assert!(body.contains(r#""project":"api-test""#));
        assert!(body.contains(r#""scenes":1"#));
    }

    #[test]
    fn get_project_returns_the_file_contents_as_json() {
        let port = 18392;
        let path = spawn_server(port);
        let (status, body) = http(port, "GET", "/project", "");
        assert_eq!(status, 200);
        let on_disk = std::fs::read_to_string(&path).unwrap();
        let returned = Project::from_json(&body).expect("valid project json");
        let expected = Project::from_json(&on_disk).unwrap();
        assert_eq!(returned.meta.title, expected.meta.title);
    }

    #[test]
    fn post_project_replaces_the_file_and_leaves_an_agent_marker() {
        let port = 18393;
        let path = spawn_server(port);
        let mut edited = test_project();
        edited.meta.title = "renamed".into();
        let (status, _) = http(port, "POST", "/project", &edited.to_json());
        assert_eq!(status, 200);
        let on_disk = Project::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk.meta.title, "renamed");
        let marker = path.parent().unwrap().join(".dualcut-cache").join("agent-edit.json");
        assert!(marker.exists(), "expected an agent-edit marker after a write");
    }

    #[test]
    fn post_project_with_invalid_json_returns_400_and_leaves_file_untouched() {
        let port = 18394;
        let path = spawn_server(port);
        let before = std::fs::read_to_string(&path).unwrap();
        let (status, body) = http(port, "POST", "/project", "{not valid json");
        assert_eq!(status, 400);
        assert!(body.contains("error"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn op_split_mutates_the_project_and_returns_a_new_id() {
        let port = 18395;
        let path = spawn_server(port);
        let (status, body) = http(port, "POST", "/op", r#"{"op":"split","id":"c1","at":1.0}"#);
        assert_eq!(status, 200, "response body: {body}");
        assert!(body.contains(r#""ok":true"#));
        assert!(body.contains("new_id"));
        let on_disk = Project::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk.scenes[0].layers.len(), 2, "split should produce two clips");
    }

    #[test]
    fn op_unknown_returns_400_with_a_helpful_message() {
        let port = 18396;
        spawn_server(port);
        let (status, body) = http(port, "POST", "/op", r#"{"op":"not-a-real-op","id":"c1"}"#);
        assert_eq!(status, 400);
        assert!(body.contains("unknown op"));
    }

    #[test]
    fn op_missing_clip_id_returns_400_not_500() {
        let port = 18397;
        spawn_server(port);
        let (status, _) =
            http(port, "POST", "/op", r#"{"op":"ripple_delete","id":"does-not-exist"}"#);
        assert_eq!(status, 400);
    }

    #[test]
    fn unknown_route_returns_404() {
        let port = 18398;
        spawn_server(port);
        let (status, body) = http(port, "GET", "/nope", "");
        assert_eq!(status, 404);
        assert!(body.contains("not found"));
    }
}
