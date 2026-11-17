# Validation plan

## Functional smoke tests
- Start Docker PostgreSQL and confirm healthcheck.
- Launch app; verify migrations create `projects`, `media_assets`, `exports`.
- Run `npm run web`; verify the hidden local backend becomes healthy, the browser opens on loopback, project listing uses PostgreSQL, and the API refuses non-ShortForge origins.
- In browser mode, import a video and audio file, play media using byte-range requests, edit/save the timeline, and render an export. Confirm the browser service is bound only to 127.0.0.1.
- Confirm Source, Edit, Sound and Export navigation shows one focused step at a time; verify Projects opens/selects projects and Settings changes persist after reload.
- At 1728x768 and 1440x900, verify the editor canvas stays centered and capped, task panels do not stretch across unused space, each step has no nested panel scrollbar or duplicate bottom navigation, and delivery controls are visible. At 390px width, verify there is no horizontal overflow and any required scrolling is on the page rather than inside nested wizard panels.
- If the local browser host is unavailable, verify the connection indicator and status message explain that PostgreSQL/media features need the local host instead of showing a raw fetch exception.
- Create/list project.
- Import a video and verify project media reloads after switching projects and restarting the app.
- Verify import creates a JPEG thumbnail in the local thumbnail directory, stores its path in PostgreSQL, and displays it in the project media list.
- Verify older assets without a thumbnail receive one when their project library is loaded.
- Confirm repeated imports of the same file in one project are rejected; change its category and relink a missing asset.
- Remove a project media asset and verify its timeline/scene rows cascade away, its managed media and thumbnail files are deleted, and the original imported file is unchanged.
- Relink an asset and verify its thumbnail is regenerated from the replacement media.
- Probe H.264/AAC MP4, MOV, MKV and variable-frame-rate samples.
- Render a 30-second segment in 9:16, 1:1 and 16:9.
- Cancel an active render and confirm the partial output is removed.
- Add multiple clips, trim by source frame, split, reorder, undo/redo and verify the timeline survives an application restart.
- Play imported media in the source monitor; seek and frame-step the program monitor across clip boundaries.
- Run scene cut detection at different sensitivities; confirm candidate times and scores persist in PostgreSQL, adjacent frame candidates are grouped, and selected shot ranges preview and append to the timeline.
- Run silence detection with different dB thresholds and minimum durations; confirm ranges persist in PostgreSQL, trailing silence is bounded to media duration, previews seek to the source time, and removing an overlapping range splits/trims the selected timeline clip and can be undone.
- Confirm asset protocol playback is limited to the local ShortForge media directory and can seek within a video using byte ranges.
- Render a multi-clip timeline, including silent clips and mixed source-audio clips, then verify duration, 1080p dimensions, H.264/AAC streams, and export history.
- Import voice, music and effects files into a project; verify project-scoped duplicate detection and local copied paths. Place clips at offsets, edit source trims, gain and fades, restart the app, and verify edits persist.
- Render overlapping positioned audio clips with source audio; verify offsets, trims, fade behavior, mixed AAC output and that sources remain unchanged.
- Test optional SRT burn-in, voice-over WAV, music-only, voice+music and source-audio-only.
- Confirm source files are unchanged and outputs are non-empty.

## Visual/audio QA
- Inspect framing for portrait, landscape and rotated phone footage.
- Listen for clipping, loudness jumps, voice/music balance and sync drift.
- Confirm subtitles are readable and within safe areas.
- Compare CRF 18/20/23 and hardware encoders on the target laptop.
- Verify export with ffprobe and play in VLC/Windows Media Player.

## Release gates
Do not call the app production-ready until automated tests, Windows clean-machine install tests, FFmpeg licence audit, database backup/restore tests, and representative output QA pass.
