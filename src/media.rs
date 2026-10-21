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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("mmdb-media-tests-{}-{id}", std::process::id()));
            fs::create_dir(&path).expect("create unique temporary directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn analysis_error(path: &Path) -> String {
        match analyze(path) {
            Ok(_) => panic!("analysis should fail for fixture"),
            Err(error) => error,
        }
    }

    fn write_wav(path: &Path, samples: &[i16], sample_rate: u32) {
        let data_len = (samples.len() * 2) as u32;
        let mut file = fs::File::create(path).unwrap();
        file.write_all(b"RIFF").unwrap();
        file.write_all(&(36 + data_len).to_le_bytes()).unwrap();
        file.write_all(b"WAVEfmt ").unwrap();
        file.write_all(&16_u32.to_le_bytes()).unwrap();
        file.write_all(&1_u16.to_le_bytes()).unwrap();
        file.write_all(&1_u16.to_le_bytes()).unwrap();
        file.write_all(&sample_rate.to_le_bytes()).unwrap();
        file.write_all(&(sample_rate * 2).to_le_bytes()).unwrap();
        file.write_all(&2_u16.to_le_bytes()).unwrap();
        file.write_all(&16_u16.to_le_bytes()).unwrap();
        file.write_all(b"data").unwrap();
        file.write_all(&data_len.to_le_bytes()).unwrap();
        for sample in samples {
            file.write_all(&sample.to_le_bytes()).unwrap();
        }
    }

    #[test]
    fn media_type_uses_stable_lowercase_names() {
        assert_eq!(MediaType::Image.as_str(), "image");
        assert_eq!(MediaType::Audio.as_str(), "audio");
        assert_eq!(
            serde_json::to_string(&MediaType::Image).unwrap(),
            "\"image\""
        );
        assert_eq!(
            serde_json::from_str::<MediaType>("\"audio\"").unwrap(),
            MediaType::Audio
        );
    }

    #[test]
    fn image_embedding_is_a_normalized_four_histogram_vector() {
        let mut image = image::RgbImage::new(2, 1);
        image.put_pixel(0, 0, image::Rgb([0, 64, 255]));
        image.put_pixel(1, 0, image::Rgb([31, 95, 224]));

        let embedding = image_embedding(&image);

        assert_eq!(embedding.len(), 32);
        for channel_histogram in embedding.chunks_exact(8) {
            assert!((channel_histogram.iter().sum::<f32>() - 1.0).abs() < f32::EPSILON);
        }
        assert_eq!(embedding[0], 1.0);
        assert_eq!(embedding[10], 1.0);
        assert_eq!(embedding[23], 1.0);
        assert_eq!(embedding[25], 0.5);
        assert_eq!(embedding[26], 0.5);
    }

    #[test]
    fn analyze_reads_image_dimensions_format_and_embedding() {
        let directory = TempDir::new();
        let path = directory.path().join("sample.png");
        let mut image = image::RgbImage::new(3, 2);
        for pixel in image.pixels_mut() {
            *pixel = image::Rgb([40, 100, 200]);
        }
        image.save(&path).unwrap();

        let analysis = analyze(&path).unwrap();

        assert_eq!(analysis.media_type, MediaType::Image);
        assert_eq!(analysis.metadata["width"], json!(3));
        assert_eq!(analysis.metadata["height"], json!(2));
        assert_eq!(analysis.metadata["channels"], json!(3));
        assert_eq!(analysis.metadata["format"], json!("png"));
        assert_eq!(analysis.embedding.len(), 32);
        assert!(analysis.embedding.iter().all(|value| value.is_finite()));
    }

    #[test]
    fn analyze_reports_corrupt_image_files() {
        let directory = TempDir::new();
        let path = directory.path().join("corrupt.png");
        fs::write(&path, b"not a png").unwrap();

        let error = analysis_error(&path);

        assert!(error.contains("cannot decode image"));
        assert!(error.contains("corrupt.png"));
    }

    #[test]
    fn audio_extension_check_covers_supported_aliases_only() {
        for extension in [
            "wav", "wave", "mp3", "flac", "ogg", "opus", "aac", "m4a", "aiff", "aif",
        ] {
            assert!(is_audio_extension(extension), "{extension}");
        }
        for extension in ["", "WAV", "mp4", "png", "mkv"] {
            assert!(!is_audio_extension(extension), "{extension}");
        }
    }

    #[test]
    fn analyze_decodes_generated_wav_and_reports_audio_features() {
        let directory = TempDir::new();
        let path = directory.path().join("tone.wav");
        let samples: Vec<i16> = (0..800)
            .map(|index| {
                if (index / 2) % 2 == 0 {
                    16_000
                } else {
                    -16_000
                }
            })
            .collect();
        write_wav(&path, &samples, 8_000);

        let analysis = analyze(&path).unwrap();

        assert_eq!(analysis.media_type, MediaType::Audio);
        assert_eq!(analysis.metadata["format"], json!("wav"));
        assert_eq!(analysis.metadata["duration_seconds"], json!(0.1));
        assert_eq!(analysis.metadata["sample_rate"], json!(8_000));
        assert_eq!(analysis.metadata["channels"], json!(1));
        assert_eq!(analysis.embedding.len(), 32);
        assert!(analysis.embedding.iter().all(|value| value.is_finite()));
        assert!(analysis.embedding[..16].iter().any(|energy| *energy > 0.0));
        assert!(analysis.embedding[16..]
            .iter()
            .any(|crossing| *crossing > 0.0));
    }

    #[test]
    fn analyze_reports_invalid_audio_containers() {
        let directory = TempDir::new();
        let path = directory.path().join("corrupt.wav");
        fs::write(&path, b"not a wav file").unwrap();

        let error = analysis_error(&path);

        assert!(error.contains("cannot inspect audio"));
    }

    #[test]
    fn analyze_rejects_unsupported_media_extensions() {
        let directory = TempDir::new();
        let path = directory.path().join("movie.mp4");
        fs::write(&path, b"video").unwrap();

        let error = analysis_error(&path);

        assert!(error.contains("unsupported media file"));
        assert!(error.contains("Video is not supported"));
    }

    #[test]
    fn audio_features_split_energy_and_crossings_into_sixteen_segments() {
        let samples = vec![(0, -1.0), (1, 1.0), (2, -1.0), (80, 0.5), (81, 0.5)];

        let features = audio_features(&samples, 160);

        assert_eq!(features.len(), 32);
        assert!((features[0] - 1.0).abs() < f32::EPSILON);
        assert!((features[8] - 0.5).abs() < f32::EPSILON);
        assert_eq!(features[1], 0.0);
        assert_eq!(features[16], 1.0);
        assert_eq!(features[24], 0.0);
        assert!(features[1..8].iter().all(|value| *value == 0.0));
        assert!(features[9..16].iter().all(|value| *value == 0.0));
        assert!(features[17..].iter().all(|value| *value == 0.0));
    }

    #[test]
    fn audio_features_handle_empty_samples_and_zero_total_frames() {
        let empty = audio_features(&[], 100);
        let single = audio_features(&[(0, -0.5)], 0);

        assert_eq!(empty, vec![0.0; 32]);
        assert_eq!(single.len(), 32);
        assert_eq!(single[0], 0.5);
        assert!(single[1..].iter().all(|value| *value == 0.0));
    }
}
