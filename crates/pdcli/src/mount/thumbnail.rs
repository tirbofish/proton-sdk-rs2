use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Configuration for thumbnail/preview process detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ThumbnailConfig {
    #[serde(default)]
    pub allowed_exes: Vec<String>,
    #[serde(default)]
    pub blocked_exes: Vec<String>,
    #[serde(default)]
    pub allowed_names: Vec<String>,
    #[serde(default)]
    pub blocked_names: Vec<String>,
}

impl Default for ThumbnailConfig {
    fn default() -> Self {
        Self {
            allowed_exes: vec![
                "/papers".to_string(),
                "/evince".to_string(),
                "/okular".to_string(),
                "/zathura".to_string(),
                "/gedit".to_string(),
                "/gnome-text-editor".to_string(),
                "/kate".to_string(),
                "/kwrite".to_string(),
                "/mousepad".to_string(),
                "/pluma".to_string(),
                "/xed".to_string(),
                "/loupe".to_string(),
                "/eog".to_string(),
                "/gwenview".to_string(),
                "/feh".to_string(),
                "/sxiv".to_string(),
                "/totem".to_string(),
                "/vlc".to_string(),
                "/mpv".to_string(),
                "/celluloid".to_string(),
                "/firefox".to_string(),
                "/chromium".to_string(),
                "/chrome".to_string(),
                "/brave".to_string(),
                "/libreoffice".to_string(),
                "/soffice".to_string(),
                "/code".to_string(),
                "/codium".to_string(),
                "/sublime_text".to_string(),
                "/atom".to_string(),
            ],
            blocked_exes: vec![
                "/nautilus".to_string(),
                "/dolphin".to_string(),
                "/thunar".to_string(),
                "/nemo".to_string(),
                "/pcmanfm".to_string(),
                "/caja".to_string(),
                "/spacefm".to_string(),
                "/libexec/".to_string(),
            ],
            allowed_names: vec![
                "papers".to_string(),
                "evince".to_string(),
                "gedit".to_string(),
                "gnome-text-ed".to_string(),
                "loupe".to_string(),
                "eog".to_string(),
                "totem".to_string(),
                "vlc".to_string(),
                "mpv".to_string(),
                "firefox".to_string(),
                "chromium".to_string(),
                "chrome".to_string(),
                "libreoffice".to_string(),
                "soffice".to_string(),
                "lowriter".to_string(),
                "localc".to_string(),
                "code".to_string(),
                "codium".to_string(),
            ],
            blocked_names: vec![
                "nautilus".to_string(),
                "dolphin".to_string(),
                "thunar".to_string(),
                "nemo".to_string(),
                "pcmanfm".to_string(),
                "caja".to_string(),
                "tumbler".to_string(),
                "tumblerd".to_string(),
                "tracker-extract".to_string(),
                "tracker-miner-fs".to_string(),
                "tracker".to_string(),
            ],
        }
    }
}

impl ThumbnailConfig {
    /// Load from ~/.config/pdcli/thumbnails.toml, creating defaults if missing.
    pub fn load() -> Self {
        let config_path = Self::config_path();

        if config_path.exists() {
            match std::fs::read_to_string(&config_path) {
                Ok(contents) => match toml::from_str(&contents) {
                    Ok(config) => {
                        tracing::debug!("Loaded thumbnail config from {:?}", config_path);
                        return config;
                    }
                    Err(e) => {
                        tracing::warn!("Failed to parse thumbnail config: {}. Using defaults.", e);
                    }
                },
                Err(e) => {
                    tracing::warn!("Failed to read thumbnail config: {}. Using defaults.", e);
                }
            }
        } else {
            let default_config = Self::default();
            if let Err(e) = default_config.save() {
                tracing::warn!("Failed to create default thumbnail config: {}", e);
            } else {
                tracing::info!("Created default thumbnail config at {:?}", config_path);
            }
            return default_config;
        }

        Self::default()
    }

    pub fn save(&self) -> Result<()> {
        let config_path = Self::config_path();

        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let contents = toml::to_string_pretty(self)?;
        let with_comments = format!(
            "# pdcli Thumbnail Configuration\n#\n# Patterns are matched with contains().\n\n{}",
            contents
        );

        std::fs::write(&config_path, with_comments)?;
        Ok(())
    }

    fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("pdcli")
            .join("thumbnails.toml")
    }

    pub fn is_exe_allowed(&self, exe: &str) -> Option<bool> {
        let exe_lower = exe.to_lowercase();

        for pattern in &self.allowed_exes {
            if exe_lower.contains(&pattern.to_lowercase()) {
                return Some(true);
            }
        }

        for pattern in &self.blocked_exes {
            if exe_lower.contains(&pattern.to_lowercase()) {
                return Some(false);
            }
        }

        None
    }

    pub fn is_name_allowed(&self, name: &str) -> Option<bool> {
        let name_lower = name.to_lowercase();

        for pattern in &self.allowed_names {
            let pattern_lower = pattern.to_lowercase();
            if name_lower.starts_with(&pattern_lower) || name_lower.contains(&pattern_lower) {
                return Some(true);
            }
        }

        for pattern in &self.blocked_names {
            let pattern_lower = pattern.to_lowercase();
            if name_lower.starts_with(&pattern_lower) || name_lower.contains(&pattern_lower) {
                return Some(false);
            }
        }

        None
    }
}

