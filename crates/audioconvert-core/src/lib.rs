//! AudioConvert core: convert any audio format to any audio format.
//!
//! ffmpeg does the heavy lifting — it decodes virtually every audio
//! format in the wild and encodes to all common ones. This crate
//! sniffs and probes the inputs, plans the outputs (never
//! overwriting, skipping same-format files), builds the encoder
//! arguments and runs the conversions one by one on a worker thread.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

// ---------------------------------------------------------------- i18n

/// UI language for user-facing messages: the app sets this once at
/// startup from the system language (Chinese by default, English
/// when the system is not Chinese).
static ENGLISH: AtomicBool = AtomicBool::new(false);

pub fn set_english(on: bool) {
    ENGLISH.store(on, Ordering::Relaxed);
}

fn txt<'a>(zh: &'a str, en: &'a str) -> &'a str {
    if ENGLISH.load(Ordering::Relaxed) {
        en
    } else {
        zh
    }
}

// ------------------------------------------------------- source format

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    Mp3,
    Flac,
    Wav,
    Aiff,
    Mp4,
    Ogg,
    Ape,
    Wma,
    Dsf,
    Dff,
    Midi,
    Matroska,
    Aac,
    Unknown,
}

impl SourceFormat {
    pub fn name(&self) -> &'static str {
        match self {
            SourceFormat::Mp3 => "MP3",
            SourceFormat::Flac => "FLAC",
            SourceFormat::Wav => "WAV",
            SourceFormat::Aiff => "AIFF",
            SourceFormat::Mp4 => "MP4/M4A",
            SourceFormat::Ogg => "OGG",
            SourceFormat::Ape => "APE",
            SourceFormat::Wma => "WMA",
            SourceFormat::Dsf => "DSF",
            SourceFormat::Dff => "DFF",
            SourceFormat::Midi => "MIDI",
            SourceFormat::Matroska => "MKA/MKV",
            SourceFormat::Aac => "AAC",
            SourceFormat::Unknown => "—",
        }
    }
}

const ASF_HEADER_GUID: [u8; 16] = [
    0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C,
];

/// Detect the container format from the file's leading bytes.
pub fn sniff(data: &[u8]) -> Option<SourceFormat> {
    if data.len() >= 4 && &data[0..4] == b"fLaC" {
        return Some(SourceFormat::Flac);
    }
    if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WAVE" {
        return Some(SourceFormat::Wav);
    }
    if data.len() >= 12 && &data[0..4] == b"FORM" && (&data[8..12] == b"AIFF" || &data[8..12] == b"AIFC") {
        return Some(SourceFormat::Aiff);
    }
    if data.len() >= 8 && &data[4..8] == b"ftyp" {
        return Some(SourceFormat::Mp4);
    }
    if data.len() >= 16 && data[0..16] == ASF_HEADER_GUID {
        return Some(SourceFormat::Wma);
    }
    if data.len() >= 4 && &data[0..4] == b"OggS" {
        return Some(SourceFormat::Ogg);
    }
    if data.len() >= 4 && &data[0..4] == b"DSD " {
        return Some(SourceFormat::Dsf);
    }
    if data.len() >= 4 && &data[0..4] == b"FRM8" {
        return Some(SourceFormat::Dff);
    }
    if data.len() >= 4 && &data[0..4] == b"MAC " {
        return Some(SourceFormat::Ape);
    }
    if data.len() >= 4 && &data[0..4] == b"MThd" {
        return Some(SourceFormat::Midi);
    }
    if data.len() >= 4 && data[0..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        return Some(SourceFormat::Matroska);
    }
    if data.len() >= 3 && &data[0..3] == b"ID3" {
        return Some(SourceFormat::Mp3);
    }
    // ADTS sync: 0xFFF with the two layer bits zero (checked before
    // the MP3 frame sync, which shares the 0xFF 0xEx/Fx lead).
    if data.len() >= 2 && data[0] == 0xFF && (data[1] & 0xF6) == 0xF0 {
        return Some(SourceFormat::Aac);
    }
    if data.len() >= 2 && data[0] == 0xFF && (data[1] & 0xE0) == 0xE0 {
        return Some(SourceFormat::Mp3);
    }
    None
}

/// Fallback for files whose head cannot be sniffed (too short, or a
/// container without a magic): guess from the file extension.
pub fn format_from_extension(path: &Path) -> Option<SourceFormat> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "mp3" => SourceFormat::Mp3,
        "flac" => SourceFormat::Flac,
        "wav" => SourceFormat::Wav,
        "aif" | "aiff" => SourceFormat::Aiff,
        "mp4" | "m4a" | "m4v" | "mov" => SourceFormat::Mp4,
        "ogg" | "oga" | "opus" => SourceFormat::Ogg,
        "ape" => SourceFormat::Ape,
        "wma" => SourceFormat::Wma,
        "dsf" => SourceFormat::Dsf,
        "dff" => SourceFormat::Dff,
        "mid" | "midi" => SourceFormat::Midi,
        "mka" | "mkv" => SourceFormat::Matroska,
        "aac" => SourceFormat::Aac,
        _ => return None,
    })
}

/// Sniff a file on disk: first bytes win, extension is the fallback.
pub fn detect_format(path: &Path) -> SourceFormat {
    use std::io::Read;
    let head = std::fs::File::open(path)
        .and_then(|mut f| {
            let mut buf = [0u8; 16];
            let n = f.read(&mut buf)?;
            Ok(buf[..n].to_vec())
        })
        .unwrap_or_default();
    sniff(&head)
        .or_else(|| format_from_extension(path))
        .unwrap_or(SourceFormat::Unknown)
}

// ------------------------------------------------------------- probing

#[derive(Debug, Clone, PartialEq)]
pub struct ProbeInfo {
    pub duration_secs: f64,
    /// Audio stream bitrate when known (kb/s).
    pub bitrate_kbps: Option<u32>,
    /// Audio codec name as ffmpeg reports it ("mp3", "flac", …).
    pub codec: String,
    pub sample_rate: Option<u32>,
    /// The file carries a cover picture (attached pic stream).
    pub has_attached_pic: bool,
}

/// Parse the stderr text of `ffmpeg -i <file>` into a ProbeInfo.
pub fn parse_probe(stderr: &str) -> Option<ProbeInfo> {
    let mut duration = None;
    let mut overall_bitrate = None;
    let mut codec = None;
    let mut stream_bitrate = None;
    let mut sample_rate = None;
    let mut has_pic = false;
    for line in stderr.lines() {
        let line = line.trim();
        if line.contains("attached pic") {
            has_pic = true;
        }
        if let Some(pos) = line.find("Duration: ") {
            let rest = &line[pos + 10..];
            let mut parts = rest.split(',');
            if let Some(hms) = parts.next() {
                duration = parse_hms(hms.trim());
            }
            for part in parts {
                let part = part.trim();
                if let Some(v) = part.strip_prefix("bitrate: ") {
                    overall_bitrate = v.trim().trim_end_matches(" kb/s").parse::<u32>().ok();
                }
            }
        }
        if line.contains("Audio: ") && codec.is_none() {
            let rest = &line[line.find("Audio: ").unwrap() + 7..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                codec = Some(name);
            }
            // "44100 Hz" inside the same stream line.
            if let Some(hz_pos) = rest.find(" Hz") {
                let before = &rest[..hz_pos];
                if let Some(num) = before.rsplit(|c: char| !(c.is_ascii_digit())).next() {
                    sample_rate = num.parse::<u32>().ok();
                }
            }
            // A trailing ", 320 kb/s" on the stream line is the
            // stream's own bitrate.
            if let Some(pos) = rest.rfind(" kb/s") {
                let before = &rest[..pos];
                if let Some(num) = before.rsplit(|c: char| !(c.is_ascii_digit())).next() {
                    stream_bitrate = num.parse::<u32>().ok();
                }
            }
        }
    }
    Some(ProbeInfo {
        duration_secs: duration?,
        bitrate_kbps: stream_bitrate.or(overall_bitrate),
        codec: codec?,
        sample_rate,
        has_attached_pic: has_pic,
    })
}

