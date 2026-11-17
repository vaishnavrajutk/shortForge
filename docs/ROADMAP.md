# Delivery plan

This is one product roadmap, not alternative product versions.

1. **Core runtime** (implemented in this code drop): Tauri shell, PostgreSQL/Docker, migrations, project CRUD, media probing, deterministic short rendering, SRT burn-in, optional voice/music mix, Windows system TTS.
2. **Media library**: copy/import workflow, metadata persistence, project-scoped SHA-256 duplicate detection, manual categories, missing-file detection, relinking, locally generated thumbnails, and per-asset removal with dependent timeline cleanup (implemented). Bulk organization remains planned.
3. **Editing**: initial single-track frame-based video timeline, append/split/trim/reorder/delete, undo/redo, PostgreSQL autosave, multi-clip render, local source/program monitors, playhead seeking and frame-step playback (implemented). Positioned project audio clips with source trims, voice/music/effects labels, gain and fades are implemented. Multitrack video, ripple editing, markers, waveforms and advanced playback controls remain.
4. **Non-AI assist**: scene cut and silence detection with reviewable suggestions are implemented using local FFmpeg and PostgreSQL. Silence ranges can be manually previewed and removed from the selected clip with undo support. A free auto clipper (local whisper.cpp transcription plus a pace/hook/energy heuristic) suggests reviewable clips (implemented). Shot grouping, caption generation from the transcript, and an optional free local LLM ranker remain. No paid or cloud services.
5. **Creator tools**: subtitle editor, title templates, transitions, overlays, audio ducking and automation, colour adjustments. Basic audio clip fades and gain are implemented.
6. **Quality and delivery**: platform export presets, rotation/VFR/HDR handling, hardware encoder detection, render queue, resume, validation and evidence report. The current single render reports progress and supports cancellation.
7. **Distribution**: offline-ready installer, bundled FFmpeg and fonts with notices, PostgreSQL Docker startup/health UX, backup/restore and upgrade migrations.
