//! Opening the shortcut files the mount shows for Proton Docs and Sheets, and
//! registering their file type, icons and opener with the desktop.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use serde::Deserialize;

const MIME_XML: &str = include_str!("../../../packaging/mime/io.github.tirbofish.pdcli.xml");
const OPENER_DESKTOP: &str =
    include_str!("../../../packaging/io.github.tirbofish.pdcli.open-document.desktop");
const OPENER_DESKTOP_NAME: &str = "io.github.tirbofish.pdcli.open-document.desktop";
const ICONS: [(&str, &str); 2] = [
    (
        "application-vnd.proton.doc.svg",
        include_str!(
            "../../../packaging/icons/hicolor/scalable/mimetypes/application-vnd.proton.doc.svg"
        ),
    ),
    (
        "application-vnd.proton.sheet.svg",
        include_str!(
            "../../../packaging/icons/hicolor/scalable/mimetypes/application-vnd.proton.sheet.svg"
        ),
    ),
];

#[derive(Deserialize)]
struct Shortcut {
    url: String,
}

/// The editor URL in a shortcut, refusing anything but Proton Docs so a crafted
/// file cannot make this open arbitrary links.
fn shortcut_url(contents: &[u8]) -> anyhow::Result<String> {
    let shortcut: Shortcut =
        serde_json::from_slice(contents).context("not a Proton Docs or Sheets shortcut")?;
    anyhow::ensure!(
        shortcut.url.starts_with("https://docs.proton.me/"),
        "shortcut does not point to Proton Docs"
    );
    Ok(shortcut.url)
}

pub fn open(path: &Path) -> anyhow::Result<()> {
    let contents = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let url = shortcut_url(&contents)?;
    Command::new("xdg-open")
        .arg(&url)
        .spawn()
        .context("failed to run xdg-open")?;
    println!("{url}");
    Ok(())
}

/// For source installs; packages install the same files system-wide.
pub fn install() -> anyhow::Result<()> {
    let data = dirs::data_dir().context("no XDG data directory")?;
    let executable = std::env::current_exe().context("failed to resolve pdcli executable")?;

    write(
        &data.join("mime/packages/io.github.tirbofish.pdcli.xml"),
        MIME_XML,
    )?;
    let icons = data.join("icons/hicolor/scalable/mimetypes");
    for (name, svg) in ICONS {
        write(&icons.join(name), svg)?;
    }
    let desktop = OPENER_DESKTOP.replace(
        "Exec=pdcli ",
        &format!("Exec={} ", desktop_quote(&executable)),
    );
    write(
        &data.join("applications").join(OPENER_DESKTOP_NAME),
        &desktop,
    )?;

    // Refreshing caches is best effort: the desktop picks changes up eventually.
    for (tool, arg) in [
        ("update-mime-database", data.join("mime")),
        ("update-desktop-database", data.join("applications")),
        ("gtk-update-icon-cache", data.join("icons/hicolor")),
    ] {
        // A per-user hicolor folder has no index.theme, which the icon cache warns about.
        let _ = Command::new(tool)
            .arg(arg)
            .stderr(std::process::Stdio::null())
            .status();
    }
    for mime in ["application/vnd.proton.doc", "application/vnd.proton.sheet"] {
        let _ = Command::new("xdg-mime")
            .args(["default", OPENER_DESKTOP_NAME, mime])
            .status();
    }
    println!(
        "installed Proton Docs and Sheets integration in {}",
        data.display()
    );
    Ok(())
}

fn write(path: &PathBuf, contents: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents).with_context(|| format!("write {}", path.display()))
}

/// Quotes an Exec= program path per the Desktop Entry specification.
fn desktop_quote(path: &Path) -> String {
    let escaped = path
        .to_string_lossy()
        .replace('\\', "\\\\\\\\")
        .replace('"', "\\\\\"")
        .replace('`', "\\\\`")
        .replace('$', "\\\\$")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::shortcut_url;

    #[test]
    fn shortcuts_open_only_proton_docs_urls() {
        assert_eq!(
            shortcut_url(br#"{"proton_drive_document":"doc","url":"https://docs.proton.me/doc?type=doc","uid":"v~l"}"#)
                .unwrap(),
            "https://docs.proton.me/doc?type=doc"
        );
        assert!(shortcut_url(br#"{"url":"https://evil.example/"}"#).is_err());
        assert!(shortcut_url(b"not json").is_err());
    }
}