fn parse_hms(text: &str) -> Option<f64> {
    let mut it = text.split(':');
    let h: f64 = it.next()?.parse().ok()?;
    let m: f64 = it.next()?.parse().ok()?;
    let s: f64 = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some(h * 3600.0 + m * 60.0 + s)
}

/// "3:25" / "1:02:05" for the list's duration column.
pub fn format_duration(secs: f64) -> String {
    let total = secs.round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Probe one file with ffmpeg (runs `ffmpeg -i`, reads stderr).
pub fn probe(ffmpeg: &Path, path: &Path) -> Option<ProbeInfo> {
    let mut cmd = std::process::Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-i"]).arg(path);
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::piped());
    no_window(&mut cmd);
    let out = cmd.output().ok()?;
    parse_probe(&String::from_utf8_lossy(&out.stderr))
}

// ------------------------------------------------------ target formats

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetFormat {
    Mp3,
    Flac,
    Wav,
    Aac,
    Alac,
    Ogg,
    Opus,
    Aiff,
    Wma,
    WavPack,
    Tta,
    Mp2,
    Ac3,
    Eac3,
    Dts,
    Speex,
    Aptx,
    AptxHd,
    Sbc,
    Gsm,
}

/// Every output format ffmpeg can encode, in dropdown order.
/// APE and DSD are absent on purpose: ffmpeg has decoders for
/// them but no encoder exists, so they can never be targets.
pub const TARGET_FORMATS: [TargetFormat; 20] = [
    TargetFormat::Mp3,
    TargetFormat::Flac,
    TargetFormat::Wav,
    TargetFormat::Aac,
    TargetFormat::Alac,
    TargetFormat::Ogg,
    TargetFormat::Opus,
    TargetFormat::Aiff,
    TargetFormat::Wma,
    TargetFormat::WavPack,
    TargetFormat::Tta,
    TargetFormat::Mp2,
    TargetFormat::Ac3,
    TargetFormat::Eac3,
    TargetFormat::Dts,
    TargetFormat::Speex,
    TargetFormat::Aptx,
    TargetFormat::AptxHd,
    TargetFormat::Sbc,
    TargetFormat::Gsm,
];