static THUMBNAIL_CONFIG: std::sync::OnceLock<ThumbnailConfig> = std::sync::OnceLock::new();

pub(super) fn get_thumbnail_config() -> &'static ThumbnailConfig {
    THUMBNAIL_CONFIG.get_or_init(ThumbnailConfig::load)
}

/// Maximum age for a cached thumbnail to be considered valid.
const THUMBNAIL_CACHE_MAX_AGE: Duration = Duration::from_secs(24 * 3600);

/// Check if a valid freedesktop.org thumbnail already exists for the given file.
/// Returns true if both "normal" and "large" thumbnails exist and are recent enough.
pub(super) fn has_cached_thumbnail(file_path: &Path, _mtime_secs: i64) -> bool {
    let file_uri = path_to_file_uri(file_path);
    let uri_md5 = format!("{:x}", md5::compute(file_uri.as_bytes()));

    let Some(cache_dir) = dirs::cache_dir() else {
        return false;
    };
    let cache_dir = cache_dir.join("thumbnails");

    // Check both sizes - if either is missing or stale, we should refresh.
    for size_name in ["large", "normal"] {
        let thumb_path = cache_dir.join(size_name).join(format!("{}.png", uri_md5));

        if !thumb_path.exists() {
            return false;
        }

        // Check if the thumbnail is recent enough.
        if let Ok(meta) = std::fs::metadata(&thumb_path) {
            if let Ok(modified) = meta.modified() {
                if modified.elapsed().unwrap_or(Duration::MAX) > THUMBNAIL_CACHE_MAX_AGE {
                    return false;
                }
            } else {
                return false;
            }
        } else {
            return false;
        }
    }

    tracing::trace!("Thumbnail cache hit for {:?}", file_path);
    true
}

/// Check if a filename is a LibreOffice/OpenOffice lock file.
/// Lock files have the pattern `.~lock.<filename>#`.
pub(super) fn is_lock_file(filename: &str) -> bool {
    filename.starts_with(".~lock.") && filename.ends_with('#')
}

/// Plant a Proton Drive thumbnail into the freedesktop.org thumbnail cache.
/// Nautilus checks `~/.cache/thumbnails/large/` before trying to generate thumbnails.
/// If a valid thumbnail exists there with correct Thumb::URI and Thumb::MTime metadata,
/// Nautilus will use it and never attempt to open the file for thumbnail generation.
pub(super) fn plant_freedesktop_thumbnail(
    file_path: &Path,
    mtime_secs: i64,
    thumbnail_data: &[u8],
) -> Result<()> {
    use png::{BitDepth, ColorType, Encoder};
    use std::io::BufWriter;

    let file_uri = path_to_file_uri(file_path);
    tracing::info!("Planting thumbnail for URI: {}", file_uri);

    // Compute MD5 of the URI - this is the cache key per freedesktop spec.
    let uri_md5 = format!("{:x}", md5::compute(file_uri.as_bytes()));

    let cache_dir = dirs::cache_dir()
        .context("Could not find cache directory")?
        .join("thumbnails");

    // Plant in both sizes because viewers may check different buckets.
    for (size_name, max_size) in [("large", 256u32), ("normal", 128u32)] {
        let thumb_dir = cache_dir.join(size_name);
        std::fs::create_dir_all(&thumb_dir)
            .context("Failed to create thumbnail cache directory")?;

        let thumb_path = thumb_dir.join(format!("{}.png", uri_md5));

        if thumb_path.exists() {
            if let Ok(meta) = std::fs::metadata(&thumb_path) {
                if let Ok(modified) = meta.modified() {
                    if modified.elapsed().unwrap_or(Duration::MAX) < Duration::from_secs(3600) {
                        tracing::trace!("Thumbnail already exists and is recent: {:?}", thumb_path);
                        continue;
                    }
                }
            }
        }

        let img = image::load_from_memory(thumbnail_data)
            .context("Failed to decode thumbnail image")?;

        let thumbnail = if img.width() > max_size || img.height() > max_size {
            img.thumbnail(max_size, max_size)
        } else {
            img.clone()
        };

        let rgba = thumbnail.to_rgba8();
        let (width, height) = rgba.dimensions();

        let file = std::fs::File::create(&thumb_path)
            .context("Failed to create thumbnail file")?;
        let writer = BufWriter::new(file);

        let mut encoder = Encoder::new(writer, width, height);
        encoder.set_color(ColorType::Rgba);
        encoder.set_depth(BitDepth::Eight);
        encoder.add_text_chunk("Thumb::URI".to_string(), file_uri.clone())?;
        encoder.add_text_chunk("Thumb::MTime".to_string(), mtime_secs.to_string())?;

        let mut writer = encoder.write_header()
            .context("Failed to write PNG header")?;
        writer.write_image_data(&rgba)
            .context("Failed to write PNG data")?;
        writer.finish().context("Failed to finish PNG")?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&thumb_path, std::fs::Permissions::from_mode(0o600));
        }

        tracing::info!("Planted {} thumbnail: {:?}", size_name, thumb_path);
    }

    Ok(())
}

/// Convert a file path to a file:// URI.
fn path_to_file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}
