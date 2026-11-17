# ShortForge

Turn a long video into vertical shorts, entirely on your own computer. Paste a YouTube link or import a file. ShortForge transcribes the speech locally, ranks the strongest 15-60 second moments, frames them on the speakers' faces and renders 9:16 clips. You review and edit every pick before anything renders.

Free and local: no paid services, no cloud APIs, no API keys, no telemetry.

![ShortForge home screen](docs/screenshots/home.png)

## Features
- **Auto shorts**: link or file in, ranked clip suggestions out. Each suggestion has an editable title, description and hashtags.
- **Local transcription** with [whisper.cpp](https://github.com/ggml-org/whisper.cpp). English uses a fast model; other languages use `large-v3-turbo`.
- **Gameplay mode** for console and PC captures with little or no talking: clips are picked from sudden bursts of game sound, sustained loudness and on-screen motion, and framed so the whole moment fits with a short lead-in. Add the game's name for matching titles and hashtags (e.g. `#gta6 #gta6gameplay`). Auto mode switches to it when speech covers less than 15% of the video, or when whisper is not installed.
- **Clip length** presets: 10–30 s, 15–60 s or 30–90 s.
- **Clip picking is a heuristic, not AI.** Windows are scored on speaking pace, opening hook, energy, clean sentence endings and, for YouTube imports, the "Most replayed" graph when available.
- **Face framing** with a small ONNX face detector (falls back to whole-frame Fit framing without it).
- **Editor**: single-track timeline (split, trim, reorder, undo/redo), positioned voice/music/effects clips, SRT burn-in, scene-cut and silence finders.
- **Media library** with thumbnails, SHA-256 duplicate checks and missing-file relinking.
- **Cancellable renders** with live FFmpeg progress. Source media is never overwritten.
- **Storage**: PostgreSQL holds metadata and paths only. Videos stay on the local filesystem.

## Requirements
- Windows 10/11 with the WebView2 Runtime
- Node.js 22+
- Rust stable with MSVC C++ build tools
- Docker Desktop (runs PostgreSQL)
- FFmpeg and FFprobe on `PATH`, or `SHORTFORGE_FFMPEG_DIR` pointing to their `bin` folder

Optional local tools. All are free; none are bundled (see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)):

| Feature | What to download | Where to put it |
| --- | --- | --- |
| Link import | `yt-dlp.exe` from the yt-dlp releases | `tools/yt-dlp/` or `SHORTFORGE_YTDLP_DIR` |
| Transcription | `whisper-cli.exe` from the whisper.cpp releases | `tools/whisper/` or `SHORTFORGE_WHISPER_DIR` |
| Transcription model | a ggml model, such as `ggml-base.en.bin` (English) or `ggml-large-v3-turbo-q5_0.bin` (other languages), from the `ggerganov/whisper.cpp` Hugging Face repo | `tools/whisper/` or `SHORTFORGE_WHISPER_MODEL` |
| Face framing | `version-RFB-320.onnx` (UltraFace, about 1 MB) | `tools/models/` or `SHORTFORGE_FACE_MODEL` |

Only download videos you own or have permission to use.

## Quick start
```powershell
git clone https://github.com/vishnuptdev/shortForge.git ShortForge
cd ShortForge
docker compose up -d postgres
npm install
npm run tauri:dev      # desktop app
```

Or the browser version:
```powershell
npm run web            # serves http://127.0.0.1:1420
```

Default database: `postgres://shortforge:shortforge_dev_password@127.0.0.1:55434/shortforge`. Override with `POSTGRES_PORT` and `DATABASE_URL`. Migrations run automatically at startup, and data persists in the `shortforge_pgdata` Docker volume.

## How to use it
1. **Get the video**: paste a link and press Start, or choose a file from your computer.
2. **Choose how many clips** once the download finishes.
3. **Review**: preview each suggested moment, edit its title, description and hashtags, and drop the ones you do not want.
4. **Render** the chosen clips as 9:16 shorts and copy the post text when you upload.

For manual edits, open a project and use the Source, Edit, Sound and Export steps.

## Browser version
`npm run web` builds the React frontend into an NGINX container (`127.0.0.1:1420`), starts PostgreSQL, builds an optimised Rust bridge on first run and launches it hidden at `127.0.0.1:1421`. The bridge gives the browser access to local media, FFmpeg and Windows speech, and shares the desktop app's database. Both ports bind to loopback. Keep the command running; after Ctrl+C run `docker compose stop web`. Use `npm run tauri:dev:webhost` only while developing Rust code: the debug bridge is slower and restarts (ending running jobs) on every source change.

## Build
```powershell
npm run tauri:build
```
FFmpeg is resolved from `PATH` and is not bundled. A distributable release must bundle a vetted FFmpeg build with its licence notices.

## Development checks
```powershell
npm run typecheck
npm run lint
npm run test
cd src-tauri; cargo test
```

## Data locations
- Database: Docker volume `shortforge_pgdata`
- Media and exports: `%LOCALAPPDATA%\ShortForge\media` and `%LOCALAPPDATA%\ShortForge\exports`

## Limitations
ShortForge is not a full nonlinear editor yet: no multitrack video, ripple edits, transitions, waveform editing or audio ducking. Voice generation uses non-neural Windows system voices. Validate output quality on your own media. See [docs/ROADMAP.md](docs/ROADMAP.md).

## Security
Change the example PostgreSQL password before exposing the database beyond localhost. The compose file binds PostgreSQL to loopback only.

## Documentation
[Architecture](docs/ARCHITECTURE.md) · [Roadmap](docs/ROADMAP.md) · [Test plan](docs/TEST_PLAN.md) · [Third-party notices](THIRD_PARTY_NOTICES.md)

## Author and licence
Made by vishnupt.dev. Released under the [MIT licence](LICENSE).