impl TargetFormat {
    pub fn label(&self) -> &'static str {
        match self {
            TargetFormat::Mp3 => "MP3",
            TargetFormat::Flac => "FLAC",
            TargetFormat::Wav => "WAV",
            TargetFormat::Aac => "AAC (M4A)",
            TargetFormat::Alac => "ALAC (M4A)",
            TargetFormat::Ogg => "OGG Vorbis",
            TargetFormat::Opus => "Opus",
            TargetFormat::Aiff => "AIFF",
            TargetFormat::Wma => "WMA",
            TargetFormat::WavPack => "WavPack",
            TargetFormat::Tta => "TTA",
            TargetFormat::Mp2 => "MP2",
            TargetFormat::Ac3 => "AC3",
            TargetFormat::Eac3 => "E-AC3",
            TargetFormat::Dts => "DTS",
            TargetFormat::Speex => "Speex",
            TargetFormat::Aptx => "aptX",
            TargetFormat::AptxHd => "aptX HD",
            TargetFormat::Sbc => "SBC",
            TargetFormat::Gsm => "GSM",
        }
    }

    pub fn extension(&self) -> &'static str {
        match self {
            TargetFormat::Mp3 => "mp3",
            TargetFormat::Flac => "flac",
            TargetFormat::Wav => "wav",
            TargetFormat::Aac | TargetFormat::Alac => "m4a",
            TargetFormat::Ogg => "ogg",
            TargetFormat::Opus => "opus",
            TargetFormat::Aiff => "aiff",
            TargetFormat::Wma => "wma",
            TargetFormat::WavPack => "wv",
            TargetFormat::Tta => "tta",
            TargetFormat::Mp2 => "mp2",
            TargetFormat::Ac3 => "ac3",
            TargetFormat::Eac3 => "eac3",
            TargetFormat::Dts => "dts",
            TargetFormat::Speex => "spx",
            TargetFormat::Aptx => "aptx",
            TargetFormat::AptxHd => "aptxhd",
            TargetFormat::Sbc => "sbc",
            TargetFormat::Gsm => "gsm",
        }
    }

    pub fn lossy(&self) -> bool {
        matches!(
            self,
            TargetFormat::Mp3
                | TargetFormat::Aac
                | TargetFormat::Ogg
                | TargetFormat::Opus
                | TargetFormat::Wma
                | TargetFormat::Mp2
                | TargetFormat::Ac3
                | TargetFormat::Eac3
                | TargetFormat::Dts
                | TargetFormat::Speex
                | TargetFormat::Aptx
                | TargetFormat::AptxHd
                | TargetFormat::Sbc
                | TargetFormat::Gsm
        )
    }

    /// Whether this target can carry a cover picture copied from the
    /// source (ASF/WMA picture handling is too quirky to promise).
    pub fn supports_pic(&self) -> bool {
        matches!(
            self,
            TargetFormat::Mp3 | TargetFormat::Flac | TargetFormat::Aac | TargetFormat::Alac
        )
    }

    /// Encoder arguments for this target at the given bitrate
    /// (bitrate is ignored by the lossless targets).
    pub fn codec_args(&self, bitrate_kbps: u32) -> Vec<String> {
        let bitrate = format!("{bitrate_kbps}k");
        let pair = |codec: &str, with_bitrate: bool| -> Vec<String> {
            let mut v = vec!["-codec:a".to_string(), codec.to_string()];
            if with_bitrate {
                v.push("-b:a".to_string());
                v.push(bitrate.clone());
            }
            v
        };
        match self {
            TargetFormat::Mp3 => pair("libmp3lame", true),
            TargetFormat::Flac => pair("flac", false),
            TargetFormat::Wav => pair("pcm_s16le", false),
            TargetFormat::Aac => pair("aac", true),
            TargetFormat::Alac => pair("alac", false),
            TargetFormat::Ogg => pair("libvorbis", true),
            TargetFormat::Opus => pair("libopus", true),
            TargetFormat::Aiff => pair("pcm_s16be", false),
            TargetFormat::Wma => pair("wmav2", true),
            TargetFormat::WavPack => pair("wavpack", false),
            TargetFormat::Tta => pair("tta", false),
            TargetFormat::Mp2 => pair("mp2", true),
            TargetFormat::Ac3 => pair("ac3", true),
            TargetFormat::Eac3 => pair("eac3", true),
            // ffmpeg's DTS encoder is still flagged experimental,
            // so it needs an explicit opt-in or the run fails.
            TargetFormat::Dts => {
                let mut v = pair("dca", true);
                v.push("-strict".to_string());
                v.push("-2".to_string());
                v
            }
            TargetFormat::Speex => pair("libspeex", true),
            // The Bluetooth and telephony codecs run at fixed
            // rates of their own; a bitrate argument only makes
            // ffmpeg reject the run, so they don't get one.
            TargetFormat::Aptx => pair("aptx", false),
            TargetFormat::AptxHd => pair("aptx_hd", false),
            TargetFormat::Sbc => pair("sbc", false),
            TargetFormat::Gsm => pair("libgsm", false),
        }
    }

    /// True when a source already *is* this target format, so
    /// converting it would only re-encode for nothing.
    pub fn same_as(&self, source: SourceFormat, codec: Option<&str>) -> bool {
        match self {
            TargetFormat::Mp3 => source == SourceFormat::Mp3,
            TargetFormat::Flac => source == SourceFormat::Flac,
            TargetFormat::Wav => source == SourceFormat::Wav,
            TargetFormat::Aiff => source == SourceFormat::Aiff,
            TargetFormat::Wma => source == SourceFormat::Wma,
            TargetFormat::Ogg => source == SourceFormat::Ogg && codec == Some("vorbis"),
            TargetFormat::Opus => source == SourceFormat::Ogg && codec == Some("opus"),
            TargetFormat::Aac => source == SourceFormat::Mp4 && codec == Some("aac"),
            TargetFormat::Alac => source == SourceFormat::Mp4 && codec == Some("alac"),
            // The newer targets are recognised by codec name
            // alone: their containers have no SourceFormat of
            // their own, but the probed codec settles it.
            TargetFormat::WavPack => codec == Some("wavpack"),
            TargetFormat::Tta => codec == Some("tta"),
            TargetFormat::Mp2 => codec == Some("mp2"),
            TargetFormat::Ac3 => codec == Some("ac3"),
            TargetFormat::Eac3 => codec == Some("eac3"),
            TargetFormat::Dts => codec == Some("dts"),
            TargetFormat::Speex => codec == Some("speex"),
            TargetFormat::Aptx => codec == Some("aptx"),
            TargetFormat::AptxHd => codec == Some("aptx_hd"),
            TargetFormat::Sbc => codec == Some("sbc"),
            TargetFormat::Gsm => codec == Some("gsm") || codec == Some("gsm_ms"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleRate {
    Keep,
    Hz(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channels {
    Keep,
    Stereo,
    Mono,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OutputMode {
    /// Next to the source file.
    SameFolder,
    /// Into this folder, mirroring each file's path relative to
    /// the root it was scanned from.
    Custom(PathBuf),
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub target: TargetFormat,
    pub bitrate_kbps: u32,
    pub sample_rate: SampleRate,
    pub channels: Channels,
    pub output: OutputMode,
    /// Keep the source's tags and cover picture. When false the
    /// output is stripped bare: no metadata mapping, no picture,
    /// and CUE tracks get no stamped tags either.
    pub keep_tags: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            target: TargetFormat::Mp3,
            bitrate_kbps: 320,
            sample_rate: SampleRate::Keep,
            channels: Channels::Keep,
            output: OutputMode::SameFolder,
            keep_tags: true,
        }
    }
}

/// The folder a conversion lands in: beside the source, or in
/// the custom folder with the source's relative path mirrored.
fn output_base(src: &Path, root: &Path, settings: &Settings) -> PathBuf {
    match &settings.output {
        OutputMode::SameFolder => src.parent().map(|p| p.to_path_buf()).unwrap_or_default(),
        OutputMode::Custom(dir) => {
            let rel = src.strip_prefix(root).unwrap_or(src);
            dir.join(rel.parent().unwrap_or(Path::new("")))
        }
    }
}

/// Where a source file's conversion lands.
pub fn plan_output(src: &Path, root: &Path, settings: &Settings) -> PathBuf {
    let stem = src.file_stem().unwrap_or_default();
    output_base(src, root, settings)
        .join(stem)
        .with_extension(settings.target.extension())
}

// ------------------------------------------------------------ convert

#[derive(Debug)]
pub enum ConvertError {
    Stopped,
    /// ffmpeg ran and failed; carries its last words.
    Failed(String),
    Io(String),
}

impl std::fmt::Display for ConvertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConvertError::Stopped => write!(f, "{}", txt("已停止", "Stopped")),
            ConvertError::Failed(why) => write!(f, "{why}"),
            ConvertError::Io(why) => write!(f, "{why}"),
        }
    }
}
impl std::error::Error for ConvertError {}

/// The full ffmpeg argument list for one conversion (pure, so the
/// tests can pin the encoder setup down).
pub fn build_args(
    src: &Path,
    tmp: &Path,
    settings: &Settings,
    keep_pic: bool,
    src_sample_rate: Option<u32>,
) -> Vec<String> {
    let mut args: Vec<String> = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-y",
        "-i",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(src.to_string_lossy().into_owned());
    args.push("-map".into());
    args.push("0:a:0".into());
    if keep_pic && settings.keep_tags {
        args.push("-map".into());
        args.push("0:v:0".into());
        args.push("-c:v".into());
        args.push("copy".into());
        args.push("-disposition:v:0".into());
        args.push("attached_pic".into());
    }
    // -map_metadata -1 is the explicit strip: without it ffmpeg
    // would still copy global metadata over by default.
    args.push("-map_metadata".into());
    args.push(if settings.keep_tags { "0" } else { "-1" }.to_string());
    append_encode_args(&mut args, settings, src_sample_rate);
    args.push(tmp.to_string_lossy().into_owned());
    args
}

/// Codec, sample-rate and channel arguments shared by whole-file
/// and per-track conversions.
fn append_encode_args(args: &mut Vec<String>, settings: &Settings, src_sample_rate: Option<u32>) {
    args.extend(settings.target.codec_args(settings.bitrate_kbps));
    // Output sample rate: the user's choice wins; otherwise keep
    // the source's, except that lossy encoders cannot take the
    // very high rates DSD decodes to, so clamp those to 48 kHz.
    let mut rate = match settings.sample_rate {
        SampleRate::Hz(hz) => Some(hz),
        SampleRate::Keep => match src_sample_rate {
            Some(r) if settings.target.lossy() && r > 48_000 => Some(48_000),
            _ => None,
        },
    };
    // A few encoders only accept a small set of sample rates and
    // refuse anything else outright; when the effective rate (the
    // user's pick, else the source's) falls outside the set, fall
    // back to the codec's most standard rate instead of failing.
    let effective = rate.or(src_sample_rate);
    let mut clamp_rate = |valid: &[u32], fallback: u32| {
        if let Some(r) = effective {
            if !valid.contains(&r) {
                rate = Some(fallback);
            }
        }
    };
    match settings.target {
        // GSM is a telephony codec: 8 kHz mono or nothing.
        TargetFormat::Gsm => rate = Some(8_000),
        TargetFormat::Aptx | TargetFormat::AptxHd => clamp_rate(&[32_000, 44_100, 48_000], 48_000),
        TargetFormat::Sbc => clamp_rate(&[16_000, 32_000, 44_100, 48_000], 48_000),
        TargetFormat::Speex => clamp_rate(&[8_000, 16_000, 32_000], 32_000),
        _ => {}
    }
    if let Some(r) = rate {
        args.push("-ar".into());
        args.push(r.to_string());
    }
    let channels = if settings.target == TargetFormat::Gsm {
        Channels::Mono
    } else {
        settings.channels
    };
    match channels {
        Channels::Keep => {}
        Channels::Stereo => {
            args.push("-ac".into());
            args.push("2".into());
        }
        Channels::Mono => {
            args.push("-ac".into());
            args.push("1".into());
        }
    }
}

/// Convert one file. Writes to a temp file beside the destination
/// first and renames it into place only on success, so a failed or
/// stopped conversion never leaves a half-written file behind.
/// The destination must not exist (the caller plans and checks);
/// that is re-checked here just before the rename.
pub fn convert(
    ffmpeg: &Path,
    src: &Path,
    dst: &Path,
    settings: &Settings,
    probe: Option<&ProbeInfo>,
    stop: &AtomicBool,
) -> Result<u64, ConvertError> {
    let io = |e: std::io::Error| ConvertError::Io(e.to_string());
    let Some(dir) = dst.parent() else {
        return Err(ConvertError::Io(txt("路径不对", "Bad path").to_string()));
    };
    std::fs::create_dir_all(dir).map_err(io)?;
    let stem = src.file_stem().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(
        ".{stem}.audioconvert-tmp.{}",
        settings.target.extension()
    ));
    let _ = std::fs::remove_file(&tmp);
    let keep_pic = settings.target.supports_pic()
        && probe.map(|p| p.has_attached_pic).unwrap_or(false);
    let args = build_args(
        src,
        &tmp,
        settings,
        keep_pic,
        probe.and_then(|p| p.sample_rate),
    );
    run_ffmpeg(ffmpeg, &args, &tmp, dst, src, stop)
}

