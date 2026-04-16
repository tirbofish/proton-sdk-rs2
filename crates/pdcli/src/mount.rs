use std::sync::Arc;

use fuse3::MountOptions;

use crate::fs::ProtonDriveFS;
use crate::index::DriveIndex;

pub fn default_mount_path() -> std::path::PathBuf {
    dirs::home_dir()
        .expect("cannot determine home directory")
        .join("ProtonDrive")
}

pub async fn mount(
    mount_path: std::path::PathBuf,
    index: Arc<DriveIndex>,
) -> anyhow::Result<fuse3::raw::MountHandle> {
    std::fs::create_dir_all(&mount_path)?;

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    let mut mount_options = MountOptions::default();
    mount_options.uid(uid).gid(gid).read_only(true);

    tracing::info!(path = %mount_path.display(), "mounting FUSE filesystem");

    let fs = ProtonDriveFS::new(index);
    let mount_handle = fuse3::raw::Session::new(mount_options)
        .mount_with_unprivileged(fs, &mount_path)
        .await?;

    Ok(mount_handle)
}