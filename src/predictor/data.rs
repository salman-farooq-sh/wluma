use crate::als::Scale;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq, Serialize, Clone)]
pub struct Data {
    pub output_name: String,
    pub entries: Vec<Entry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    legacy_entries_v1: Option<Vec<LegacyEntryV1>>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, Clone)]
struct LegacyEntryV1 {
    lux: String,
    luma: u8,
    brightness: u64,
}

#[derive(Debug, PartialEq, Eq, Hash, Serialize, Clone)]
pub struct Entry {
    pub als: u64,
    pub luma: u8,
    pub brightness: u64,
}

#[derive(Deserialize)]
struct StoredData {
    output_name: String,
    entries: Vec<StoredEntry>,
    #[serde(default)]
    legacy_entries_v1: Option<Vec<LegacyEntryV1>>,
}

#[derive(Deserialize)]
struct StoredEntry {
    #[serde(alias = "lux")]
    als: StoredAls,
    luma: u8,
    brightness: u64,
}

impl StoredEntry {
    fn migrate(
        self,
        thresholds: &HashMap<u64, String>,
        scale: Scale,
    ) -> (Option<Entry>, Option<LegacyEntryV1>) {
        let legacy = match &self.als {
            StoredAls::Value(_) => None,
            StoredAls::Profile(profile) => Some(LegacyEntryV1 {
                lux: profile.clone(),
                luma: self.luma,
                brightness: self.brightness,
            }),
        };
        let als = match self.als {
            StoredAls::Value(value) => {
                return (Some(Entry::new(value, self.luma, self.brightness)), legacy)
            }
            StoredAls::Profile(profile) if profile == "none" => 0,
            StoredAls::Profile(profile) => {
                let mut thresholds = thresholds.iter().collect::<Vec<_>>();
                thresholds.sort_unstable_by_key(|(value, _)| **value);
                let Some(index) = thresholds
                    .iter()
                    .position(|(_, name)| name.as_str() == profile)
                else {
                    log::warn!("Dropping learned data with unknown ALS profile '{profile}'");
                    return (None, legacy);
                };
                let lower = scale.coordinate(*thresholds[index].0);
                let upper = thresholds
                    .get(index + 1)
                    .map(|(value, _)| scale.coordinate(**value))
                    .or_else(|| {
                        index.checked_sub(1).map(|previous| {
                            lower + lower - scale.coordinate(*thresholds[previous].0)
                        })
                    })
                    .unwrap_or(lower);
                scale.value((lower + upper) / 2.0)
            }
        };
        (Some(Entry::new(als, self.luma, self.brightness)), legacy)
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredAls {
    Value(u64),
    Profile(String),
}

impl Data {
    pub fn new(output_name: &str) -> Self {
        Self {
            output_name: output_name.to_string(),
            entries: Vec::new(),
            legacy_entries_v1: None,
        }
    }

    pub fn load(output_name: &str, thresholds: &HashMap<u64, String>, scale: Scale) -> Self {
        let path = match Self::path(output_name) {
            Ok(path) if path.exists() => path,
            Ok(_) => return Self::new(output_name),
            Err(error) => {
                log::warn!("Unable to locate learned data for '{output_name}': {error}");
                return Self::new(output_name);
            }
        };
        let stored = match Self::read_file(path)
            .and_then(|file| serde_yaml::from_reader::<_, StoredData>(file).map_err(Into::into))
        {
            Ok(stored) => stored,
            Err(error) => {
                log::warn!("Unable to load learned data for '{output_name}': {error}");
                return Self::new(output_name);
            }
        };
        if stored.output_name != output_name {
            log::warn!(
                "Learned data for '{output_name}' contains output name '{}'",
                stored.output_name
            );
        }

        let (data, migrated) = Self::from_stored(output_name, stored, thresholds, scale);
        if migrated {
            data.save().expect("Unable to save migrated learned data");
        }
        data
    }

    fn from_stored(
        output_name: &str,
        stored: StoredData,
        thresholds: &HashMap<u64, String>,
        scale: Scale,
    ) -> (Self, bool) {
        let mut migrated = false;
        let mut legacy_entries_v1 = stored.legacy_entries_v1;
        let entries = stored
            .entries
            .into_iter()
            .filter_map(|entry| {
                let (entry, legacy) = entry.migrate(thresholds, scale);
                if let Some(legacy) = legacy {
                    migrated = true;
                    legacy_entries_v1.get_or_insert_with(Vec::new).push(legacy);
                }
                entry
            })
            .collect();
        (
            Self {
                output_name: output_name.to_string(),
                entries,
                legacy_entries_v1,
            },
            migrated,
        )
    }

    pub fn save(&self) -> Result<()> {
        Self::save_to_path(self, &Self::path(&self.output_name)?)
    }

    fn save_to_path(&self, path: &Path) -> Result<()> {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state");
        let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&temporary)?;
            serde_yaml::to_writer(&mut file, self)?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            if let Some(parent) = path.parent() {
                File::open(parent)?.sync_all()?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn read_file(path: PathBuf) -> Result<File> {
        Ok(File::open(path)?)
    }

    fn path(output_name: &str) -> Result<PathBuf> {
        Ok(xdg::BaseDirectories::with_prefix("wluma")
            .create_state_directory("")?
            .join(format!("{output_name}.yaml")))
    }
}

impl Entry {
    pub fn new(als: u64, luma: u8, brightness: u64) -> Self {
        Self {
            als,
            luma,
            brightness,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_profile_to_bucket_midpoint() {
        let stored: StoredData = serde_yaml::from_str(
            "output_name: panel\nentries:\n  - lux: dark\n    luma: 20\n    brightness: 30\n",
        )
        .unwrap();
        let thresholds = [(10, "dark".to_string()), (20, "dark".to_string())]
            .into_iter()
            .collect();
        let (entry, legacy) = stored
            .entries
            .into_iter()
            .next()
            .unwrap()
            .migrate(&thresholds, Scale::Linear);
        assert_eq!(Some(Entry::new(15, 20, 30)), entry);
        assert_eq!(
            Some(LegacyEntryV1 {
                lux: "dark".to_string(),
                luma: 20,
                brightness: 30,
            }),
            legacy
        );
    }

    #[test]
    fn accepts_numeric_als() {
        let stored: StoredData = serde_yaml::from_str(
            "output_name: panel\nentries:\n  - als: 42\n    luma: 20\n    brightness: 30\n",
        )
        .unwrap();
        let (entry, legacy) = stored
            .entries
            .into_iter()
            .next()
            .unwrap()
            .migrate(&HashMap::new(), Scale::Linear);
        assert_eq!(Some(Entry::new(42, 20, 30)), entry);
        assert_eq!(None, legacy);
    }

    #[test]
    fn drops_unknown_profiles_as_migrated() {
        let stored: StoredData = serde_yaml::from_str(
            "output_name: panel\nentries:\n  - lux: unknown\n    luma: 20\n    brightness: 30\n",
        )
        .unwrap();
        let (entry, legacy) = stored
            .entries
            .into_iter()
            .next()
            .unwrap()
            .migrate(&HashMap::new(), Scale::Linear);
        assert_eq!(None, entry);
        assert_eq!(
            Some(LegacyEntryV1 {
                lux: "unknown".to_string(),
                luma: 20,
                brightness: 30,
            }),
            legacy
        );
    }

    #[test]
    fn keeps_linear_migration_in_native_domain() {
        let stored: StoredData = serde_yaml::from_str(
            "output_name: panel\nentries:\n  - lux: bright\n    luma: 20\n    brightness: 30\n",
        )
        .unwrap();
        let thresholds = [(80, "bright".to_string()), (20, "dim".to_string())]
            .into_iter()
            .collect();
        let (entry, _) = stored
            .entries
            .into_iter()
            .next()
            .unwrap()
            .migrate(&thresholds, Scale::Linear);
        assert_eq!(Some(Entry::new(100, 20, 30)), entry);
    }

    #[test]
    fn preserves_legacy_entries_across_reloads() {
        let stored = serde_yaml::from_str(
            "output_name: panel\nentries:\n  - lux: dark\n    luma: 20\n    brightness: 30\n",
        )
        .unwrap();
        let thresholds = [(10, "dark".to_string()), (20, "bright".to_string())]
            .into_iter()
            .collect();

        let (migrated, was_migrated) =
            Data::from_stored("panel", stored, &thresholds, Scale::Linear);
        assert!(was_migrated);
        assert_eq!(vec![Entry::new(15, 20, 30)], migrated.entries);

        let stored = serde_yaml::from_str(&serde_yaml::to_string(&migrated).unwrap()).unwrap();
        let (reloaded, was_migrated) =
            Data::from_stored("panel", stored, &thresholds, Scale::Linear);
        assert!(!was_migrated);
        assert_eq!(migrated, reloaded);
    }

    #[test]
    fn omits_legacy_field_for_new_data() {
        let yaml = serde_yaml::to_string(&Data::new("panel")).unwrap();
        assert!(!yaml.contains("legacy_entries_v1"));
    }
}
