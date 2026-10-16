use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaType {
    Image,
    Audio,
}

impl MediaType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Audio => "audio",
        }
    }
}

pub struct MediaAnalysis {
    pub media_type: MediaType,
    pub metadata: BTreeMap<String, Value>,
    pub embedding: Vec<f32>,
}

pub fn analyze(path: &Path) -> Result<MediaAnalysis, String> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if let Ok(format) = image::ImageFormat::from_path(path) {
        let decoded = image::open(path)
            .map_err(|e| format!("cannot decode image {}: {e}", path.display()))?;
        let width = decoded.width();
        let height = decoded.height();
        let resized = decoded
            .resize_exact(128, 128, image::imageops::FilterType::Triangle)
            .to_rgb8();
        let embedding = image_embedding(&resized);
        let mut metadata = BTreeMap::new();
        metadata.insert("format".to_owned(), json!(format!("{format:?}").to_lowercase()));
        metadata.insert("width".to_owned(), json!(width));
        metadata.insert("height".to_owned(), json!(height));
        metadata.insert("channels".to_owned(), json!(3));
        return Ok(MediaAnalysis {
            media_type: MediaType::Image,
            metadata,
            embedding,
        });
    }

    if is_audio_extension(&extension) {
        let (embedding, duration, sample_rate, channels) = audio_embedding(path, &extension)?;
        let mut metadata = BTreeMap::new();
        metadata.insert("format".to_owned(), json!(extension));
        metadata.insert("duration_seconds".to_owned(), json!(duration));
        metadata.insert("sample_rate".to_owned(), json!(sample_rate));
        metadata.insert("channels".to_owned(), json!(channels));
        return Ok(MediaAnalysis {
            media_type: MediaType::Audio,
            metadata,
            embedding,
        });
    }

    Err(format!(
        "unsupported media file {}. Supported inputs are image files and audio files (WAV, MP3, FLAC, OGG, Opus, AAC, M4A, AIFF). Video is not supported.",
        path.display()
    ))
}

fn image_embedding(image: &image::RgbImage) -> Vec<f32> {
    // 8-bin histograms for each RGB channel plus an 8-bin luminance histogram.
    let mut histogram = [0_u32; 32];
    let pixels = (u64::from(image.width()) * u64::from(image.height())).max(1);
    for pixel in image.pixels() {
        for channel in 0..3 {
            histogram[channel * 8 + (pixel[channel] as usize / 32)] += 1;
        }
        let luminance = (u16::from(pixel[0]) * 3
            + u16::from(pixel[1]) * 6
            + u16::from(pixel[2]))
            / 10;
        histogram[24 + (luminance as usize / 32)] += 1;
    }
    histogram
        .iter()
        .map(|count| *count as f32 / pixels as f32)
        .collect()
}

fn is_audio_extension(extension: &str) -> bool {
    matches!(
        extension,
        "wav" | "wave" | "mp3" | "flac" | "ogg" | "opus" | "aac" | "m4a" | "aiff" | "aif"
    )
}

fn audio_embedding(path: &Path, extension: &str) -> Result<(Vec<f32>, f64, u32, u16), String> {
    let file = File::open(path).map_err(|e| format!("cannot open audio {}: {e}", path.display()))?;
    let mut hint = Hint::new();
    hint.with_extension(extension);
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("cannot inspect audio {}: {e}", path.display()))?;
    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| format!("audio file has no default track: {}", path.display()))?;
    let track_id = track.id;
    let sample_rate = track.codec_params.sample_rate.unwrap_or(0);
    if sample_rate == 0 {
        return Err(format!("audio track has no sample rate: {}", path.display()));
    }
    let channels = track
        .codec_params
        .channels
        .map(|channel_count| channel_count.count() as u16)
        .unwrap_or(1)
        .max(1);
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("cannot initialize audio decoder: {e}"))?;

    let mut mono_samples: Vec<(u64, f32)> = Vec::new();
    let mut frame_count = 0_u64;
    let mut sampled_frames = 0_u64;
    let mut random_state = 0x5d58_8b65_2d31_9a07_u64;
    let stride = (sample_rate as usize / 4_000).max(1);
    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break
            }
            Err(SymphoniaError::IoError(error)) => {
                return Err(format!("cannot read audio file: {error}"));
            }
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(error) => return Err(format!("cannot read audio packet: {error}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(error) => return Err(format!("cannot decode audio packet: {error}")),
        };
        let frame_base = frame_count;
        frame_count = frame_count.saturating_add(decoded.frames() as u64);
        let mut samples = SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
        samples.copy_interleaved_ref(decoded);
        let channel_count = channels as usize;
        for (frame, interleaved) in samples.samples().chunks(channel_count).enumerate() {
            let absolute_frame = frame_base.saturating_add(frame as u64);
            if absolute_frame % stride as u64 == 0 {
                let mono = interleaved.iter().copied().sum::<f32>() / interleaved.len().max(1) as f32;
                let mono = mono.clamp(-1.0, 1.0);
                sampled_frames += 1;
                if mono_samples.len() < 65_536 {
                    mono_samples.push((absolute_frame, mono));
                } else {
                    // Reservoir sampling keeps a bounded, reproducible sample across the
                    // whole recording instead of embedding only its opening seconds.
                    random_state ^= random_state << 13;
                    random_state ^= random_state >> 7;
                    random_state ^= random_state << 17;
                    let slot = random_state % sampled_frames;
                    if slot < mono_samples.len() as u64 {
                        mono_samples[slot as usize] = (absolute_frame, mono);
                    }
                }
            }
        }
    }

    if mono_samples.is_empty() {
        return Err(format!("audio file contains no decodable samples: {}", path.display()));
    }
    mono_samples.sort_unstable_by_key(|(position, _)| *position);
    let duration = frame_count as f64 / sample_rate as f64;
    Ok((
        audio_features(&mono_samples, frame_count),
        duration,
        sample_rate,
        channels,
    ))
}

fn audio_features(samples: &[(u64, f32)], total_frames: u64) -> Vec<f32> {
    let mut features = Vec::with_capacity(32);
    let mut segments: Vec<Vec<f32>> = (0..16).map(|_| Vec::new()).collect();
    let total_frames = total_frames.max(1) as u128;
    for (position, sample) in samples {
        let segment = ((*position as u128 * 16) / total_frames).min(15) as usize;
        segments[segment].push(*sample);
    }
    for slice in &segments {
        let energy = if slice.is_empty() {
            0.0
        } else {
            (slice.iter().map(|sample| sample * sample).sum::<f32>() / slice.len() as f32).sqrt()
        };
        features.push(energy);
    }
    for slice in &segments {
        let crossings = slice
            .windows(2)
            .filter(|pair| (pair[0] >= 0.0) != (pair[1] >= 0.0))
            .count();
        features.push(if slice.len() < 2 {
            0.0
        } else {
            crossings as f32 / (slice.len() - 1) as f32
        });
    }
    features
}
