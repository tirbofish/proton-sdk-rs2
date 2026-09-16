//! Additional node metadata (EXIF, camera, GPS) ported from the TypeScript SDK.
//!
//! Binary EXIF decoding via `exifreader` is stubbed: image files return empty
//! tags unless a loader is injected. Parsing, tagging, and Apple MakerNote
//! inspection match the TypeScript helpers.

use crate::node::photo::PhotoTag;
use chrono::{TimeZone, Utc};
use serde_json::Value;

const ANDROID_PORTRAIT: &str =
    "com.google.android.apps.camera.gallery.specialtype.SpecialType-PORTRAIT";

const RAW_MIME_TYPES: &[(&str, &str)] = &[
    ("dcraw", "image/x-dcraw"),
    ("dng", "image/x-adobe-dng"),
    ("crw", "image/x-canon-crw"),
    ("cr2", "image/x-canon-cr2"),
    ("cr3", "image/x-canon-cr3"),
    ("erf", "image/x-epson-erf"),
    ("raf", "image/x-fuji-raf"),
    ("dcr", "image/x-kodak-dcr"),
    ("k25", "image/x-kodak-k25"),
    ("kdc", "image/x-kodak-kdc"),
    ("mrw", "image/x-minolta-mrw"),
    ("nef", "image/x-nikon-nef"),
    ("nrw", "image/x-nikon-nrw"),
    ("orf", "image/x-olympus-orf"),
    ("raw", "image/x-panasonic-raw"),
    ("rw2", "image/x-panasonic-rw2"),
    ("pef", "image/x-pentax-pef"),
    ("ptx", "image/x-pentax-ptx"),
    ("x3f", "image/x-sigma-x3f"),
    ("srf", "image/x-sony-srf"),
    ("sr2", "image/x-sony-sr2"),
    ("arw", "image/x-sony-arw"),
    ("iiq", "image/x-phaseone-iiq"),
    ("mef", "image/x-mamiya-mef"),
    ("rwl", "image/x-leica-rwl"),
    ("3fr", "image/x-hasselblad-3fr"),
    ("fff", "image/x-hasselblad-fff"),
];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SubjectCoordinates {
    pub top: i64,
    pub left: i64,
    pub bottom: i64,
    pub right: i64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AdditionalNodeMetadata {
    pub media: Option<MediaInfo>,
    pub location: Option<Location>,
    pub camera: Option<Camera>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MediaInfo {
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub duration: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Location {
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Camera {
    pub capture_time: Option<String>,
    pub device: Option<String>,
    pub orientation: Option<i64>,
    pub subject_coordinates: Option<SubjectCoordinates>,
}

#[derive(Debug, Clone, Default)]
pub struct FileLike {
    pub name: Option<String>,
    pub last_modified: i64,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct ExifTag {
    pub value: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct NumericTag {
    pub value: i64,
}

#[derive(Debug, Clone, Default)]
pub struct ExpandedTags {
    pub exif: Option<ExifTags>,
    pub gps: Option<GpsTags>,
    pub png: Option<PngTags>,
    pub xmp: Option<XmpTags>,
}

#[derive(Debug, Clone, Default)]
pub struct ExifTags {
    pub date_time: Option<ExifTag>,
    pub date_time_original: Option<ExifTag>,
    pub date_time_digitized: Option<ExifTag>,
    pub image_width: Option<NumericTag>,
    pub image_length: Option<NumericTag>,
    pub pixel_x_dimension: Option<NumericTag>,
    pub pixel_y_dimension: Option<NumericTag>,
    pub model: Option<ExifTag>,
    pub orientation: Option<NumericTag>,
    pub subject_area: Option<Vec<i64>>,
    pub maker_note: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Default)]
pub struct GpsTags {
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct PngTags {
    pub image_width: Option<i64>,
    pub image_height: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct XmpTags {
    pub user_comment: Option<String>,
    pub projection_type: Option<String>,
    pub motion_photo: Option<String>,
    pub special_type_id: Option<XmpSpecialType>,
}

#[derive(Debug, Clone)]
pub enum XmpSpecialType {
    Text(String),
    List(Vec<String>),
}

#[derive(Debug, Clone)]
pub enum MakerNoteInput {
    Missing,
    Invalid,
    Bytes(Vec<u8>),
}

pub fn format_exif_date_time(exif_date_time: &str) -> anyhow::Result<String> {
    let ok = exif_date_time.len() == 19
        && exif_date_time.as_bytes().iter().enumerate().all(|(i, b)| {
            let expected = b"YYYY:MM:DD HH:MM:SS";
            match expected[i] {
                b'Y' | b'M' | b'D' | b'H' | b'S' => b.is_ascii_digit(),
                other => *b == other,
            }
        });
    if !ok {
        anyhow::bail!(
            "The DateTime passed is not in the right format (received: {exif_date_time}, expected: YYYY:MM:DD HH:MM:SS)"
        );
    }
    let (date, time) = exif_date_time.split_once(' ').unwrap();
    Ok(format!("{} {time}", date.replace(':', "-")))
}

pub fn convert_subject_area_to_subject_coordinates(
    subject_area: &[i64],
) -> anyhow::Result<SubjectCoordinates> {
    match subject_area {
        [x, y] => Ok(SubjectCoordinates {
            top: *y,
            left: *x,
            bottom: *y,
            right: *x,
        }),
        [x, y, diameter] => {
            let radius = diameter / 2;
            Ok(SubjectCoordinates {
                top: y - radius,
                left: x - radius,
                bottom: y + radius,
                right: x + radius,
            })
        }
        [x, y, width, height] => {
            let middle_height = height / 2;
            let middle_width = width / 2;
            Ok(SubjectCoordinates {
                top: y - middle_height,
                left: x - middle_width,
                bottom: y + middle_height,
                right: x + middle_width,
            })
        }
        _ => anyhow::bail!("Invalid exif SubjectArea was passed"),
    }
}

pub fn is_apple_maker_note(input: MakerNoteInput) -> Option<Vec<u8>> {
    match input {
        MakerNoteInput::Bytes(bytes) => Some(bytes),
        MakerNoteInput::Missing | MakerNoteInput::Invalid => None,
    }
}

pub fn detect_selfie_from_maker_note(maker_note: Option<&[u8]>) -> bool {
    let Some(data) = maker_note else {
        return false;
    };
    if data.len() < 20 || !is_apple_format(data) {
        return false;
    }
    extract_value_from_maker_note(0x002e, data) == Some(6)
}

pub fn detect_portrait_from_maker_note(maker_note: Option<&[u8]>) -> bool {
    let Some(data) = maker_note else {
        return false;
    };
    if data.len() < 20 || !is_apple_format(data) {
        return false;
    }
    extract_value_from_maker_note(0x0014, data) == Some(2)
}

fn is_apple_format(data: &[u8]) -> bool {
    data.len() >= 5 && data[..5] == [65, 112, 112, 108, 101]
}

fn extract_value_from_maker_note(tag_to_find: u16, maker_note: &[u8]) -> Option<i64> {
    if maker_note.len() < 20 {
        return None;
    }
    for i in 0..maker_note.len().saturating_sub(12) {
        let current_tag = u16::from(maker_note[i]) << 8 | u16::from(maker_note[i + 1]);
        if current_tag == tag_to_find {
            return Some(
                (i64::from(maker_note[i + 8]) << 24)
                    | (i64::from(maker_note[i + 9]) << 16)
                    | (i64::from(maker_note[i + 10]) << 8)
                    | i64::from(maker_note[i + 11]),
            );
        }
    }
    None
}

pub fn get_file_extension(name: Option<&str>) -> String {
    name.unwrap_or("")
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_string()
}

pub fn is_image(media_type: &str) -> bool {
    media_type.starts_with("image/")
}

pub fn is_video(media_type: &str) -> bool {
    media_type.starts_with("video/")
}

pub fn is_raw_photo(media_type: &str) -> bool {
    RAW_MIME_TYPES.iter().any(|(_, mime)| *mime == media_type)
}

pub fn is_raw_extension(extension: &str) -> bool {
    RAW_MIME_TYPES
        .iter()
        .any(|(ext, _)| ext.eq_ignore_ascii_case(extension))
}

pub fn get_formatted_date_time(exif: Option<&ExifTags>) -> Option<String> {
    let exif = exif?;
    for source in [
        exif.date_time_original.as_ref(),
        exif.date_time_digitized.as_ref(),
        exif.date_time.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(value) = source.value.first() {
            if let Ok(formatted) = format_exif_date_time(value) {
                return Some(formatted);
            }
        }
    }
    None
}

pub fn get_capture_date_time_string(exif: Option<&ExifTags>) -> Option<String> {
    let formatted = get_formatted_date_time(exif)?;
    let parsed = chrono::NaiveDateTime::parse_from_str(&formatted, "%Y-%m-%d %H:%M:%S").ok()?;
    Some(
        Utc.from_utc_datetime(&parsed)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    )
}

pub fn get_capture_date_time(file: &FileLike, exif: Option<&ExifTags>) -> chrono::DateTime<Utc> {
    if let Some(formatted) = get_formatted_date_time(exif) {
        if let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(&formatted, "%Y-%m-%d %H:%M:%S") {
            return Utc.from_utc_datetime(&parsed);
        }
    }
    Utc.timestamp_millis_opt(file.last_modified)
        .single()
        .unwrap_or_else(Utc::now)
}

pub fn get_photo_dimensions(tags: &ExpandedTags) -> (Option<i64>, Option<i64>) {
    let width = tags
        .exif
        .as_ref()
        .and_then(|exif| {
            exif.image_width
                .as_ref()
                .or(exif.pixel_x_dimension.as_ref())
        })
        .map(|tag| tag.value)
        .or_else(|| tags.png.as_ref().and_then(|png| png.image_width));
    let height = tags
        .exif
        .as_ref()
        .and_then(|exif| {
            exif.image_length
                .as_ref()
                .or(exif.pixel_y_dimension.as_ref())
        })
        .map(|tag| tag.value)
        .or_else(|| tags.png.as_ref().and_then(|png| png.image_height));
    (width, height)
}

pub fn build_additional_node_metadata(
    exif_info: Option<&ExpandedTags>,
    media_info: Option<&MediaInfo>,
) -> AdditionalNodeMetadata {
    let (width, height) = match exif_info {
        Some(tags) => get_photo_dimensions(tags),
        None => (
            media_info.and_then(|info| info.width),
            media_info.and_then(|info| info.height),
        ),
    };
    let (location, camera) = exif_info
        .map(photo_extended_attributes)
        .unwrap_or((None, None));
    AdditionalNodeMetadata {
        media: Some(MediaInfo {
            width,
            height,
            duration: media_info.and_then(|info| info.duration),
        }),
        location,
        camera,
    }
}

fn photo_extended_attributes(tags: &ExpandedTags) -> (Option<Location>, Option<Camera>) {
    let capture_time = tags
        .exif
        .as_ref()
        .and_then(|exif| get_capture_date_time_string(Some(exif)));
    let subject_coordinates = tags
        .exif
        .as_ref()
        .and_then(|exif| exif.subject_area.as_deref())
        .filter(|area| matches!(area.len(), 2 | 3 | 4))
        .and_then(|area| convert_subject_area_to_subject_coordinates(area).ok());
    let location = tags.gps.as_ref().and_then(|gps| {
        Some(Location {
            latitude: gps.latitude?,
            longitude: gps.longitude?,
        })
    });
    let camera = tags.exif.as_ref().map(|exif| Camera {
        device: exif
            .model
            .as_ref()
            .and_then(|tag| tag.value.first().cloned()),
        orientation: exif.orientation.as_ref().map(|tag| tag.value),
        capture_time,
        subject_coordinates,
    });
    (location, camera)
}

pub fn get_photo_tags(
    file: &FileLike,
    media_type: &str,
    exif_info: Option<&ExpandedTags>,
) -> Vec<PhotoTag> {
    let mut tags = Vec::new();
    let extension = get_file_extension(file.name.as_deref());
    if is_raw_photo(media_type) || is_raw_extension(&extension) {
        tags.push(PhotoTag::Raw);
    }
    if is_video(media_type) {
        tags.push(PhotoTag::Video);
    }
    let Some(info) = exif_info else {
        return tags;
    };
    let Some(xmp) = &info.xmp else {
        return tags;
    };
    let apple = info
        .exif
        .as_ref()
        .and_then(|exif| exif.maker_note.as_deref())
        .and_then(|bytes| is_apple_maker_note(MakerNoteInput::Bytes(bytes.to_vec())));
    if xmp.user_comment.as_deref() == Some("Screenshot")
        || file
            .name
            .as_deref()
            .is_some_and(|name| name.to_ascii_lowercase().contains("screenshot"))
    {
        tags.push(PhotoTag::Screenshot);
    }
    if xmp.projection_type.as_deref() == Some("equirectangular") {
        tags.push(PhotoTag::Panorama);
    }
    if xmp.motion_photo.as_deref() == Some("1") {
        tags.push(PhotoTag::MotionPhoto);
    }
    let android_portrait = match &xmp.special_type_id {
        Some(XmpSpecialType::Text(value)) => value == ANDROID_PORTRAIT,
        Some(XmpSpecialType::List(values)) => values.iter().any(|value| value == ANDROID_PORTRAIT),
        None => false,
    };
    if android_portrait
        || apple
            .as_deref()
            .is_some_and(|note| detect_portrait_from_maker_note(Some(note)))
    {
        tags.push(PhotoTag::Portrait);
    }
    if apple
        .as_deref()
        .is_some_and(|note| detect_selfie_from_maker_note(Some(note)))
    {
        tags.push(PhotoTag::Selfie);
    }
    tags
}

pub fn get_exif_info(
    file: &FileLike,
    media_type: &str,
    mut loader: Option<&mut dyn FnMut(&[u8]) -> anyhow::Result<ExpandedTags>>,
    logger: Option<&mut dyn FnMut(String)>,
) -> Option<ExpandedTags> {
    if !is_image(media_type) {
        return None;
    }
    match loader.as_mut() {
        Some(load) => match load(&file.bytes) {
            Ok(tags) => Some(tags),
            Err(error) => {
                if let Some(log) = logger {
                    log(format!("Cannot read exif data: {error}"));
                }
                None
            }
        },
        None => None,
    }
}

pub fn generate_additional_node_metadata(
    file: &FileLike,
    media_type: &str,
    media_info: Option<&MediaInfo>,
) -> AdditionalNodeMetadata {
    let exif = get_exif_info(file, media_type, None, None);
    build_additional_node_metadata(exif.as_ref(), media_info)
}

pub fn generate_additional_photo_node_metadata(
    file: &FileLike,
    media_type: &str,
    media_info: Option<&MediaInfo>,
) -> (AdditionalNodeMetadata, Vec<PhotoTag>, chrono::DateTime<Utc>) {
    let exif = get_exif_info(file, media_type, None, None);
    let metadata = build_additional_node_metadata(exif.as_ref(), media_info);
    let tags = get_photo_tags(file, media_type, exif.as_ref());
    let capture_time =
        get_capture_date_time(file, exif.as_ref().and_then(|tags| tags.exif.as_ref()));
    (metadata, tags, capture_time)
}

pub fn parse_additional_node_metadata(raw: &Value) -> AdditionalNodeMetadata {
    let mut result = AdditionalNodeMetadata::default();
    let Some(object) = raw.as_object() else {
        return result;
    };
    if let Some(location) = object.get("Location") {
        result.location = parse_location(location);
    }
    if let Some(camera) = object.get("Camera") {
        result.camera = parse_camera(camera);
    }
    if let Some(media) = object.get("Media") {
        result.media = parse_media(media);
    }
    result
}

fn parse_location(value: &Value) -> Option<Location> {
    let latitude = parse_number(value.get("Latitude"))?;
    let longitude = parse_number(value.get("Longitude"))?;
    if !(-90.0..=90.0).contains(&latitude) || !(-180.0..=180.0).contains(&longitude) {
        return None;
    }
    Some(Location {
        latitude,
        longitude,
    })
}

fn parse_camera(value: &Value) -> Option<Camera> {
    let camera = Camera {
        capture_time: parse_string(value.get("CaptureTime")),
        device: parse_string(value.get("Device")),
        orientation: parse_orientation(value.get("Orientation")),
        subject_coordinates: parse_subject_coordinates(value.get("SubjectCoordinates")),
    };
    if camera.capture_time.is_none()
        && camera.device.is_none()
        && camera.orientation.is_none()
        && camera.subject_coordinates.is_none()
    {
        None
    } else {
        Some(camera)
    }
}

fn parse_media(value: &Value) -> Option<MediaInfo> {
    let media = MediaInfo {
        width: parse_integer(value.get("Width")),
        height: parse_integer(value.get("Height")),
        duration: parse_number(value.get("Duration")),
    };
    if media.width.is_none() && media.height.is_none() && media.duration.is_none() {
        None
    } else {
        Some(media)
    }
}

fn parse_subject_coordinates(value: Option<&Value>) -> Option<SubjectCoordinates> {
    let value = value?;
    Some(SubjectCoordinates {
        top: parse_integer(value.get("Top"))?,
        left: parse_integer(value.get("Left"))?,
        bottom: parse_integer(value.get("Bottom"))?,
        right: parse_integer(value.get("Right"))?,
    })
}

fn parse_orientation(value: Option<&Value>) -> Option<i64> {
    let num = parse_integer(value)?;
    (1..=8).contains(&num).then_some(num)
}

fn parse_integer(value: Option<&Value>) -> Option<i64> {
    let num = parse_number(value)?;
    if num.fract() == 0.0 {
        Some(num as i64)
    } else {
        None
    }
}

fn parse_number(value: Option<&Value>) -> Option<f64> {
    let number = value?.as_f64()?;
    number.is_finite().then_some(number)
}

fn parse_string(value: Option<&Value>) -> Option<String> {
    value?.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn format_exif_date_time_accepts_valid_and_rejects_invalid() {
        assert_eq!(
            format_exif_date_time("2023:07:21 22:12:01").unwrap(),
            "2023-07-21 22:12:01"
        );
        let error = format_exif_date_time("2023-07:21-22:12:01").unwrap_err();
        assert!(error.to_string().contains("YYYY:MM:DD HH:MM:SS"));
    }

    #[test]
    fn convert_subject_area_matches_typescript() {
        assert_eq!(
            convert_subject_area_to_subject_coordinates(&[232, 643]).unwrap(),
            SubjectCoordinates {
                top: 643,
                left: 232,
                bottom: 643,
                right: 232
            }
        );
        assert_eq!(
            convert_subject_area_to_subject_coordinates(&[232, 643, 142]).unwrap(),
            SubjectCoordinates {
                top: 572,
                left: 161,
                bottom: 714,
                right: 303
            }
        );
        assert_eq!(
            convert_subject_area_to_subject_coordinates(&[232, 643, 142, 432]).unwrap(),
            SubjectCoordinates {
                top: 427,
                left: 161,
                bottom: 859,
                right: 303
            }
        );
    }

    fn mock_apple_maker_note(tag: u16, value: u8) -> Vec<u8> {
        let mut data = vec![0; 100];
        data[..5].copy_from_slice(&[65, 112, 112, 108, 101]);
        data[20] = (tag >> 8) as u8;
        data[21] = tag as u8;
        data[31] = value;
        data
    }

    #[test]
    fn apple_maker_note_detection() {
        assert!(is_apple_maker_note(MakerNoteInput::Missing).is_none());
        assert!(is_apple_maker_note(MakerNoteInput::Invalid).is_none());
        assert_eq!(
            is_apple_maker_note(MakerNoteInput::Bytes(vec![1, 2, 3])).unwrap(),
            vec![1, 2, 3]
        );
        assert!(!detect_selfie_from_maker_note(None));
        assert!(!detect_selfie_from_maker_note(Some(&[1, 2, 3])));
        assert!(!detect_selfie_from_maker_note(Some(&vec![0; 30])));
        assert!(!detect_selfie_from_maker_note(Some(
            &mock_apple_maker_note(0x002e, 0)
        )));
        assert!(!detect_selfie_from_maker_note(Some(
            &mock_apple_maker_note(0x002e, 1)
        )));
        assert!(detect_selfie_from_maker_note(Some(&mock_apple_maker_note(
            0x002e, 6
        ))));
        assert!(detect_portrait_from_maker_note(Some(
            &mock_apple_maker_note(0x0014, 2)
        )));
        assert!(!detect_portrait_from_maker_note(Some(
            &mock_apple_maker_note(0x0014, 10)
        )));
        assert!(!detect_portrait_from_maker_note(Some(
            &mock_apple_maker_note(0x0014, 1)
        )));
        let mut apple_only = vec![0; 100];
        apple_only[..5].copy_from_slice(&[65, 112, 112, 108, 101]);
        assert!(!detect_portrait_from_maker_note(Some(&apple_only)));
    }

    #[test]
    fn formatted_datetime_prefers_original_then_digitized() {
        let mock = ExifTags {
            date_time: Some(ExifTag {
                value: vec!["2024:01:07 10:00:53".into()],
            }),
            date_time_original: Some(ExifTag {
                value: vec!["2024:01:07 09:00:53".into()],
            }),
            date_time_digitized: Some(ExifTag {
                value: vec!["2024:01:07 08:00:53".into()],
            }),
            ..Default::default()
        };
        assert_eq!(
            get_formatted_date_time(Some(&mock)).as_deref(),
            Some("2024-01-07 09:00:53")
        );
        let mut missing_original = mock.clone();
        missing_original.date_time_original = None;
        assert_eq!(
            get_formatted_date_time(Some(&missing_original)).as_deref(),
            Some("2024-01-07 08:00:53")
        );
        missing_original.date_time_digitized = None;
        assert_eq!(
            get_formatted_date_time(Some(&missing_original)).as_deref(),
            Some("2024-01-07 10:00:53")
        );
        assert!(get_formatted_date_time(Some(&ExifTags::default())).is_none());
        let mut unparsable = mock;
        unparsable.date_time_original = Some(ExifTag {
            value: vec!["some random text".into()],
        });
        assert_eq!(
            get_formatted_date_time(Some(&unparsable)).as_deref(),
            Some("2024-01-07 08:00:53")
        );
    }

    #[test]
    fn builder_uses_media_info_without_exif() {
        let result = build_additional_node_metadata(
            None,
            Some(&MediaInfo {
                width: Some(1920),
                height: Some(1080),
                duration: Some(120.0),
            }),
        );
        assert_eq!(result.media.unwrap().width, Some(1920));
        assert!(result.location.is_none());
        assert!(result.camera.is_none());
    }

    #[test]
    fn builder_extracts_exif_camera_gps_and_subject_area() {
        let tags = ExpandedTags {
            exif: Some(ExifTags {
                image_width: Some(NumericTag { value: 3840 }),
                image_length: Some(NumericTag { value: 2160 }),
                model: Some(ExifTag {
                    value: vec!["iPhone 12".into()],
                }),
                orientation: Some(NumericTag { value: 6 }),
                date_time_original: Some(ExifTag {
                    value: vec!["2024:01:15 14:30:00".into()],
                }),
                subject_area: Some(vec![500, 300, 200]),
                ..Default::default()
            }),
            gps: Some(GpsTags {
                latitude: Some(48.8566),
                longitude: Some(2.3522),
            }),
            ..Default::default()
        };
        let result = build_additional_node_metadata(Some(&tags), None);
        assert_eq!(result.media.as_ref().unwrap().width, Some(3840));
        assert_eq!(result.location.unwrap().latitude, 48.8566);
        assert_eq!(
            result.camera.as_ref().unwrap().device.as_deref(),
            Some("iPhone 12")
        );
        assert_eq!(result.camera.as_ref().unwrap().orientation, Some(6));
        assert!(
            result
                .camera
                .as_ref()
                .unwrap()
                .capture_time
                .as_ref()
                .unwrap()
                .contains("2024-01-15")
        );
        assert_eq!(
            result.camera.unwrap().subject_coordinates.unwrap(),
            SubjectCoordinates {
                top: 200,
                left: 400,
                bottom: 400,
                right: 600
            }
        );
    }

    #[test]
    fn builder_skips_malformed_subject_area() {
        let tags = ExpandedTags {
            exif: Some(ExifTags {
                model: Some(ExifTag {
                    value: vec!["iPhone 12".into()],
                }),
                subject_area: Some(vec![]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = build_additional_node_metadata(Some(&tags), None);
        assert_eq!(
            result.camera.as_ref().unwrap().device.as_deref(),
            Some("iPhone 12")
        );
        assert!(result.camera.unwrap().subject_coordinates.is_none());
    }

    #[test]
    fn builder_prefers_exif_dimensions_and_png() {
        let tags = ExpandedTags {
            exif: Some(ExifTags {
                image_width: Some(NumericTag { value: 3840 }),
                image_length: Some(NumericTag { value: 2160 }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = build_additional_node_metadata(
            Some(&tags),
            Some(&MediaInfo {
                width: Some(1920),
                height: Some(1080),
                duration: Some(60.0),
            }),
        );
        assert_eq!(result.media.as_ref().unwrap().width, Some(3840));
        assert_eq!(result.media.unwrap().duration, Some(60.0));

        let png = ExpandedTags {
            png: Some(PngTags {
                image_width: Some(800),
                image_height: Some(600),
            }),
            ..Default::default()
        };
        let result = build_additional_node_metadata(Some(&png), None);
        assert_eq!(result.media.unwrap().width, Some(800));
    }

    #[test]
    fn photo_tags_match_typescript_cases() {
        let raw = FileLike {
            name: Some("photo.dng".into()),
            ..Default::default()
        };
        assert!(get_photo_tags(&raw, "image/x-adobe-dng", None).contains(&PhotoTag::Raw));
        let cr2 = FileLike {
            name: Some("photo.cr2".into()),
            ..Default::default()
        };
        assert!(get_photo_tags(&cr2, "image/jpeg", None).contains(&PhotoTag::Raw));
        let video = FileLike {
            name: Some("video.mp4".into()),
            ..Default::default()
        };
        assert!(get_photo_tags(&video, "video/mp4", None).contains(&PhotoTag::Video));

        let screenshot = FileLike {
            name: Some("screenshot.png".into()),
            ..Default::default()
        };
        let xmp = ExpandedTags {
            xmp: Some(XmpTags {
                user_comment: Some("Screenshot".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(
            get_photo_tags(&screenshot, "image/png", Some(&xmp)).contains(&PhotoTag::Screenshot)
        );

        let filename = FileLike {
            name: Some("Screenshot_20240115.png".into()),
            ..Default::default()
        };
        let empty_xmp = ExpandedTags {
            xmp: Some(XmpTags::default()),
            ..Default::default()
        };
        assert!(
            get_photo_tags(&filename, "image/png", Some(&empty_xmp))
                .contains(&PhotoTag::Screenshot)
        );

        let panorama = ExpandedTags {
            xmp: Some(XmpTags {
                projection_type: Some("equirectangular".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(
            get_photo_tags(
                &FileLike {
                    name: Some("panorama.jpg".into()),
                    ..Default::default()
                },
                "image/jpeg",
                Some(&panorama)
            )
            .contains(&PhotoTag::Panorama)
        );
        let motion = ExpandedTags {
            xmp: Some(XmpTags {
                motion_photo: Some("1".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(
            get_photo_tags(
                &FileLike {
                    name: Some("motion.jpg".into()),
                    ..Default::default()
                },
                "image/jpeg",
                Some(&motion)
            )
            .contains(&PhotoTag::MotionPhoto)
        );
        let portrait = ExpandedTags {
            xmp: Some(XmpTags {
                special_type_id: Some(XmpSpecialType::Text(ANDROID_PORTRAIT.into())),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(
            get_photo_tags(
                &FileLike {
                    name: Some("portrait.jpg".into()),
                    ..Default::default()
                },
                "image/jpeg",
                Some(&portrait)
            )
            .contains(&PhotoTag::Portrait)
        );
        assert!(
            get_photo_tags(
                &FileLike {
                    name: Some("photo.jpg".into()),
                    ..Default::default()
                },
                "image/jpeg",
                None
            )
            .is_empty()
        );
        assert!(
            get_photo_tags(
                &FileLike {
                    name: Some("photo.jpg".into()),
                    ..Default::default()
                },
                "image/jpeg",
                Some(&ExpandedTags {
                    exif: Some(ExifTags::default()),
                    ..Default::default()
                })
            )
            .is_empty()
        );
    }

    #[test]
    fn parse_additional_node_metadata_validates_ranges() {
        assert_eq!(
            parse_additional_node_metadata(&json!({})),
            AdditionalNodeMetadata::default()
        );
        assert_eq!(
            parse_additional_node_metadata(
                &json!({"Location":{"Latitude":48.8566,"Longitude":2.3522}})
            )
            .location
            .unwrap()
            .latitude,
            48.8566
        );
        assert!(
            parse_additional_node_metadata(&json!({"Location":{"Latitude":91,"Longitude":2}}))
                .location
                .is_none()
        );
        assert!(
            parse_additional_node_metadata(&json!({"Location":{"Latitude":48,"Longitude":-181}}))
                .location
                .is_none()
        );
        assert!(
            parse_additional_node_metadata(&json!({"Location":{"Latitude":48,"Longitude":181}}))
                .location
                .is_none()
        );
        assert!(
            parse_additional_node_metadata(&json!({"Location":{"Latitude":48}}))
                .location
                .is_none()
        );
        assert_eq!(
            parse_additional_node_metadata(&json!({"Location":{"Latitude":-90,"Longitude":0}}))
                .location
                .unwrap()
                .latitude,
            -90.0
        );
        assert_eq!(
            parse_additional_node_metadata(&json!({"Camera":{"Orientation":1}}))
                .camera
                .unwrap()
                .orientation,
            Some(1)
        );
        assert!(
            parse_additional_node_metadata(&json!({"Camera":{"Orientation":9}}))
                .camera
                .and_then(|camera| camera.orientation)
                .is_none()
        );
        assert!(
            parse_additional_node_metadata(&json!({"Camera":{"Orientation":0}}))
                .camera
                .and_then(|camera| camera.orientation)
                .is_none()
        );
        for orientation in 1..=8 {
            assert_eq!(
                parse_additional_node_metadata(&json!({"Camera":{"Orientation":orientation}}))
                    .camera
                    .unwrap()
                    .orientation,
                Some(orientation)
            );
        }
        assert!(
            parse_additional_node_metadata(
                &json!({"Camera":{"SubjectCoordinates":{"Top":10,"Left":20,"Bottom":100}}})
            )
            .camera
            .and_then(|camera| camera.subject_coordinates)
            .is_none()
        );
        assert!(
            parse_additional_node_metadata(
                &json!({"Camera":{"SubjectCoordinates":{"Top":10.5,"Left":20,"Bottom":100,"Right":200}}})
            )
            .camera
            .and_then(|camera| camera.subject_coordinates)
            .is_none()
        );
        assert_eq!(
            parse_additional_node_metadata(
                &json!({"Media":{"Width":1920,"Height":1080,"Duration":60.5}})
            )
            .media
            .unwrap()
            .duration,
            Some(60.5)
        );
        assert!(
            parse_additional_node_metadata(&json!({"Media":{"Width":1920.5}}))
                .media
                .and_then(|media| media.width)
                .is_none()
        );
        assert!(
            parse_additional_node_metadata(&json!({"Media":{"Duration":null}}))
                .media
                .is_none()
        );
        assert!(
            parse_additional_node_metadata(&json!({"Camera":{"Orientation":99,"Device":123}}))
                .camera
                .is_none()
        );
        assert_eq!(
            parse_additional_node_metadata(&json!({"Unknown":{"Foo":"bar"}})),
            AdditionalNodeMetadata::default()
        );
    }

    #[test]
    fn get_exif_info_skips_non_images_and_warns_on_loader_errors() {
        let file = FileLike {
            name: Some("document.pdf".into()),
            bytes: b"content".to_vec(),
            ..Default::default()
        };
        let mut called = false;
        let mut loader = |_bytes: &[u8]| {
            called = true;
            Ok(ExpandedTags::default())
        };
        assert!(get_exif_info(&file, "application/pdf", Some(&mut loader), None).is_none());
        assert!(!called);

        let image = FileLike {
            name: Some("photo.jpg".into()),
            bytes: vec![0; 8],
            ..Default::default()
        };
        let mut loader = |_bytes: &[u8]| Ok(ExpandedTags::default());
        assert!(get_exif_info(&image, "image/jpeg", Some(&mut loader), None).is_some());

        let mut warning = None;
        let mut loader = |_bytes: &[u8]| anyhow::bail!("Invalid EXIF data");
        let mut log = |message: String| warning = Some(message);
        assert!(get_exif_info(&image, "image/jpeg", Some(&mut loader), Some(&mut log)).is_none());
        assert_eq!(
            warning.as_deref(),
            Some("Cannot read exif data: Invalid EXIF data")
        );
    }
}
