use anyhow::{anyhow, Result};
use std::io::{BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::{fs::File, sync::OnceLock};
use symphonia::core::{
    codecs::audio::well_known::{CODEC_ID_AC3, CODEC_ID_EAC3},
    codecs::{
        audio::{AudioDecoder as SymphDecoder, AudioDecoderOptions},
        registry::CodecRegistry,
        CodecParameters,
    },
    errors::Error as SymphError,
    formats::{probe::Hint, FormatOptions, FormatReader, SeekMode, SeekTo, TrackType},
    io::MediaSourceStream,
    meta::MetadataOptions,
    units::Time,
};
use symphonia_adapter_libopus::OpusDecoder;

enum Inner {
    Symphonia {
        format: Box<dyn FormatReader>,
        decoder: Box<dyn SymphDecoder>,
        track_id: u32,
    },
    Ffmpeg(FfmpegPcm),
}

struct FfmpegPcm {
    child: Child,
    stdout: BufReader<std::process::ChildStdout>,
    pending: Vec<u8>,
    path: String,
}

pub struct Decoder {
    inner: Inner,
    sample_rate: u32,
    channels: u32,
    bit_depth: u32,
    duration_secs: f64,
    position_secs: f64,
    logged_layout: bool,
}

impl Decoder {
    pub fn open(path: &str, gapless: bool) -> Result<Self> {
        let file = File::open(path)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());

        let mut hint = Hint::new();
        if let Some(ext) = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
        {
            hint.with_extension(ext);
        }

        let format = symphonia::default::get_probe().probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )?;

        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| anyhow!("No default audio track found"))?;

        let track_id = track.id;

        let CodecParameters::Audio(params) = track
            .codec_params
            .as_ref()
            .ok_or_else(|| anyhow!("No codec parameters"))?
        else {
            return Err(anyhow!("Track is not an audio track"));
        };

        let sample_rate = params.sample_rate.unwrap_or(44100);
        let channels = params
            .channels
            .as_ref()
            .map(|c| c.count() as u32)
            .unwrap_or(2);
        let bit_depth = params.bits_per_sample.unwrap_or(16);

        let n_frames = track.num_frames.unwrap_or(0);
        let duration_secs = if sample_rate > 0 {
            n_frames as f64 / sample_rate as f64
        } else {
            0.0
        };

        let mut dec_opts = AudioDecoderOptions::default();
        dec_opts.gapless = gapless;

        static CODEC_REGISTRY: OnceLock<CodecRegistry> = OnceLock::new();

        let codec_registry = CODEC_REGISTRY.get_or_init(|| {
            let mut registry = CodecRegistry::new();
            symphonia::default::register_enabled_codecs(&mut registry);
            registry.register_audio_decoder::<OpusDecoder>();
            registry
        });
        let codec = params.codec;
        let made = codec_registry.make_audio_decoder(&params, &dec_opts);
        let decoder = match made {
            Ok(decoder) => decoder,
            Err(SymphError::Unsupported(_)) if codec == CODEC_ID_EAC3 || codec == CODEC_ID_AC3 => {
                drop(format);
                return open_ffmpeg(path, sample_rate.max(1), duration_secs);
            }
            Err(e) => {
                crate::logger::error_audio(format!("decoder open failed: {e}"));
                return Err(e.into());
            }
        };

        Ok(Self {
            inner: Inner::Symphonia {
                format,
                decoder,
                track_id,
            },
            sample_rate,
            channels,
            bit_depth,
            duration_secs,
            position_secs: 0.0,
            logged_layout: false,
        })
    }

    pub fn next_packet_into(&mut self, buf: &mut Vec<f32>) -> Result<bool> {
        let channels = self.channels.max(1);
        let rate = self.sample_rate;
        let pulled = match &mut self.inner {
            Inner::Symphonia {
                format,
                decoder,
                track_id,
            } => pull_symphonia(format.as_mut(), decoder.as_mut(), *track_id, buf)?,
            Inner::Ffmpeg(ff) => pull_ffmpeg(ff, channels, rate, buf)?,
        };
        let Some(pulled) = pulled else {
            return Ok(false);
        };
        if matches!(self.inner, Inner::Symphonia { .. })
            && (pulled.ch != self.channels || pulled.rate != self.sample_rate)
            && !self.logged_layout
        {
            self.logged_layout = true;
            crate::logger::warn_audio(format!(
                "packet layout {}Hz {}ch ({} frames, {} samples) differs from stream {}Hz {}ch spec_ch={}",
                pulled.rate,
                pulled.ch,
                pulled.frames,
                pulled.samples,
                self.sample_rate,
                self.channels,
                pulled.spec_ch
            ));
        }
        self.sample_rate = pulled.rate;
        self.channels = pulled.ch;
        self.position_secs += pulled.frames as f64 / pulled.rate.max(1) as f64;
        Ok(true)
    }

    pub fn seek(&mut self, position_secs: f64) -> Result<()> {
        let position_secs = position_secs.max(0.0);
        match &mut self.inner {
            Inner::Symphonia {
                format,
                decoder,
                track_id,
            } => {
                let time = Time::try_from_secs_f64(position_secs)
                    .ok_or_else(|| anyhow!("invalid seek target: {position_secs}"))?;
                format.seek(
                    SeekMode::Accurate,
                    SeekTo::Time {
                        time,
                        track_id: Some(*track_id),
                    },
                )?;
                decoder.reset();
            }
            Inner::Ffmpeg(ff) => {
                stop_ffmpeg(ff);
                let (child, stdout, pending) =
                    spawn_ffmpeg(&ff.path, self.sample_rate.max(1), position_secs, false)?;
                ff.child = child;
                ff.stdout = stdout;
                ff.pending = pending;
            }
        }
        self.position_secs = position_secs;
        Ok(())
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    pub fn channels(&self) -> u32 {
        self.channels
    }
    pub fn bit_depth(&self) -> u32 {
        self.bit_depth
    }
    pub fn duration_secs(&self) -> f64 {
        self.duration_secs
    }
    pub fn position_secs(&self) -> f64 {
        self.position_secs
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        if let Inner::Ffmpeg(ff) = &mut self.inner {
            stop_ffmpeg(ff);
        }
    }
}

