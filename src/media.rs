//! Sound and video through ffmpeg: a recording (or a video's soundtrack) cut into overlapping windows for the model,
//! silent ones skipped, and a video's frames, one per shot.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::image;
use crate::tools::{self, ExtractError, Result};

pub const SAMPLE_RATE: f64 = 16_000.0;

/// A stretch of audio written to a temporary WAV for the model, or a video frame written to a JPEG.
#[derive(Clone, Debug)]
pub struct Piece {
    pub path: String,
    pub start: f64,
    pub end: f64,
}

/// What ffprobe says about a media file.
pub struct Probe {
    pub duration: f64,
    pub has_audio: bool,
    pub has_video: bool,
}

impl Probe {
    pub fn of(path: &Path) -> Result<Self> {
        #[derive(Deserialize)]
        struct Format {
            duration: Option<String>,
        }
        #[derive(Deserialize)]
        struct Stream {
            codec_type: Option<String>,
            disposition: Option<std::collections::HashMap<String, i64>>,
        }
        #[derive(Deserialize)]
        struct Info {
            format: Option<Format>,
            streams: Option<Vec<Stream>>,
        }
        let probe = tools::run(
            "ffprobe",
            &["-v", "error", "-show_entries", "format=duration:stream=codec_type:stream_disposition=attached_pic",
              "-of", "json", &path.to_string_lossy()],
        )?;
        let info: Info = probe
            .ok()
            .then(|| serde_json::from_slice(&probe.stdout).ok())
            .flatten()
            .ok_or_else(|| ExtractError::Unreadable(format!("can't read media: {}", probe.complaint())))?;
        let duration = info
            .format
            .and_then(|format| format.duration)
            .and_then(|duration| duration.parse::<f64>().ok())
            .filter(|duration| duration.is_finite() && *duration > 0.0)
            .unwrap_or(0.0);
        let streams = info.streams.unwrap_or_default();
        Ok(Self {
            duration,
            has_audio: streams.iter().any(|stream| stream.codec_type.as_deref() == Some("audio")),
            // A cover picture in a music file is a "video" stream too.
            has_video: streams.iter().any(|stream| {
                stream.codec_type.as_deref() == Some("video")
                    && stream.disposition.as_ref().and_then(|d| d.get("attached_pic")) != Some(&1)
            }),
        })
    }
}

/// A file's duration in seconds.
pub fn duration(path: &Path) -> Option<f64> {
    Probe::of(path).ok().map(|probe| probe.duration).filter(|duration| *duration > 0.0)
}

/// Cuts 16 kHz mono samples, fed as they're decoded, into `window`-second WAV pieces `hop` seconds apart, skipping
/// silent ones. One model input ignores anything past 30 s, so windows stay ≤ 30 s. Memory stays at one window, even
/// for a 3-hour podcast.
struct Windows {
    window: usize,
    hop: usize,
    minimum: usize,
    folder: PathBuf,
    buffer: Vec<i16>,
    /// Sample index of buffer[0] in the file.
    offset: usize,
    pieces: Vec<Piece>,
}

impl Windows {
    fn append(&mut self, samples: &[i16]) -> std::io::Result<()> {
        self.buffer.extend_from_slice(samples);
        while self.buffer.len() >= self.window {
            self.emit(self.window)?;
            self.buffer.drain(..self.hop);
            self.offset += self.hop;
        }
        Ok(())
    }

    /// The tail: new audio beyond the previous window's overlap, or the whole file if it was shorter than a window.
    fn finish(mut self) -> std::io::Result<(Vec<Piece>, f64)> {
        let covered = if self.offset == 0 { 0 } else { self.window - self.hop };
        if self.buffer.len().saturating_sub(covered) >= self.minimum && self.buffer.len() > covered {
            self.emit(self.buffer.len())?;
        }
        let seconds = (self.offset + self.buffer.len()) as f64 / SAMPLE_RATE;
        Ok((self.pieces, seconds))
    }

    fn emit(&mut self, count: usize) -> std::io::Result<()> {
        let slice = &self.buffer[..count];
        if rms(slice) <= 0.003 {
            return Ok(()); // ≈ −50 dBFS: silence isn't worth a vector
        }
        let out = self.folder.join(format!("{}.wav", uuid::Uuid::new_v4()));
        write_wav(slice, &out)?;
        self.pieces.push(Piece {
            path: out.to_string_lossy().into_owned(),
            start: self.offset as f64 / SAMPLE_RATE,
            end: (self.offset + count) as f64 / SAMPLE_RATE,
        });
        Ok(())
    }
}

