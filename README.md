# AudioConvert

**Any audio format in, any audio format out.**

A small desktop tool that converts audio files between formats in
bulk. Drag in files or a folder, pick a target format, press
Convert. Tags and cover art travel with the music; the originals
are never touched, and existing outputs are never overwritten.

- **Targets:** MP3 · FLAC · WAV · AAC · ALAC · OGG Vorbis · Opus · AIFF · WMA
- **Inputs:** everything the bundled ffmpeg can decode — APE, DSD
  (DSF/DFF), WavPack, TTA, Matroska, even the audio track of video
  files. MIDI files are listed but skipped (they need a synthesizer,
  not a converter).
- Bitrate (128–320 kbps), sample rate and channel options for the
  formats they apply to; conversions run on a worker thread with a
  stop button, so the window never freezes.
- Chinese / English UI following the system language.
- macOS and Windows, one small window, no setup.

Built with Rust + egui, powered by ffmpeg.

## Build

```sh
cargo build --release -p audioconvert-app
```

On macOS, double-click `编译Mac版.command` to build `AudioConvert.app`
(bundles ffmpeg from `packaging/macos/`).

## License

MIT © Ben Zhang