struct Pulled {
    ch: u32,
    rate: u32,
    frames: usize,
    samples: usize,
    spec_ch: usize,
}

fn pull_symphonia(
    format: &mut dyn FormatReader,
    decoder: &mut dyn SymphDecoder,
    track_id: u32,
    buf: &mut Vec<f32>,
) -> Result<Option<Pulled>> {
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => return Ok(None),
            Err(SymphError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(None)
            }
            Err(SymphError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(e) => return Err(e.into()),
        };

        if packet.track_id != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymphError::DecodeError(_)) => continue,
            Err(e) => return Err(e.into()),
        };

        let spec = decoded.spec();
        let decoded_frames = decoded.frames();
        if decoded_frames == 0 {
            continue;
        }

        buf.clear();
        decoded.copy_to_vec_interleaved(buf);

        let spec_ch = spec.channels().count().max(1);
        let pcm_ch = (buf.len() / decoded_frames).max(1);
        let ch = if buf.len() == decoded_frames * spec_ch {
            spec_ch as u32
        } else {
            pcm_ch as u32
        };
        buf.truncate(decoded_frames * ch as usize);
        let rate = spec.rate().max(1);
        return Ok(Some(Pulled {
            ch,
            rate,
            frames: decoded_frames,
            samples: buf.len(),
            spec_ch,
        }));
    }
}

fn pull_ffmpeg(
    ff: &mut FfmpegPcm,
    channels: u32,
    rate: u32,
    buf: &mut Vec<f32>,
) -> Result<Option<Pulled>> {
    let channels = channels.max(1) as usize;
    let need = 1024 * channels * 4;
    while ff.pending.len() < need {
        let mut chunk = [0u8; 8192];
        match ff.stdout.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => ff.pending.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    let samples = ff.pending.len() / 4;
    let usable = samples - (samples % channels);
    if usable == 0 {
        return Ok(None);
    }
    let bytes = usable * 4;
    buf.clear();
    buf.extend(
        ff.pending[..bytes]
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])),
    );
    ff.pending.drain(..bytes);
    Ok(Some(Pulled {
        ch: channels as u32,
        rate: rate.max(1),
        frames: usable / channels,
        samples: buf.len(),
        spec_ch: channels,
    }))
}

