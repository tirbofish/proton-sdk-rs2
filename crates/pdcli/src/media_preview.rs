//! Thumbnails and dimensions for photo uploads, so Proton Photos can show and play
//! them. Images the `image` crate cannot decode (HEIC) and videos go through
//! `ffmpeg`/`ffprobe` when installed.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use proton_drive_sdk::api::attr::MediaExtendedAttributes;
use proton_drive_sdk::node::thumbnail::Thumbnail;
use proton_drive_sdk::utils::thumbnail::ThumbnailGenerator;
use serde::Deserialize;

const FFMPEG_TIMEOUT: Duration = Duration::from_secs(60);
/// Larger images are not decoded in memory just for a thumbnail.
const MAX_DECODED_IMAGE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Default)]
pub struct MediaPreview {
    pub thumbnails: Vec<Thumbnail>,
    pub media: Option<MediaExtendedAttributes>,
}

/// Best effort: a photo without a preview still uploads, so failures only log.
pub async fn generate(path: &Path, media_type: &str, size: u64) -> MediaPreview {
    let mut preview = MediaPreview::default();
    if media_type.starts_with("image/") && size <= MAX_DECODED_IMAGE_BYTES {
        if let Ok(data) = tokio::fs::read(path).await {
            preview = from_image_bytes(data).await;
        }
    }
    if preview.thumbnails.is_empty() {
        if let Some(frame) = ffmpeg_frame(path).await {
            let probed = ffprobe_media(path).await;
            preview = from_image_bytes(frame).await;
            // The frame is scaled down, so its dimensions are not the original's.
            preview.media = probed.or(preview.media);
        }
    } else if media_type.starts_with("video/") {
        preview.media = ffprobe_media(path).await.or(preview.media);
    }
    if preview.thumbnails.is_empty() {
        tracing::warn!(path = %path.display(), "no thumbnail generated; install ffmpeg for videos and HEIC");
    }
    preview
}

async fn from_image_bytes(data: Vec<u8>) -> MediaPreview {
    tokio::task::spawn_blocking(move || {
        let (thumbnails, media) = ThumbnailGenerator::generate_thumbnails(&data);
        MediaPreview { thumbnails, media }
    })
    .await
    .unwrap_or_default()
}

/// A representative frame, rotated upright and at most 1024 pixels wide, as PNG.
async fn ffmpeg_frame(path: &Path) -> Option<Vec<u8>> {
    let output = run_with_timeout(
        tokio::process::Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-i"])
            .arg(path)
            .args([
                "-vf",
                "thumbnail,scale='min(1024,iw)':-2",
                "-frames:v",
                "1",
                "-f",
                "image2pipe",
                "-c:v",
                "png",
                "-",
            ]),
    )
    .await?;
    (!output.is_empty()).then_some(output)
}

async fn ffprobe_media(path: &Path) -> Option<MediaExtendedAttributes> {
    let output = run_with_timeout(
        tokio::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=width,height:stream_tags=rotate:stream_side_data=rotation:format=duration",
                "-of",
                "json",
            ])
            .arg(path),
    )
    .await?;
    parse_ffprobe(&output)
}

async fn run_with_timeout(command: &mut tokio::process::Command) -> Option<Vec<u8>> {
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let output = tokio::time::timeout(FFMPEG_TIMEOUT, child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    output.status.success().then_some(output.stdout)
}

#[derive(Deserialize)]
struct Probe {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    format: Option<ProbeFormat>,
}

#[derive(Deserialize)]
struct ProbeStream {
    width: Option<u32>,
    height: Option<u32>,
    #[serde(default)]
    tags: std::collections::HashMap<String, String>,
    #[serde(default)]
    side_data_list: Vec<ProbeSideData>,
}

#[derive(Deserialize)]
struct ProbeSideData {
    rotation: Option<f64>,
}

#[derive(Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
}

fn parse_ffprobe(json: &[u8]) -> Option<MediaExtendedAttributes> {
    let probe: Probe = serde_json::from_slice(json).ok()?;
    let stream = probe.streams.into_iter().next();
    let rotation = stream.as_ref().and_then(|stream| {
        stream
            .side_data_list
            .iter()
            .find_map(|data| data.rotation)
            .or_else(|| stream.tags.get("rotate")?.parse().ok())
    });
    let (mut width, mut height) = stream
        .map(|stream| (stream.width, stream.height))
        .unwrap_or_default();
    // Phones record portrait video as landscape frames plus a rotation.
    if rotation.is_some_and(|degrees| (degrees.abs() as u32) % 180 == 90) {
        std::mem::swap(&mut width, &mut height);
    }
    let duration = probe
        .format
        .and_then(|format| format.duration?.parse::<f64>().ok())
        .map(|seconds| seconds.round() as u32);
    (width.is_some() || duration.is_some()).then_some(MediaExtendedAttributes {
        width,
        height,
        duration,
    })
}

#[cfg(test)]
mod tests {
    use super::parse_ffprobe;

    #[test]
    fn ffprobe_dimensions_follow_rotation_and_duration_rounds() {
        let media = parse_ffprobe(
            br#"{"streams":[{"width":1920,"height":1080,"side_data_list":[{"rotation":-90}]}],"format":{"duration":"12.6"}}"#,
        )
        .unwrap();
        assert_eq!(
            (media.width, media.height, media.duration),
            (Some(1080), Some(1920), Some(13))
        );

        let media = parse_ffprobe(
            br#"{"streams":[{"width":640,"height":480,"tags":{"rotate":"0"}}],"format":{}}"#,
        )
        .unwrap();
        assert_eq!(
            (media.width, media.height, media.duration),
            (Some(640), Some(480), None)
        );
    }
}