/// Spawn ffmpeg with `args`, watch it (killing it when `stop` is
/// set), and on success move the temp output into place. Shared by
/// whole-file and per-track conversions.
fn run_ffmpeg(
    ffmpeg: &Path,
    args: &[String],
    tmp: &Path,
    dst: &Path,
    src: &Path,
    stop: &AtomicBool,
) -> Result<u64, ConvertError> {
    let io = |e: std::io::Error| ConvertError::Io(e.to_string());
    let mut cmd = std::process::Command::new(ffmpeg);
    cmd.args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    no_window(&mut cmd);
    let mut child = cmd.spawn().map_err(io)?;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if stop.load(Ordering::Relaxed) {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = std::fs::remove_file(&tmp);
                    return Err(ConvertError::Stopped);
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(io(e));
            }
        }
    };
    if !status.success() {
        // ffmpeg's stderr says why; keep its last meaningful line.
        let mut err_text = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            use std::io::Read;
            let _ = pipe.read_to_string(&mut err_text);
        }
        let _ = std::fs::remove_file(&tmp);
        let last = err_text
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("ffmpeg failed")
            .trim()
            .chars()
            .take(200)
            .collect::<String>();
        return Err(ConvertError::Failed(last));
    }
    if dst.exists() {
        let _ = std::fs::remove_file(&tmp);
        return Err(ConvertError::Failed(
            txt("目标文件已存在", "Output already exists").to_string(),
        ));
    }
    std::fs::rename(&tmp, dst).map_err(io)?;
    let size = std::fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
    if let Ok(md) = std::fs::metadata(src) {
        let _ = std::fs::set_permissions(dst, md.permissions());
    }
    Ok(size)
}

// -------------------------------------------------------------- ffmpeg

/// Locate an ffmpeg to convert with: bundled with the app first,
/// then the usual Homebrew locations, then PATH.
pub fn find_ffmpeg() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let name = if cfg!(target_os = "windows") {
                "ffmpeg.exe"
            } else {
                "ffmpeg"
            };
            let beside = dir.join(name);
            if beside.is_file() {
                return Some(beside);
            }
            // Packaged layout: the bundled ffmpeg lives in a
            // Resources folder next to the exe.
            let sibling = dir.join("Resources").join(name);
            if sibling.is_file() {
                return Some(sibling);
            }
            // .app layout: Contents/MacOS/<exe> + Contents/Resources/ffmpeg
            let resources = dir.join("../Resources/ffmpeg");
            if resources.is_file() {
                return Some(resources);
            }
        }
    }
    for candidate in ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg"] {
        let p = PathBuf::from(candidate);
        if p.is_file() {
            return Some(p);
        }
    }
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    no_window(&mut cmd);
    if cmd.status().map(|st| st.success()).unwrap_or(false) {
        Some(PathBuf::from("ffmpeg"))
    } else {
        None
    }
}

fn no_window(#[allow(unused_variables)] cmd: &mut std::process::Command) {
    // Windows: no console window for the child process.
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
}

// ------------------------------------------------------------ scanning

/// Extensions the scanner picks up: audio first, plus the common
/// video containers whose audio track can be extracted.
pub const AUDIO_EXTS: [&str; 30] = [
    "mp3", "flac", "wav", "aif", "aiff", "mp4", "m4a", "m4v", "mov", "mkv", "mka", "ogg", "oga",
    "opus", "ape", "wma", "dsf", "dff", "mid", "midi", "wv", "tta", "aac", "ac3", "dts", "mp2",
    "m2a", "aifc", "caf", "au",
];

/// Recursively collect convertible files under a folder (hidden
/// folders are skipped).
pub fn collect_audio_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                let hidden = path
                    .file_name()
                    .map(|n| n.to_string_lossy().starts_with('.'))
                    .unwrap_or(false);
                if !hidden {
                    walk(&path, out);
                }
            } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                let ext = ext.to_ascii_lowercase();
                if AUDIO_EXTS.contains(&ext.as_str()) || ext == "cue" {
                    out.push(path);
                }
            }
        }
    }
    walk(root, &mut out);
    // An audio file a CUE sheet points at is represented by the
    // sheet's tracks, so it must not also appear as a plain file.
    let mut referenced: Vec<PathBuf> = Vec::new();
    for cue in out.iter().filter(|p| is_cue_path(p)) {
        if let Ok(sheet) = parse_cue(cue) {
            referenced.extend(sheet.tracks.iter().map(|t| t.file.clone()));
        }
    }
    if !referenced.is_empty() {
        out.retain(|p| is_cue_path(p) || !referenced.contains(p));
    }
    out
}

fn is_cue_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("cue"))
        .unwrap_or(false)
}

// ---------------------------------------------------------------- cue

/// One track of a CUE sheet, resolved against the sheet on disk:
/// `file` is the audio file to cut and `start`/`end` are seconds
/// (`end` is None for a file's last track: cut to EOF). Album-level
/// fields are copied onto every track, so a track carries
/// everything its conversion needs.
#[derive(Debug, Clone, PartialEq)]
pub struct CueTrack {
    pub number: u32,
    pub title: String,
    pub performer: String,
    pub album: String,
    pub date: Option<String>,
    pub genre: Option<String>,
    pub total: u32,
    pub file: PathBuf,
    pub start: f64,
    pub end: Option<f64>,
}

impl CueTrack {
    pub fn duration(&self) -> Option<f64> {
        self.end.map(|e| e - self.start)
    }

    /// "01 - Title", made safe as a file name.
    pub fn file_stem(&self) -> String {
        let cleaned = sanitize_stem(&self.title);
        if cleaned.is_empty() {
            format!("Track {:02}", self.number)
        } else {
            format!("{:02} - {}", self.number, cleaned)
        }
    }

    /// The cut and the tags for this track's conversion.
    pub fn slice(&self) -> TrackSlice {
        let mut metadata = vec![("title".to_string(), self.title.clone())];
        if !self.performer.is_empty() {
            metadata.push(("artist".to_string(), self.performer.clone()));
        }
        if !self.album.is_empty() {
            metadata.push(("album".to_string(), self.album.clone()));
        }
        metadata.push(("track".to_string(), format!("{}/{}", self.number, self.total)));
        if let Some(date) = &self.date {
            metadata.push(("date".to_string(), date.clone()));
        }
        if let Some(genre) = &self.genre {
            metadata.push(("genre".to_string(), genre.clone()));
        }
        TrackSlice {
            start: self.start,
            end: self.end,
            metadata,
        }
    }
}

/// A parsed CUE sheet.
#[derive(Debug, Clone, PartialEq)]
pub struct CueSheet {
    pub album: String,
    pub performer: String,
    pub tracks: Vec<CueTrack>,
}

