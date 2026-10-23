# Recipe: auto-captions with local STT (#37)

Transcribe a clip's audio with any local speech-to-text tool and land
the segments as styled text clips on a `subtitles` overlay track. This
This recipe is the integration point for headless/agent workflows that drive the HTTP API from outside the app.

The GUI's *Generate Captions…* menu action (#37) is a simpler path for interactive use. The Flatpak build bundles a CPU-only whisper.cpp (`/app/bin/whisper-cli`) and a small ggml model (`/app/share/dualcut/models/ggml-tiny.en-q5_1.bin`), so it works with no install. `DUALCUT_WHISPER_MODEL` overrides the model path for either the bundled or a self-installed whisper-cli.

## 1. Extract the audio

```sh
# from the project's media file (or export just the audio):
dualcut-render project.json /tmp/voice.wav wav
```

## 2. Transcribe with timestamps

Any tool that emits timed segments works; whisper.cpp example:

```sh
whisper-cli -m ggml-base.en.bin -f /tmp/voice.wav --output-json /tmp/voice.json
```

## 3. Convert segments into subtitle clips

```python
import json, urllib.request

segs = json.load(open("/tmp/voice.json"))["transcription"]
project = json.load(urllib.request.urlopen("http://localhost:7357/project"))

track = next((t for t in project["overlays"] if t["id"] == "subtitles"), None)
if track is None:
    track = {"id": "subtitles", "name": "Subtitles", "clips": []}
    project["overlays"].append(track)

for i, seg in enumerate(segs):
    t0 = seg["offsets"]["from"] / 1000
    t1 = seg["offsets"]["to"] / 1000
    track["clips"].append({
        "id": f"sub-{i}",
        "start": round(t0, 2),
        "duration": round(t1 - t0, 2),
        "type": "text",
        "text": seg["text"].strip(),
        "font": "Sans Semi-Bold 22",
        "color": "#ffffff",
        "outline": "#000000",
        "align": "center",
    })

req = urllib.request.Request(
    "http://localhost:7357/project",
    data=json.dumps(project).encode(),
    headers={"Content-Type": "application/json"},
    method="POST",
)
urllib.request.urlopen(req)
```

The editor hot-reloads with the subtitles in place — review them on the timeline, retime via clip moves, restyle via a caption def.

## Notes

- Outline + center alignment for captions (since v0.20.0) keeps them readable over any footage.
- For burned-in style shared across captions, make a `caption` def and emit `compref` clips instead of raw text clips.
- If the HTTP API is off, edit the project file on disk instead; the app watches the file.
