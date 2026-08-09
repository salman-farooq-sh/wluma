use anyhow::{Context, Result};
use std::fs::{self, OpenOptions};
use std::io;
use std::path::Path;

pub fn migrate() -> Result<bool> {
    let directories = xdg::BaseDirectories::with_prefix("wluma");
    let Some(source) = directories.get_data_home() else {
        return Ok(false);
    };
    if !source.is_dir() {
        return Ok(false);
    }

    let destination = directories.create_state_directory("")?;
    migrate_directory(&source, &destination)
}

fn migrate_directory(source: &Path, destination: &Path) -> Result<bool> {
    let mut migrated = false;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }

        let source_file = entry.path();
        let destination_file = destination.join(entry.file_name());
        if destination_file.exists() {
            continue;
        }

        if fs::rename(&source_file, &destination_file).is_ok() {
            migrated = true;
            continue;
        }

        let mut input = fs::File::open(&source_file)?;
        let mut output = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination_file)
        {
            Ok(output) => output,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        };
        if let Err(error) = io::copy(&mut input, &mut output) {
            drop(output);
            let _ = fs::remove_file(&destination_file);
            return Err(error.into());
        }
        fs::remove_file(&source_file).with_context(|| {
            format!(
                "Unable to remove migrated state file {}",
                source_file.display()
            )
        })?;
        migrated = true;
    }

    Ok(migrated)
}

#[cfg(test)]
mod tests {
    use super::migrate_directory;
    use std::fs;

    #[test]
    fn migrates_files_without_overwriting_existing_state() {
        let root = std::env::temp_dir().join(format!(
            "wluma-state-migration-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let source = root.join("data");
        let destination = root.join("state");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(source.join("migrate.yaml"), "old").unwrap();
        fs::write(source.join("existing.yaml"), "old").unwrap();
        fs::write(destination.join("existing.yaml"), "new").unwrap();

        assert!(migrate_directory(&source, &destination).unwrap());

        assert_eq!(
            fs::read_to_string(destination.join("migrate.yaml")).unwrap(),
            "old"
        );
        assert!(!source.join("migrate.yaml").exists());
        assert_eq!(
            fs::read_to_string(destination.join("existing.yaml")).unwrap(),
            "new"
        );
        assert_eq!(
            fs::read_to_string(source.join("existing.yaml")).unwrap(),
            "old"
        );
        assert!(!migrate_directory(&source, &destination).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
}
