//! Shared application identity used by the daemon and desktop client.

use std::path::Path;

#[cfg(debug_assertions)]
pub const APP_NAME: &str = "Dinosaur Debug";
#[cfg(not(debug_assertions))]
pub const APP_NAME: &str = "Dinosaur";

#[cfg(debug_assertions)]
pub const APP_ID: &str = "sh.dinosaur.dev";
#[cfg(not(debug_assertions))]
pub const APP_ID: &str = "sh.dinosaur";

#[cfg(debug_assertions)]
pub const DATA_DIRECTORY_NAME: &str = "Dinosaur Debug";
#[cfg(not(debug_assertions))]
pub const DATA_DIRECTORY_NAME: &str = "Dinosaur";

/// The data directory name used before the app was renamed to Dinosaur.
#[cfg(debug_assertions)]
pub const LEGACY_DATA_DIRECTORY_NAME: &str = "Waku Debug";
#[cfg(not(debug_assertions))]
pub const LEGACY_DATA_DIRECTORY_NAME: &str = "Waku";

/// Moves the pre-rename per-user data directory to its new name so existing
/// sessions, settings, and blobs survive the rename. It only runs when the old
/// directory exists and the new one does not, so it is a no-op after the first
/// launch and never merges or overwrites. Call it before anything resolves a
/// path under [`DATA_DIRECTORY_NAME`].
pub fn migrate_legacy_data_directory() {
    for parent in [dirs::data_local_dir(), dirs::data_dir()]
        .into_iter()
        .flatten()
    {
        migrate_legacy_data_directory_in(&parent);
    }
}

fn migrate_legacy_data_directory_in(parent: &Path) {
    let legacy = parent.join(LEGACY_DATA_DIRECTORY_NAME);
    let current = parent.join(DATA_DIRECTORY_NAME);
    if current.exists() || !legacy.is_dir() {
        return;
    }
    if let Err(error) = std::fs::rename(&legacy, &current) {
        eprintln!(
            "could not move {} to {}: {error}",
            legacy.display(),
            current.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_data_directory_moves_once_without_overwriting() {
        let root = std::env::temp_dir().join(format!("dinosaur-identity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(LEGACY_DATA_DIRECTORY_NAME)).unwrap();
        std::fs::write(root.join(LEGACY_DATA_DIRECTORY_NAME).join("app.db"), b"old").unwrap();

        migrate_legacy_data_directory_in(&root);
        assert_eq!(
            std::fs::read(root.join(DATA_DIRECTORY_NAME).join("app.db")).unwrap(),
            b"old"
        );
        assert!(!root.join(LEGACY_DATA_DIRECTORY_NAME).exists());

        std::fs::create_dir_all(root.join(LEGACY_DATA_DIRECTORY_NAME)).unwrap();
        std::fs::write(root.join(LEGACY_DATA_DIRECTORY_NAME).join("app.db"), b"stale").unwrap();
        migrate_legacy_data_directory_in(&root);
        assert_eq!(
            std::fs::read(root.join(DATA_DIRECTORY_NAME).join("app.db")).unwrap(),
            b"old"
        );

        std::fs::remove_dir_all(root).unwrap();
    }
}
