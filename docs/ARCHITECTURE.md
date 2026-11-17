# Architecture and implementation decisions

## Runtime topology
- Tauri desktop shell: React UI + Rust commands.
- PostgreSQL 17 in Docker Compose is mandatory. The app retries the loopback database connection during startup and applies SQLx migrations.
- Original video/audio/subtitle files and rendered outputs remain in local directories. PostgreSQL stores metadata and paths.
- Timeline clips, their order, and exclusive source in/out frame numbers are stored in PostgreSQL. The first editing pass is one ordered video track; frame indices use each asset's probed `r_frame_rate` and variable-frame-rate accuracy is still limited.
- Imported audio is copied into the local media directory; PostgreSQL stores its path, hash and project metadata. Positioned audio clips store track labels, timeline offset, source trim, gain and fades. Rendering trims and processes each clip with FFmpeg filters, delays it to its timeline offset, then mixes it with source audio and other overlays.
- Removing video media deletes the PostgreSQL asset row and its dependent timeline/scene-cut rows, then removes the imported copy and thumbnail only after canonical-path checks keep each file inside its managed local directory. The original imported source is never removed.
- Scene cut candidates are calculated locally with FFmpeg's scene score filter, grouped to suppress adjacent duplicate frames, and stored by project and media path in PostgreSQL. Editors preview a suggested shot and explicitly add it to the video timeline.
- Silence ranges are detected locally with FFmpeg's `silencedetect` filter using an adjustable dB threshold and minimum duration. The reviewable ranges and settings are stored in PostgreSQL; editors can preview them and manually remove an overlapping range from the selected clip, with the normal timeline undo history available.
- FFmpeg-generated media thumbnails are small JPEG files in `%LOCALAPPDATA%\\ShortForge\\thumbnails`; PostgreSQL stores their paths. The frontend requests thumbnails through a project-scoped Tauri command that checks the resolved path stays inside this directory.
- Tauri's asset protocol serves local source media to the source/program HTML video monitors. Its scope is restricted to `%LOCALAPPDATA%\\ShortForge\\media`; no general filesystem access is enabled for playback.
- Browser mode builds the React frontend into an NGINX Docker container bound to `127.0.0.1:1420`. A hidden Windows Tauri host exposes the Rust HTTP bridge on `127.0.0.1:1421`; the bridge only allows the ShortForge localhost UI origin, reuses the same command handlers and PostgreSQL pool, streams local media with byte ranges, and never listens on LAN interfaces. Browser file inputs upload directly to the local bridge before media is copied into the managed media directory. The Windows host remains required for local filesystem access and System.Speech.
- The editor is divided into Source, Edit, Sound and Export steps. Projects and Settings are separate working views; aspect ratio and scene sensitivity are UI preferences, not application database records.
- FFmpeg/FFprobe are invoked as child processes with argument arrays.
- PostgreSQL is exposed on loopback port 55434 by default to avoid collisions with local PostgreSQL services; override it using `POSTGRES_PORT` when starting Compose and set `DATABASE_URL` for the app process if the port changes.
- FFmpeg resolution checks `SHORTFORGE_FFMPEG_DIR`, the local development `tools/ffmpeg/bin` folder, then PATH. The ignored local binaries are not part of release packaging.

## Non-AI feature policy
- No cloud endpoints, paid APIs, neural models, LLMs, AI transcription or AI-generated voice.
- Voice-over uses Windows System.Speech / installed Windows voices. Voice availability and quality depend on Windows language packs and installed voices.
- Captions are editable/imported SRT. Speech-to-text is excluded under the no-AI requirement.
- Manual clip selection and deterministic FFmpeg processing are implemented first. Scene cut detection may use non-neural pixel-change algorithms.

## Video quality policy
Current render settings: H.264, CRF 18, medium preset, yuv420p, AAC 192 kb/s, 30 fps, faststart. This is a sensible high-quality baseline, not a universal guarantee. Source frame rate, HDR, variable frame rate, rotation metadata, audio layouts, and source codec need additional handling and test coverage before a production release.
Audio target: loudnorm I=-16 LUFS, LRA=11, TP=-1.5 dBTP. Platform requirements change; presets must be configurable and measured, not assumed.
Vertical reframing currently uses centered crop. It does not track faces/subjects.

## Reuse policy
- FFmpeg is an external executable; distribute only a build whose exact LGPL/GPL configuration and notices have been audited.
- Clypra is a potential MIT-licensed reference for decoding/playback/timeline engineering; do not copy code until individual files and dependencies are reviewed.
- Handy is MIT but is speech-to-text and uses AI models; excluded from this no-AI product.
- PySceneDetect is BSD-3-Clause and Python/OpenCV-based; reference only unless packaging and runtime overhead are justified.
- Windows TTS can be implemented through Windows native speech APIs; voice assets are supplied/licensed by Microsoft or installed voice vendors.

## Known engineering work before production
- Media import copies the source into the app media directory and records its metadata in PostgreSQL.
- Add a queued render manager and failed-job recovery. Single-clip and timeline renders report FFmpeg progress and support cancellation, but interrupted-job recovery is not implemented.
- Expand the persistent single-track, frame-based timeline into multitrack editing with ripple operations and markers.
- Add audio waveforms, gain automation, ducking, voice selection UI and voice preview. Current audio controls edit clip-level gain and fades.
- Add shot grouping and manually curated highlight ranges. Scene cut and silence detection are implemented as local candidate finders; each suggestion remains subject to manual review.
- Add subtitle editor and style presets.
- Add output verification using ffprobe plus black-frame/silence/clipping checks.
- Bundle a specifically audited FFmpeg build and local fonts before offline release; the UI uses system fonts.
- Add integration tests with real video fixtures, Windows build pipeline, backup/restore, database health checks.