/// Make a string safe as a file-name stem: filename-hostile
/// characters become spaces, runs collapse, edge dots go away.
pub fn sanitize_stem(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() {
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.trim_matches(|c| c == '.' || c == ' ').to_string()
}

/// CUE sheets in the wild are UTF-8, the ripper's ANSI codepage
/// (Shift-JIS on Japanese Windows, GBK on Chinese), rarely with a
/// BOM. Decode in that order of likelihood.
fn decode_cue_bytes(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        if let Ok(s) = std::str::from_utf8(rest) {
            return s.to_string();
        }
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let (sjis, _, sjis_bad) = encoding_rs::SHIFT_JIS.decode(bytes);
    if !sjis_bad {
        return sjis.into_owned();
    }
    let (gb, _, _) = encoding_rs::GB18030.decode(bytes);
    gb.into_owned()
}

/// A CUE time stamp: mm:ss:ff with 75 frames per second.
fn parse_cue_time(s: &str) -> Option<f64> {
    let mut parts = s.split(':');
    let m: f64 = parts.next()?.parse().ok()?;
    let sec: f64 = parts.next()?.parse().ok()?;
    let ff: f64 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0.0);
    Some(m * 60.0 + sec + ff / 75.0)
}

/// The value of a CUE command: a "quoted string" or a bare token
/// run to the end of the line.
fn cue_value(s: &str) -> String {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('"') {
        match rest.find('"') {
            Some(end) => rest[..end].to_string(),
            None => rest.to_string(),
        }
    } else {
        s.to_string()
    }
}

/// Resolve a FILE entry against the sheet's folder. Rips often
/// rename the audio (the sheet still says .wav, the file on disk
/// is .ape), so fall back to any same-stem audio file there.
fn resolve_cue_file(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if candidate.is_file() {
        return candidate;
    }
    if let Some(stem) = candidate.file_stem().and_then(|s| s.to_str()) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                let same_stem = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.eq_ignore_ascii_case(stem))
                    .unwrap_or(false);
                let audio = p
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| AUDIO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
                    .unwrap_or(false);
                if same_stem && audio {
                    return p;
                }
            }
        }
    }
    candidate
}

/// The CUE sheet sitting next to an audio file, if there is one.
pub fn cue_sidecar(audio: &Path) -> Option<PathBuf> {
    for ext in ["cue", "CUE", "Cue"] {
        let p = audio.with_extension(ext);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Parse a CUE sheet from disk.
pub fn parse_cue(path: &Path) -> Result<CueSheet, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let text = decode_cue_bytes(&bytes);
    let dir = path.parent().unwrap_or(Path::new("")).to_path_buf();

    struct Raw {
        number: u32,
        title: Option<String>,
        performer: Option<String>,
        file_idx: usize,
        start: Option<f64>,
    }

    let mut album = String::new();
    let mut album_performer = String::new();
    let mut genre: Option<String> = None;
    let mut date: Option<String> = None;
    let mut files: Vec<String> = Vec::new();
    let mut raw: Vec<Raw> = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.splitn(2, char::is_whitespace);
        let kw = it.next().unwrap_or("").to_ascii_uppercase();
        let rest = it.next().unwrap_or("").trim();
        match kw.as_str() {
            "REM" => {
                let mut rit = rest.splitn(2, char::is_whitespace);
                let sub = rit.next().unwrap_or("").to_ascii_uppercase();
                let val = cue_value(rit.next().unwrap_or(""));
                match sub.as_str() {
                    "GENRE" => genre = Some(val),
                    "DATE" | "YEAR" => date = Some(val),
                    _ => {}
                }
            }
            "FILE" => {
                files.push(cue_value(rest));
            }
            "TRACK" => {
                let number = rest
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(raw.len() as u32 + 1);
                raw.push(Raw {
                    number,
                    title: None,
                    performer: None,
                    file_idx: files.len().saturating_sub(1),
                    start: None,
                });
            }
            "TITLE" => {
                if let Some(t) = raw.last_mut() {
                    t.title = Some(cue_value(rest));
                } else {
                    album = cue_value(rest);
                }
            }
            "PERFORMER" => {
                if let Some(t) = raw.last_mut() {
                    t.performer = Some(cue_value(rest));
                } else {
                    album_performer = cue_value(rest);
                }
            }
            "INDEX" => {
                let mut iit = rest.split_whitespace();
                let idx = iit.next().unwrap_or("");
                let when = iit.next().and_then(parse_cue_time);
                if let (Some(t), Some(when)) = (raw.last_mut(), when) {
                    // The track starts at INDEX 01; INDEX 00 is
                    // the pregap and only a fallback.
                    if idx == "01" || t.start.is_none() {
                        t.start = Some(when);
                    }
                }
            }
            _ => {}
        }
    }

    let total = raw.len() as u32;
    let resolved: Vec<PathBuf> = files.iter().map(|f| resolve_cue_file(&dir, f)).collect();
    let mut tracks: Vec<CueTrack> = Vec::new();
    for r in raw {
        let Some(start) = r.start else { continue };
        let Some(file) = resolved.get(r.file_idx).cloned() else {
            continue;
        };
        tracks.push(CueTrack {
            number: r.number,
            title: r.title.unwrap_or_else(|| format!("Track {:02}", r.number)),
            performer: r.performer.unwrap_or_else(|| album_performer.clone()),
            album: album.clone(),
            date: date.clone(),
            genre: genre.clone(),
            total,
            file,
            start,
            end: None,
        });
    }
    // A track ends where the next track on the same file begins.
    for i in 0..tracks.len() {
        if i + 1 < tracks.len() && tracks[i + 1].file == tracks[i].file {
            tracks[i].end = Some(tracks[i + 1].start);
        }
    }
    if tracks.is_empty() {
        return Err(txt("CUE 里没有曲目", "No tracks in the CUE sheet").to_string());
    }
    Ok(CueSheet {
        album,
        performer: album_performer,
        tracks,
    })
}

/// Where one track's conversion lands: the usual output folder,
/// the track's own "NN - Title" name.
pub fn plan_track_output(
    src: &Path,
    root: &Path,
    settings: &Settings,
    track: &CueTrack,
) -> PathBuf {
    output_base(src, root, settings)
        .join(track.file_stem())
        .with_extension(settings.target.extension())
}

/// One track's cut: where it starts and ends in the source file,
/// plus the tags to stamp on the output.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackSlice {
    pub start: f64,
    pub end: Option<f64>,
    pub metadata: Vec<(String, String)>,
}

/// The full ffmpeg argument list for one track (pure, like
/// build_args, so tests can pin it down). Source metadata is NOT
/// mapped: on a whole-disc file it describes the disc, not the
/// track — the track's own tags are set explicitly instead.
pub fn build_track_args(
    src: &Path,
    tmp: &Path,
    settings: &Settings,
    keep_pic: bool,
    src_sample_rate: Option<u32>,
    slice: &TrackSlice,
) -> Vec<String> {
    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if slice.start > 0.001 {
        args.push("-ss".into());
        args.push(format!("{:.3}", slice.start));
    }
    args.push("-i".into());
    args.push(src.to_string_lossy().into_owned());
    if let Some(end) = slice.end {
        args.push("-t".into());
        args.push(format!("{:.3}", (end - slice.start).max(0.0)));
    }
    args.push("-map".into());
    args.push("0:a:0".into());
    if keep_pic && settings.keep_tags {
        args.push("-map".into());
        args.push("0:v:0".into());
        args.push("-c:v".into());
        args.push("copy".into());
        args.push("-disposition:v:0".into());
        args.push("attached_pic".into());
    }
    append_encode_args(&mut args, settings, src_sample_rate);
    if settings.keep_tags {
        for (k, v) in &slice.metadata {
            args.push("-metadata".into());
            args.push(format!("{k}={v}"));
        }
    }
    args.push(tmp.to_string_lossy().into_owned());
    args
}

