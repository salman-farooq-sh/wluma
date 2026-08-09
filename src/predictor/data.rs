use crate::als::Scale;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq, Serialize, Clone)]
pub struct Data {
    pub output_name: String,
    pub entries: Vec<Entry>,
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
}

#[derive(Deserialize)]
struct StoredEntry {
    #[serde(alias = "lux")]
    als: StoredAls,
    luma: u8,
    brightness: u64,
}

impl StoredEntry {
    fn migrate(self, thresholds: &HashMap<u64, String>, scale: Scale) -> (Option<Entry>, bool) {
        let als = match self.als {
            StoredAls::Value(value) => {
                return (Some(Entry::new(value, self.luma, self.brightness)), false)
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
                    return (None, true);
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
        (Some(Entry::new(als, self.luma, self.brightness)), true)
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

        let mut migrated = false;
        let entries = stored
            .entries
            .into_iter()
            .filter_map(|entry| {
                let (entry, entry_migrated) = entry.migrate(thresholds, scale);
                migrated |= entry_migrated;
                entry
            })
            .collect();
        let data = Self {
            output_name: output_name.to_string(),
            entries,
        };
        if migrated {
            data.save().expect("Unable to save migrated learned data");
        }
        data
    }

    pub fn save(&self) -> Result<()> {
        Ok(serde_yaml::to_writer(self.write_file()?, self)?)
    }

    fn read_file(path: PathBuf) -> Result<File> {
        Ok(File::open(path)?)
    }

    fn write_file(&self) -> Result<File> {
        let path = Self::path(&self.output_name)?;
        Ok(OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?)
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
        let (entry, migrated) = stored
            .entries
            .into_iter()
            .next()
            .unwrap()
            .migrate(&thresholds, Scale::Linear);
        assert!(migrated);
        assert_eq!(Some(Entry::new(15, 20, 30)), entry);
    }

    #[test]
    fn accepts_numeric_als() {
        let stored: StoredData = serde_yaml::from_str(
            "output_name: panel\nentries:\n  - als: 42\n    luma: 20\n    brightness: 30\n",
        )
        .unwrap();
        let (entry, migrated) = stored
            .entries
            .into_iter()
            .next()
            .unwrap()
            .migrate(&HashMap::new(), Scale::Linear);
        assert!(!migrated);
        assert_eq!(Some(Entry::new(42, 20, 30)), entry);
    }

    #[test]
    fn drops_unknown_profiles_as_migrated() {
        let stored: StoredData = serde_yaml::from_str(
            "output_name: panel\nentries:\n  - lux: unknown\n    luma: 20\n    brightness: 30\n",
        )
        .unwrap();
        let (entry, migrated) = stored
            .entries
            .into_iter()
            .next()
            .unwrap()
            .migrate(&HashMap::new(), Scale::Linear);
        assert!(migrated);
        assert_eq!(None, entry);
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
}
