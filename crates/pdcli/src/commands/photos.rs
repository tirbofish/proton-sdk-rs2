use crate::app::AppState;
use crate::index::IndexEntry;

/// `mkdir <name>` creates a new album in the Photos section.
pub async fn mkdir(args: &[String], state: &AppState) -> anyhow::Result<()> {
    let name = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("mkdir: missing album name"))?
        .clone();

    let pb = crate::ui::spinner(format!("Creating album \"{}\"…", name));
    let uid = match state.photos.create_album(name.clone()).await {
        Ok(u) => { pb.finish_and_clear(); u }
        Err(e) => { pb.finish_and_clear(); return Err(e); }
    };
    state.index.insert(IndexEntry {
        uid: uid.clone(),
        parent_uid: Some(state.photos_root_uid.clone()),
        name,
        is_folder: true,
        size: None,
        modification_time: None,
        media_type: None,
    });
    println!("Album created (uid={})", uid);
    Ok(())
}
