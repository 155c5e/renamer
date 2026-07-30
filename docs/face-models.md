# Face recognition models

Face identification needs two ONNX models that are **not** in this repository:

| Role | Model | Rough size |
|------|-------|-----------|
| Detection | SCRFD (e.g. `scrfd_500m_bnkps`) | ~2.5 MB |
| Embedding | ArcFace / MobileFaceNet-class recognizer | ~5–17 MB |

Drop them here and the app picks them up:

```
~/.local/share/renamer/models/detect.onnx      # Linux
~/.local/share/renamer/models/embed.onnx
~/Library/Application Support/renamer/models/  # macOS
```

(`RENAMER_DATA_DIR` moves this, like everything else under the data directory.)

## Why they aren't vendored

**Licensing.** InsightFace's code is MIT, but its pretrained packs — `buffalo_l`,
`buffalo_s`, `antelopev2` — are released for **non-commercial research use only**.
That is fine for organizing your own photos, and not fine to redistribute inside
a public repository. Committing them here would relicense someone else's weights
by accident.

If this ever ships as anything commercial, you need either a commercial licence
from InsightFace or a permissively-licensed model.

**Size and provenance.** Even the small models are megabytes of opaque binary. A
photo organizer should not carry those in its git history, and a build that
downloads weights from a third party at compile time is not a build you can
reproduce or audit.

## Inference runtime

`tract-onnx`, not `ort`.

`ort` wraps Microsoft's ONNX Runtime and is faster, with CoreML and CUDA
backends. It also links a native C++ library, which would mean shipping a
`.so`/`.dylib` next to the binary — and this project's whole distribution story
is "download one file, `chmod +x`, run it".

Indexing is batch work done once per photo and cached, so a pure-Rust runtime
being a few times slower costs a slower first index and nothing else. That is a
good trade for keeping the single binary.

## Expectations

Face recognition on a personal library works well on adults facing the camera,
and poorly on profiles, small or blurred faces, and children as they age. The
same person will land in two or three clusters sometimes.

That means merge/split/rename UI is not optional, and it is usually more work
than the inference itself. Plan for the clustering to be *reviewed*, never
trusted blindly — the same posture as near-duplicate matches.