fn rms(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|sample| (*sample as f64 / 32768.0).powi(2)).sum();
    (sum / samples.len() as f64).sqrt()
}

fn write_wav(samples: &[i16], path: &Path) -> std::io::Result<()> {
    let rate = SAMPLE_RATE as u32;
    let bytes = (samples.len() * 2) as u32;
    let mut data = Vec::with_capacity(44 + samples.len() * 2);
    data.extend_from_slice(b"RIFF");
    data.extend_from_slice(&(36 + bytes).to_le_bytes());
    data.extend_from_slice(b"WAVEfmt ");
    data.extend_from_slice(&16u32.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes()); // PCM
    data.extend_from_slice(&1u16.to_le_bytes()); // mono
    data.extend_from_slice(&rate.to_le_bytes());
    data.extend_from_slice(&(rate * 2).to_le_bytes());
    data.extend_from_slice(&2u16.to_le_bytes());
    data.extend_from_slice(&16u16.to_le_bytes());
    data.extend_from_slice(b"data");
    data.extend_from_slice(&bytes.to_le_bytes());
    for sample in samples {
        data.extend_from_slice(&sample.to_le_bytes());
    }
    fs::File::create(path)?.write_all(&data)
}

/// Decodes the sound of `path` (a recording, or a video's soundtrack) to 16 kHz mono with ffmpeg and cuts it into
/// windows, reading ffmpeg's output as it comes. Returns the pieces and the duration.
pub fn audio_windows(path: &Path, window: f64, hop: f64, folder: &Path) -> Result<(Vec<Piece>, f64)> {
    let probe = Probe::of(path)?;
    if !probe.has_audio {
        return Ok((Vec::new(), 0.0));
    }
    let ffmpeg = tools::path("ffmpeg").ok_or_else(|| ExtractError::Unreadable("needs ffmpeg, which isn't installed".into()))?;
    let mut child = Command::new(ffmpeg)
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-vn", "-ac", "1", "-ar", "16000", "-f", "s16le", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| ExtractError::Unreadable(error.to_string()))?;
    let mut windows = Windows {
        window: (window * SAMPLE_RATE) as usize,
        hop: (hop * SAMPLE_RATE) as usize,
        minimum: (2.0 * SAMPLE_RATE) as usize,
        folder: folder.to_path_buf(),
        buffer: Vec::new(),
        offset: 0,
        pieces: Vec::new(),
    };
    let mut output = child.stdout.take().expect("piped");
    let mut chunk = vec![0u8; 1 << 16];
    let mut odd: Option<u8> = None; // half a sample left from the last read
    let failed = |error: std::io::Error| ExtractError::Unreadable(error.to_string());
    loop {
        let read = output.read(&mut chunk).map_err(failed)?;
        if read == 0 {
            break;
        }
        let mut bytes = Vec::with_capacity(read + 1);
        bytes.extend(odd.take());
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() % 2 == 1 {
            odd = bytes.pop();
        }
        let samples: Vec<i16> = bytes.as_chunks::<2>().0.iter().map(|pair| i16::from_le_bytes(*pair)).collect();
        if let Err(error) = windows.append(&samples) {
            let _ = child.kill();
            return Err(failed(error));
        }
    }
    let status = child.wait().map_err(failed)?;
    let (pieces, seconds) = windows.finish().map_err(failed)?;
    if !status.success() && pieces.is_empty() {
        return Err(ExtractError::Unreadable("audio decoding failed".into()));
    }
    Ok((pieces, if probe.duration > 0.0 { probe.duration } else { seconds }))
}

/// Whether a frame with `hash` looks like the last kept one (8×8 average hash within 4 bits), and so only extends it.
pub fn repeats(hash: u64, last: Option<u64>) -> bool {
    last.is_some_and(|last| (hash ^ last).count_ones() <= 4)
}

/// The average hash of an 8×8 grayscale thumbnail: a bit per pixel brighter than the mean.
pub fn average_hash(pixels: &[u8]) -> u64 {
    if pixels.is_empty() {
        return 0;
    }
    let mean = pixels.iter().map(|p| *p as usize).sum::<usize>() / pixels.len();
    pixels.iter().take(64).enumerate().filter(|(_, p)| **p as usize > mean).fold(0, |hash, (index, _)| hash | 1 << index)
}

pub struct Keyframes {
    pub frames: Vec<Piece>,
    pub duration: f64,
    pub has_audio: bool,
}

