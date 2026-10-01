//! Choose accessible storage before editing, and preserve documents from older versions.
use crate::{note::default_note_path, sandbox, settings::{self, Settings}};
use std::{io, path::Path};

pub fn ensure() -> bool {
    let mut settings = Settings::load(&settings::settings_path());
    if let Some(folder) = &settings.note_folder {
        let folder = sandbox::restore(folder);
        // A disconnected volume remains the selected location; the existing UI
        // handles it read-only instead of silently reverting to hidden storage.
        if !sandbox::private_document_location(&folder) { return true; }
    }
    loop {
        let Some(folder) = sandbox::choose_document_folder(
            "Choose a folder for your notes. Your note.md, images and backups will save here automatically. Existing notes will be copied here; the originals will stay safe.") else { return false; };
        let result = (|| -> io::Result<()> {
            sandbox::check_document_folder(&folder)?;
            let old = settings.note_folder.clone().unwrap_or_else(|| default_note_path().parent().unwrap().to_path_buf());
            migrate(&old, &folder)?;
            sandbox::remember(&folder).map_err(io::Error::other)?;
            let previous = settings.note_folder.replace(folder.clone());
            if let Err(error) = settings.save(&settings::settings_path()) {
                settings.note_folder = previous;
                return Err(error);
            }
            Ok(())
        })();
        match result { Ok(()) => return true, Err(error) => sandbox::location_error(&error.to_string()) }
    }
}

fn migrate(old: &Path, chosen: &Path) -> io::Result<()> {
    for name in ["note.md", "images", "backups"] {
        sandbox::copy_preserving(&old.join(name), &chosen.join(name))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migration_copies_notes_images_and_backups_without_overwriting() {
        let base = std::env::temp_dir().join(format!("gravitynote-docs-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let old = base.join("private"); let chosen = base.join("Documents");
        std::fs::create_dir_all(old.join("images")).unwrap();
        std::fs::create_dir_all(old.join("backups")).unwrap(); std::fs::create_dir_all(&chosen).unwrap();
        for name in ["note.md", "images/photo.png", "backups/earlier.md"] {
            std::fs::write(old.join(name), name).unwrap();
        }
        std::fs::write(old.join("settings.txt"), "private preferences").unwrap();
        migrate(&old, &chosen).unwrap();
        for name in ["note.md", "images/photo.png", "backups/earlier.md"] {
            assert_eq!(std::fs::read(chosen.join(name)).unwrap(), std::fs::read(old.join(name)).unwrap());
        }
        assert!(!chosen.join("settings.txt").exists());
        std::fs::write(chosen.join("note.md"), "different existing notes").unwrap();
        assert_eq!(migrate(&old, &chosen).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(chosen.join("note.md")).unwrap(), "different existing notes");
        std::fs::remove_dir_all(base).unwrap();
    }
}
