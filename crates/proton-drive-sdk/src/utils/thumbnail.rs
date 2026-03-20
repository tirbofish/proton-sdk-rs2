use image::GenericImageView;
use std::io::Cursor;
use crate::node::thumbnail::{Thumbnail, ThumbnailType};
use crate::api::attr::MediaExtendedAttributes;

pub struct ThumbnailGenerator;

impl ThumbnailGenerator {
    pub fn generate_thumbnails(data: &[u8]) -> (Vec<Thumbnail>, Option<MediaExtendedAttributes>) {
        let Ok(img) = image::load_from_memory(data) else {
            return (Vec::new(), None);
        };

        let (width, height) = img.dimensions();
        let media_info = Some(MediaExtendedAttributes {
            width: Some(width),
            height: Some(height),
            duration: None,
        });

        let mut thumbnails = Vec::new();

        // Type 1: Small thumbnail (e.g. 256x256)
        if let Some(t1) = Self::generate_thumbnail(&img, 256, ThumbnailType::Thumbnail) {
            thumbnails.push(t1);
        }

        // Type 2: Large preview (e.g. 1024x1024)
        if let Some(t2) = Self::generate_thumbnail(&img, 1024, ThumbnailType::Preview) {
            thumbnails.push(t2);
        }

        (thumbnails, media_info)
    }

    fn generate_thumbnail(img: &image::DynamicImage, size: u32, r#type: ThumbnailType) -> Option<Thumbnail> {
        let thumb = img.thumbnail(size, size);
        let mut buffer = Cursor::new(Vec::new());
        // Use JPEG for thumbnails to keep size small
        if thumb.write_to(&mut buffer, image::ImageFormat::Jpeg).is_ok() {
            Some(Thumbnail {
                r#type,
                content: buffer.into_inner(),
            })
        } else {
            None
        }
    }
}