/// Samples a frame every `interval` seconds and keeps one per shot: a frame that looks like the previous kept one only
/// extends it, so screen recordings and lectures shrink a lot. One ffmpeg run writes each frame as a JPEG and, beside
/// it, as an 8×8 RGB thumbnail for the hash and color comparison; the frames that repeat the previous kept one are deleted again.
pub fn keyframes(path: &Path, interval: f64, max_pixel: u32, max_frames: usize, folder: &Path) -> Result<Keyframes> {
    let probe = Probe::of(path)?;
    let duration = probe.duration;
    if duration <= 0.0 || !probe.has_video {
        return Ok(Keyframes { frames: Vec::new(), duration, has_audio: probe.has_audio });
    }
    let step = interval.max(duration / max_frames as f64);
    let start = 0.5f64.min(duration / 2.0);
    let prefix = uuid::Uuid::new_v4().to_string();
    let pattern = folder.join(format!("{prefix}-%06d.jpg"));
    let graph = format!(
        "[0:v]fps=fps={},split=2[big][small];[big]{}[frames];[small]scale=8:8:flags=area,format=rgb24[hashes]",
        1.0 / step,
        image::fit(max_pixel)
    );
    let made = tools::run(
        "ffmpeg",
        &["-nostdin", "-v", "error", "-ss", &start.to_string(), "-i", &path.to_string_lossy(), "-filter_complex",
          &graph, "-map", "[frames]", "-q:v", "3", &pattern.to_string_lossy(), "-map", "[hashes]", "-f", "rawvideo",
          "pipe:1"],
    )?;
    let mut frames: Vec<Piece> = Vec::new();
    let mut last = None;
    let mut last_color: Option<[i32; 3]> = None;
    for (index, pixels) in made.stdout.as_chunks::<192>().0.iter().enumerate() {
        let frame = folder.join(format!("{prefix}-{:06}.jpg", index + 1));
        if !frame.exists() {
            break;
        }
        let gray: Vec<u8> = pixels.as_chunks::<3>().0.iter().map(|rgb|
            ((rgb[0] as u32 * 299 + rgb[1] as u32 * 587 + rgb[2] as u32 * 114) / 1000) as u8).collect();
        let color: [i32; 3] = std::array::from_fn(|channel|
            pixels.as_chunks::<3>().0.iter().map(|rgb| rgb[channel] as i32).sum::<i32>() / 64);
        let hash = average_hash(&gray);
        let similar_color = last_color.map(|previous|
            (0..3).all(|channel| (color[channel] - previous[channel]).abs() < 16)).unwrap_or(false);
        if repeats(hash, last) && similar_color {
            let _ = fs::remove_file(&frame);
            continue;
        }
        last = Some(hash);
        last_color = Some(color);
        frames.push(Piece { path: frame.to_string_lossy().into_owned(), start: start + index as f64 * step, end: duration });
    }
    if frames.is_empty() && !made.ok() {
        return Err(ExtractError::Unreadable(format!("can't read video: {}", made.complaint())));
    }
    // Each kept frame stands for the stretch until the next kept frame.
    for index in 0..frames.len().saturating_sub(1) {
        frames[index].end = frames[index + 1].start;
    }
    Ok(Keyframes { frames, duration, has_audio: probe.has_audio })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_cuts_sound_and_keeps_one_frame_per_shot() {
        if tools::path("ffmpeg").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("clip.mp4");
        let made = tools::run(
            "ffmpeg",
            &["-v", "error", "-f", "lavfi", "-i", "color=red:size=320x240:duration=6", "-f", "lavfi", "-i",
              "color=blue:size=320x240:duration=6", "-f", "lavfi", "-i", "sine=frequency=440:duration=12",
              "-filter_complex", "[0:v][1:v]concat=n=2:v=1[v]", "-map", "[v]", "-map", "2:a", "-c:v", "libx264",
              "-pix_fmt", "yuv420p", "-c:a", "aac", &clip.to_string_lossy()],
        )
        .unwrap();
        assert!(made.ok());
        let video = keyframes(&clip, 3.0, 1024, 1200, dir.path()).unwrap();
        assert!((video.duration - 12.0).abs() < 0.2);
        assert!(video.has_audio);
        assert_eq!(video.frames.len(), 2); // one per colour
        assert_eq!(video.frames[0].start, 0.5);
        assert!((video.frames[1].start - 6.5).abs() < 0.01);
        let (pieces, _) = audio_windows(&clip, 5.0, 4.0, dir.path()).unwrap();
        assert_eq!(pieces.iter().map(|p| p.start).collect::<Vec<_>>(), [0.0, 4.0, 8.0]);
    }
}