/// Convert one track out of a whole-disc file. Same temp-file
/// discipline as convert(): the destination appears only once
/// the track is fully written.
pub fn convert_track(
    ffmpeg: &Path,
    src: &Path,
    dst: &Path,
    settings: &Settings,
    probe: Option<&ProbeInfo>,
    stop: &AtomicBool,
    slice: &TrackSlice,
) -> Result<u64, ConvertError> {
    let io = |e: std::io::Error| ConvertError::Io(e.to_string());
    let Some(dir) = dst.parent() else {
        return Err(ConvertError::Io(txt("路径不对", "Bad path").to_string()));
    };
    std::fs::create_dir_all(dir).map_err(io)?;
    let stem = dst.file_stem().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(
        ".{stem}.audioconvert-tmp.{}",
        settings.target.extension()
    ));
    let _ = std::fs::remove_file(&tmp);
    let keep_pic = settings.target.supports_pic()
        && probe.map(|p| p.has_attached_pic).unwrap_or(false);
    let args = build_track_args(
        src,
        &tmp,
        settings,
        keep_pic,
        probe.and_then(|p| p.sample_rate),
        slice,
    );
    run_ffmpeg(ffmpeg, &args, &tmp, dst, src, stop)
}

// --------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn cue_test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "audioconvert-cue-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_a_cue_sheet() {
        let dir = cue_test_dir("utf8");
        std::fs::write(dir.join("album.ape"), b"").unwrap();
        let cue = "REM GENRE Rock\nREM DATE 1994\nPERFORMER \"Album Artist\"\nTITLE \"The Album\"\nFILE \"album.ape\" WAVE\n  TRACK 1 AUDIO\n    TITLE \"First Song\"\n    INDEX 01 00:00:00\n  TRACK 2 AUDIO\n    TITLE \"Second: Song?\"\n    PERFORMER \"Guest\"\n    INDEX 01 01:05:37\n  TRACK 3 AUDIO\n    TITLE \"Third Song\"\n    INDEX 00 01:58:00\n    INDEX 01 02:00:00\n";
        std::fs::write(dir.join("album.cue"), cue).unwrap();
        let sheet = parse_cue(&dir.join("album.cue")).unwrap();
        assert_eq!(sheet.album, "The Album");
        assert_eq!(sheet.tracks.len(), 3);
        let t1 = &sheet.tracks[0];
        assert_eq!(t1.title, "First Song");
        assert_eq!(t1.performer, "Album Artist");
        assert_eq!(t1.start, 0.0);
        assert_eq!(t1.end, Some(65.0 + 37.0 / 75.0));
        assert_eq!(t1.genre.as_deref(), Some("Rock"));
        assert_eq!(t1.date.as_deref(), Some("1994"));
        assert_eq!(t1.total, 3);
        assert!(t1.file.ends_with("album.ape"));
        assert_eq!(t1.file_stem(), "01 - First Song");
        let t2 = &sheet.tracks[1];
        assert_eq!(t2.performer, "Guest");
        assert_eq!(t2.file_stem(), "02 - Second Song");
        let t3 = &sheet.tracks[2];
        assert_eq!(t3.start, 120.0);
        assert_eq!(t3.end, None);
        assert_eq!(t3.duration(), None);
        let slice = t1.slice();
        assert!(slice.metadata.contains(&("track".into(), "1/3".into())));
        assert!(slice.metadata.contains(&("title".into(), "First Song".into())));
    }

    #[test]
    fn parses_a_shift_jis_cue_and_finds_renamed_audio() {
        let dir = cue_test_dir("sjis");
        // The sheet says .wav; the rip on disk is .flac.
        std::fs::write(dir.join("disc.flac"), b"").unwrap();
        let text = "PERFORMER \"歌手\"\nTITLE \"專輯\"\nFILE \"disc.wav\" WAVE\n  TRACK 1 AUDIO\n    TITLE \"青い空\"\n    INDEX 01 00:00:00\n";
        let (bytes, _, _) = encoding_rs::SHIFT_JIS.encode(text);
        std::fs::write(dir.join("disc.cue"), bytes.as_ref()).unwrap();
        let sheet = parse_cue(&dir.join("disc.cue")).unwrap();
        assert_eq!(sheet.tracks.len(), 1);
        assert_eq!(sheet.tracks[0].title, "青い空");
        assert!(sheet.tracks[0].file.ends_with("disc.flac"));
    }

    #[test]
    fn track_args_seek_stamp_and_skip_source_tags() {
        let settings = Settings::default();
        let slice = TrackSlice {
            start: 65.493,
            end: Some(120.0),
            metadata: vec![("title".into(), "Song".into())],
        };
        let args = build_track_args(
            Path::new("album.ape"),
            Path::new("out.mp3"),
            &settings,
            false,
            Some(44_100),
            &slice,
        );
        let joined = args.join(" ");
        assert!(joined.contains("-ss 65.493"), "{joined}");
        assert!(joined.contains("-t 54.507"), "{joined}");
        assert!(joined.contains("-metadata title=Song"), "{joined}");
        assert!(!joined.contains("-map_metadata"), "{joined}");
    }

    #[test]
    fn splits_a_real_file_by_cue() {
        let Some(ffmpeg) = find_ffmpeg() else { return };
        let dir = cue_test_dir("split");
        let wav = dir.join("album.wav");
        // Six seconds of tone as the "whole-disc" file.
        let ok = std::process::Command::new(&ffmpeg)
            .args([
                "-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi",
                "-i", "sine=frequency=440:duration=6",
            ])
            .arg(&wav)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let cue = "TITLE \"Tones\"\nFILE \"album.wav\" WAVE\n  TRACK 1 AUDIO\n    TITLE \"Part One\"\n    INDEX 01 00:00:00\n  TRACK 2 AUDIO\n    TITLE \"Part Two\"\n    INDEX 01 00:02:00\n  TRACK 3 AUDIO\n    TITLE \"Part Three\"\n    INDEX 01 00:04:00\n";
        std::fs::write(dir.join("album.cue"), cue).unwrap();
        let sheet = parse_cue(&dir.join("album.cue")).unwrap();
        let settings = Settings {
            target: TargetFormat::Flac,
            ..Settings::default()
        };
        let stop = AtomicBool::new(false);
        let src_probe = probe(&ffmpeg, &wav);
        for track in &sheet.tracks {
            let dst = plan_track_output(&wav, &dir, &settings, track);
            convert_track(&ffmpeg, &wav, &dst, &settings, src_probe.as_ref(), &stop, &track.slice())
                .unwrap();
            let info = probe(&ffmpeg, &dst).unwrap();
            assert!(
                (info.duration_secs - 2.0).abs() < 0.3,
                "track {} duration {}",
                track.number,
                info.duration_secs
            );
            // The track title must be stamped on the output.
            let out = std::process::Command::new(&ffmpeg)
                .args(["-hide_banner", "-i"])
                .arg(&dst)
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(stderr.contains("Part"), "metadata missing: {stderr}");
        }
    }

    #[test]
    fn sniffs_the_magics() {
        assert_eq!(sniff(b"fLaC...."), Some(SourceFormat::Flac));
        assert_eq!(sniff(b"ID3\x03\x00"), Some(SourceFormat::Mp3));
        assert_eq!(sniff(&[0xFF, 0xFB, 0x90, 0x00]), Some(SourceFormat::Mp3));
        assert_eq!(sniff(&[0xFF, 0xF1, 0x50, 0x80]), Some(SourceFormat::Aac));
        assert_eq!(sniff(b"RIFF....WAVE"), Some(SourceFormat::Wav));
        assert_eq!(sniff(b"FORM....AIFF"), Some(SourceFormat::Aiff));
        assert_eq!(sniff(b"....ftypM4A "), Some(SourceFormat::Mp4));
        assert_eq!(sniff(b"OggS...."), Some(SourceFormat::Ogg));
        assert_eq!(sniff(b"DSD ...."), Some(SourceFormat::Dsf));
        assert_eq!(sniff(b"FRM8...."), Some(SourceFormat::Dff));
        assert_eq!(sniff(b"MAC ...."), Some(SourceFormat::Ape));
        assert_eq!(sniff(b"MThd...."), Some(SourceFormat::Midi));
        assert_eq!(sniff(&[0x1A, 0x45, 0xDF, 0xA3]), Some(SourceFormat::Matroska));
        assert_eq!(sniff(b"????"), None);
    }

    const MP3_PROBE: &str = "Input #0, mp3, from 'song.mp3':\n  Duration: 00:03:25.12, start: 0.025056, bitrate: 320 kb/s\n    Stream #0:0: Audio: mp3, 44100 Hz, stereo, fltp, 320 kb/s\n    Stream #0:1: Video: mjpeg (attached pic), 500x500 [SAR 1:1 DAR 1:1], 90k tbn\n";

    const FLAC_PROBE: &str = "Input #0, flac, from 'song.flac':\n  Duration: 00:00:10.00, start: 0.000000, bitrate: 705 kb/s\n    Stream #0:0: Audio: flac, 44100 Hz, stereo, s16\n";

    #[test]
    fn parses_probe_output() {
        let p = parse_probe(MP3_PROBE).unwrap();
        assert!((p.duration_secs - 205.12).abs() < 0.001);
        assert_eq!(p.bitrate_kbps, Some(320));
        assert_eq!(p.codec, "mp3");
        assert_eq!(p.sample_rate, Some(44100));
        assert!(p.has_attached_pic);
        let f = parse_probe(FLAC_PROBE).unwrap();
        assert_eq!(f.codec, "flac");
        assert_eq!(f.bitrate_kbps, Some(705));
        assert!(!f.has_attached_pic);
        assert_eq!(format_duration(205.12), "3:25");
        assert_eq!(format_duration(3725.0), "1:02:05");
    }

    #[test]
    fn builds_encoder_args() {
        let mut s = Settings::default();
        let args = build_args(Path::new("in.flac"), Path::new("out.mp3"), &s, false, Some(44100));
        assert!(args.windows(2).any(|w| w == ["-codec:a", "libmp3lame"]));
        assert!(args.windows(2).any(|w| w == ["-b:a", "320k"]));
        assert!(!args.iter().any(|a| a == "-ar"));
        // Lossless target: no bitrate anywhere.
        s.target = TargetFormat::Flac;
        let args = build_args(Path::new("in.mp3"), Path::new("out.flac"), &s, false, Some(44100));
        assert!(args.windows(2).any(|w| w == ["-codec:a", "flac"]));
        assert!(!args.iter().any(|a| a == "-b:a"));
        // Cover picture mapping only when asked.
        let args = build_args(Path::new("in.flac"), Path::new("out.mp3"), &s, true, Some(44100));
        assert!(args.windows(2).any(|w| w == ["-c:v", "copy"]));
        // DSD-rate source into a lossy target clamps to 48 kHz.
        s.target = TargetFormat::Mp3;
        let args = build_args(Path::new("in.dsf"), Path::new("out.mp3"), &s, false, Some(705_600));
        assert!(args.windows(2).any(|w| w == ["-ar", "48000"]));
        // Explicit rate and mono win over everything.
        s.sample_rate = SampleRate::Hz(44_100);
        s.channels = Channels::Mono;
        let args = build_args(Path::new("in.dsf"), Path::new("out.mp3"), &s, false, Some(705_600));
        assert!(args.windows(2).any(|w| w == ["-ar", "44100"]));
        assert!(args.windows(2).any(|w| w == ["-ac", "1"]));
    }

    #[test]
    fn plans_outputs() {
        let mut s = Settings::default();
        let src = Path::new("/music/album/song.flac");
        let root = Path::new("/music");
        assert_eq!(plan_output(src, root, &s), PathBuf::from("/music/album/song.mp3"));
        s.output = OutputMode::Custom(PathBuf::from("/out"));
        assert_eq!(plan_output(src, root, &s), PathBuf::from("/out/album/song.mp3"));
        s.target = TargetFormat::Aac;
        assert_eq!(plan_output(src, root, &s), PathBuf::from("/out/album/song.m4a"));
    }

    #[test]
    fn knows_same_format() {
        assert!(TargetFormat::Mp3.same_as(SourceFormat::Mp3, Some("mp3")));
        assert!(TargetFormat::Flac.same_as(SourceFormat::Flac, Some("flac")));
        assert!(TargetFormat::Aac.same_as(SourceFormat::Mp4, Some("aac")));
        assert!(!TargetFormat::Alac.same_as(SourceFormat::Mp4, Some("aac")));
        assert!(TargetFormat::Opus.same_as(SourceFormat::Ogg, Some("opus")));
        assert!(!TargetFormat::Ogg.same_as(SourceFormat::Ogg, Some("opus")));
        assert!(!TargetFormat::Mp3.same_as(SourceFormat::Unknown, None));
    }

    #[test]
    fn converts_with_real_ffmpeg() {
        let Some(ffmpeg) = find_ffmpeg() else {
            eprintln!("no ffmpeg available, skipping");
            return;
        };
        let dir = std::env::temp_dir().join(format!("audioconvert-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("tone.wav");
        let status = std::process::Command::new(&ffmpeg)
            .args([
                "-hide_banner", "-loglevel", "error", "-y",
                "-f", "lavfi", "-i", "sine=frequency=440:duration=1",
                "-codec:a", "pcm_s16le",
            ])
            .arg(&wav)
            .status()
            .unwrap();
        assert!(status.success());
        let stop = AtomicBool::new(false);
        let mut s = Settings::default();
        // WAV -> MP3
        let mp3 = dir.join("tone.mp3");
        let size = convert(&ffmpeg, &wav, &mp3, &s, None, &stop).unwrap();
        assert!(size > 1_000);
        let p = probe(&ffmpeg, &mp3).unwrap();
        assert_eq!(p.codec, "mp3");
        assert!((p.duration_secs - 1.0).abs() < 0.2);
        // WAV -> FLAC
        s.target = TargetFormat::Flac;
        let flac = dir.join("tone.flac");
        convert(&ffmpeg, &wav, &flac, &s, None, &stop).unwrap();
        assert_eq!(probe(&ffmpeg, &flac).unwrap().codec, "flac");
        // Existing destination is refused, never overwritten.
        let again = convert(&ffmpeg, &wav, &mp3, &Settings::default(), None, &stop);
        assert!(again.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builds_new_format_args() {
        let mut s = Settings::default();
        // DTS needs the experimental opt-in or ffmpeg refuses.
        s.target = TargetFormat::Dts;
        let args = build_args(Path::new("in.flac"), Path::new("out.dts"), &s, false, Some(44100));
        assert!(args.windows(2).any(|w| w == ["-codec:a", "dca"]));
        assert!(args.windows(2).any(|w| w == ["-strict", "-2"]));
        // WavPack is lossless: no bitrate.
        s.target = TargetFormat::WavPack;
        let args = build_args(Path::new("in.flac"), Path::new("out.wv"), &s, false, Some(44100));
        assert!(args.windows(2).any(|w| w == ["-codec:a", "wavpack"]));
        assert!(!args.iter().any(|a| a == "-b:a"));
        // GSM forces 8 kHz mono over the user's stereo / 44.1 kHz.
        s.target = TargetFormat::Gsm;
        s.sample_rate = SampleRate::Hz(44_100);
        s.channels = Channels::Stereo;
        let args = build_args(Path::new("in.wav"), Path::new("out.gsm"), &s, false, Some(44100));
        assert!(args.windows(2).any(|w| w == ["-ar", "8000"]));
        assert!(args.windows(2).any(|w| w == ["-ac", "1"]));
        assert!(!args.iter().any(|a| a == "-b:a"));
        // aptX rejects a 96 kHz source rate and falls back to 48k.
        s = Settings::default();
        s.target = TargetFormat::Aptx;
        let args = build_args(Path::new("in.flac"), Path::new("out.aptx"), &s, false, Some(96000));
        assert!(args.windows(2).any(|w| w == ["-ar", "48000"]));
        // Speex tops out at 32 kHz.
        s.target = TargetFormat::Speex;
        let args = build_args(Path::new("in.wav"), Path::new("out.spx"), &s, false, Some(44100));
        assert!(args.windows(2).any(|w| w == ["-ar", "32000"]));
        // Same-format recognition for the codec-only targets.
        assert!(TargetFormat::WavPack.same_as(SourceFormat::Unknown, Some("wavpack")));
        assert!(TargetFormat::Dts.same_as(SourceFormat::Unknown, Some("dts")));
        assert!(!TargetFormat::Ac3.same_as(SourceFormat::Unknown, Some("eac3")));
        assert!(TargetFormat::Gsm.same_as(SourceFormat::Unknown, Some("gsm_ms")));
    }

    #[test]
    fn honours_keep_tags() {
        // On: metadata mapped from the source, picture copied.
        let s = Settings::default();
        assert!(s.keep_tags);
        let args = build_args(Path::new("in.flac"), Path::new("out.mp3"), &s, true, Some(44100));
        assert!(args.windows(2).any(|w| w == ["-map_metadata", "0"]));
        assert!(args.windows(2).any(|w| w == ["-c:v", "copy"]));
        // Off: explicit strip, and no picture even when offered.
        let mut s = Settings::default();
        s.keep_tags = false;
        let args = build_args(Path::new("in.flac"), Path::new("out.mp3"), &s, true, Some(44100));
        assert!(args.windows(2).any(|w| w == ["-map_metadata", "-1"]));
        assert!(!args.iter().any(|a| a == "-c:v"));
        // CUE tracks: the stamped tags are gated by the same flag.
        let slice = TrackSlice {
            start: 0.0,
            end: Some(2.0),
            metadata: vec![("title".to_string(), "Song".to_string())],
        };
        let args = build_track_args(Path::new("in.ape"), Path::new("out.mp3"), &s, true, Some(44100), &slice);
        assert!(!args.iter().any(|a| a == "-metadata"));
        assert!(!args.iter().any(|a| a == "-c:v"));
        s.keep_tags = true;
        let args = build_track_args(Path::new("in.ape"), Path::new("out.mp3"), &s, true, Some(44100), &slice);
        assert!(args.windows(2).any(|w| w == ["-metadata", "title=Song"]));
    }

    #[test]
    fn strips_tags_with_real_ffmpeg() {
        let Some(ffmpeg) = find_ffmpeg() else {
            eprintln!("no ffmpeg available, skipping");
            return;
        };
        let dir = std::env::temp_dir().join(format!("audioconvert-tag-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mp3 = dir.join("tagged.mp3");
        let status = std::process::Command::new(&ffmpeg)
            .args([
                "-hide_banner", "-loglevel", "error", "-y",
                "-f", "lavfi", "-i", "sine=frequency=440:duration=1",
                "-codec:a", "libmp3lame",
                "-metadata", "title=TagTestTitle",
                "-metadata", "artist=TagTestSinger",
            ])
            .arg(&mp3)
            .status()
            .unwrap();
        assert!(status.success());
        let stop = AtomicBool::new(false);
        let report = |p: &Path| -> String {
            let out = std::process::Command::new(&ffmpeg)
                .args(["-hide_banner", "-i"])
                .arg(p)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stderr).into_owned()
        };
        // Kept: the tags ride along into the FLAC.
        let mut s = Settings::default();
        s.target = TargetFormat::Flac;
        let kept = dir.join("kept.flac");
        convert(&ffmpeg, &mp3, &kept, &s, None, &stop).unwrap();
        assert!(report(&kept).contains("TagTestTitle"));
        // Stripped: nothing survives.
        s.keep_tags = false;
        let bare = dir.join("bare.flac");
        convert(&ffmpeg, &mp3, &bare, &s, None, &stop).unwrap();
        let text = report(&bare);
        assert!(!text.contains("TagTestTitle"), "title leaked: {text}");
        assert!(!text.contains("TagTestSinger"), "artist leaked: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn converts_to_new_formats_with_real_ffmpeg() {
        let Some(ffmpeg) = find_ffmpeg() else {
            eprintln!("no ffmpeg available, skipping");
            return;
        };
        let dir = std::env::temp_dir().join(format!("audioconvert-fmt-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("tone.wav");
        let status = std::process::Command::new(&ffmpeg)
            .args([
                "-hide_banner", "-loglevel", "error", "-y",
                "-f", "lavfi", "-i", "sine=frequency=440:duration=1",
                "-ar", "44100", "-ac", "2", "-codec:a", "pcm_s16le",
            ])
            .arg(&wav)
            .status()
            .unwrap();
        assert!(status.success());
        let stop = AtomicBool::new(false);
        let cases: [(TargetFormat, &str); 11] = [
            (TargetFormat::WavPack, "wavpack"),
            (TargetFormat::Tta, "tta"),
            (TargetFormat::Mp2, "mp2"),
            (TargetFormat::Ac3, "ac3"),
            (TargetFormat::Eac3, "eac3"),
            (TargetFormat::Dts, "dts"),
            (TargetFormat::Speex, "speex"),
            (TargetFormat::Aptx, "aptx"),
            (TargetFormat::AptxHd, "aptx_hd"),
            (TargetFormat::Sbc, "sbc"),
            (TargetFormat::Gsm, "gsm"),
        ];
        for (target, codec) in cases {
            let mut s = Settings::default();
            s.target = target;
            let out = dir.join(format!("tone.{}", target.extension()));
            convert(&ffmpeg, &wav, &out, &s, None, &stop)
                .unwrap_or_else(|e| panic!("{target:?} conversion failed: {e:?}"));
            match probe(&ffmpeg, &out) {
                Some(p) => assert_eq!(p.codec, codec, "wrong codec for {target:?}"),
                None => {
                    // Raw aptX / SBC streams carry no duration,
                    // so probe() bows out (ffmpeg itself reports
                    // "Duration: N/A"); fall back to the codec
                    // name in ffmpeg's own report.
                    let outp = std::process::Command::new(&ffmpeg)
                        .args(["-hide_banner", "-i"])
                        .arg(&out)
                        .output()
                        .unwrap();
                    let text = String::from_utf8_lossy(&outp.stderr);
                    assert!(
                        text.contains(&format!("Audio: {codec}")),
                        "no codec line for {target:?}"
                    );
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
