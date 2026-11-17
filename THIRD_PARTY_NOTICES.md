# Third-party notices (initial implementation)

- Tauri 2 and Tauri plugins: MIT or Apache-2.0 (verify exact dependency notices in generated Cargo/npm lockfiles).
- `http-range` 0.1.6: MIT; transitive dependency enabled by Tauri's local asset protocol for media byte-range requests.
- React: MIT.
- Inter typeface via `@fontsource-variable/inter`: SIL Open Font License 1.1; bundled into the frontend build so no font is fetched from the network.
- Vite: MIT.
- TypeScript: Apache-2.0.
- SQLx: MIT or Apache-2.0.
- RustCrypto `sha2` crate: MIT or Apache-2.0; used to compute streamed SHA-256 fingerprints for project-scoped duplicate detection.
- FFmpeg development binaries: Gyan.dev FFmpeg 9.0.2 essentials build, downloaded from https://www.gyan.dev/ffmpeg/builds/packages/ffmpeg-9.0.2-essentials_build.7z (SHA-256 `4705843ccaaf54257c16ad90f3e952ece33c17df964ecf7bfdbb0f49c7171077`). The published build is static GPLv3 and enables `libx264`; it is kept in the ignored local `tools/ffmpeg/` folder for development and is not bundled or redistributed by this repository. The build includes FFmpeg/FFprobe and libraries such as libass, freetype and x264. Review the exact archive's included notices and source information before any redistribution. See the [Gyan build page](https://www.gyan.dev/ffmpeg/builds/) and [FFmpeg legal information](https://ffmpeg.org/legal.html).
- whisper.cpp (`whisper-cli`) and its ggml Whisper models: MIT. Used locally for the auto clipper's transcription; the binary and model are downloaded by the developer into the ignored `tools/whisper/` folder and are not bundled by this repository.
- yt-dlp: The Unlicense (public domain). Used locally to download a video from a link the user pastes; the executable is kept in the ignored `tools/yt-dlp/` folder and is not bundled. Users are responsible for only downloading videos they own or have permission to use.
- UltraFace face detector (`version-RFB-320.onnx`, Linzaer/Ultra-Light-Fast-Generic-Face-Detector-1MB): MIT. Kept in the ignored `tools/models/` folder; used locally to frame shorts on the people in each shot.
- tract (`tract-onnx`): MIT or Apache-2.0; runs the face detector inside the Rust backend.
- PostgreSQL: PostgreSQL License (permissive).
- Browser container: official NGINX Alpine image. NGINX source is distributed under a 2-clause BSD license; Alpine base-image packages retain their own notices inside the image. This repository adds only the NGINX site configuration and built frontend assets.
- Windows System.Speech: Windows platform API; Windows speech voices are separately supplied platform assets and may have their own terms.
- Clypra, Handy, PySceneDetect are research/reference candidates only; no source code from them is included in this archive.

This is an engineering inventory, not legal advice. Generate a complete SBOM and review every transitive dependency before public distribution.