fn open_ffmpeg(path: &str, sample_rate: u32, duration_secs: f64) -> Result<Decoder> {
    let rate = sample_rate.max(1);
    let (child, stdout, pending) = match spawn_ffmpeg(path, rate, 0.0, true) {
        Ok(opened) => opened,
        Err(e) => {
            crate::logger::error_audio(format!("ffmpeg decode failed: {e}"));
            return Err(e);
        }
    };
    crate::logger::info_audio(format!(
        "ffmpeg decode {rate}Hz stereo ({})",
        ffmpeg_program()
    ));
    Ok(Decoder {
        inner: Inner::Ffmpeg(FfmpegPcm {
            child,
            stdout,
            pending,
            path: path.to_owned(),
        }),
        sample_rate: rate,
        channels: 2,
        bit_depth: 32,
        duration_secs,
        position_secs: 0.0,
        logged_layout: false,
    })
}

fn ffmpeg_program() -> String {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let name = if cfg!(windows) {
                "ffmpeg.exe"
            } else {
                "ffmpeg"
            };
            let bundled = dir.join(name);
            if bundled.is_file() {
                return bundled.to_string_lossy().into_owned();
            }
        }
    }
    "ffmpeg".to_owned()
}

fn spawn_ffmpeg(
    path: &str,
    sample_rate: u32,
    start_secs: f64,
    check_audio: bool,
) -> Result<(Child, BufReader<std::process::ChildStdout>, Vec<u8>)> {
    let program = ffmpeg_program();
    let mut cmd = Command::new(&program);
    cmd.arg("-nostdin").arg("-v").arg("error");
    if start_secs > 0.0 {
        cmd.arg("-ss").arg(format!("{start_secs:.3}"));
    }
    cmd.arg("-i")
        .arg(path)
        .arg("-map")
        .arg("0:a:0")
        .arg("-f")
        .arg("f32le")
        .arg("-acodec")
        .arg("pcm_f32le")
        .arg("-ac")
        .arg("2")
        .arg("-ar")
        .arg(sample_rate.to_string())
        .arg("pipe:1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow!("unsupported audio codec (ffmpeg not installed)")
        } else {
            anyhow!("ffmpeg: {e}")
        }
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("ffmpeg: no stdout"))?;
    let mut stdout = BufReader::new(stdout);
    let mut pending = Vec::new();
    if check_audio {
        let mut probe = [0u8; 64];
        let mut filled = 0;
        while filled < probe.len() {
            match stdout.read(&mut probe[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        if filled == 0 {
            let _ = child.kill();
            let status = child.wait();
            return Err(anyhow!("ffmpeg produced no audio ({status:?})"));
        }
        pending.extend_from_slice(&probe[..filled]);
    }
    Ok((child, stdout, pending))
}

fn stop_ffmpeg(ff: &mut FfmpegPcm) {
    let _ = ff.child.kill();
    let _ = ff.child.wait();
}

#[cfg(test)]
mod decode_tests {
    use super::*;
    use std::process::Command;

    fn ffmpeg_ok() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn synth(path: &std::path::Path, extra: &[&str]) {
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=0.4",
            ])
            .args(extra)
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("ffmpeg");
        assert!(status.success());
    }

    fn read_some(path: &std::path::Path) -> Decoder {
        let mut dec = Decoder::open(path.to_str().unwrap(), false).unwrap();
        let mut buf = Vec::new();
        let mut n = 0usize;
        while dec.next_packet_into(&mut buf).unwrap() {
            assert!(buf.iter().any(|s| s.abs() > 0.001));
            n += buf.len();
            if n > 4000 {
                break;
            }
        }
        assert!(n > 1000, "decoded {n} samples");
        dec
    }

    #[test]
    fn aac_m4a_uses_symphonia() {
        if !ffmpeg_ok() {
            return;
        }
        let path = std::env::temp_dir().join("aqloss-aac-probe.m4a");
        synth(&path, &["-c:a", "aac"]);
        let dec = read_some(&path);
        assert!(matches!(dec.inner, Inner::Symphonia { .. }));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn eac3_m4a_uses_ffmpeg() {
        if !ffmpeg_ok() {
            return;
        }
        let path = std::env::temp_dir().join("aqloss-eac3-probe.m4a");
        synth(&path, &["-c:a", "eac3", "-ac", "6", "-f", "mp4"]);
        let mut dec = read_some(&path);
        assert!(matches!(dec.inner, Inner::Ffmpeg(_)));
        assert_eq!(dec.channels(), 2);
        assert!(dec.duration_secs() > 0.3);
        dec.seek(0.2).unwrap();
        let mut buf = Vec::new();
        assert!(dec.next_packet_into(&mut buf).unwrap());
        assert!(!buf.is_empty());
        let _ = std::fs::remove_file(path);
    }
}
