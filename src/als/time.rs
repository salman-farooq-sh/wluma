use anyhow::{anyhow, Result};
use chrono::{Local, Timelike};
use std::collections::HashMap;

pub struct Als {
    levels: Vec<(u64, u64)>,
}

impl Als {
    pub fn new(levels: HashMap<u64, u64>) -> Result<Self> {
        if levels.is_empty() {
            return Err(anyhow!("Time ALS requires at least one level"));
        }
        if levels.keys().any(|hour| *hour >= 24) {
            return Err(anyhow!("Time ALS hours must be between 0 and 23"));
        }
        let mut levels = levels.into_iter().collect::<Vec<_>>();
        levels.sort_unstable_by_key(|(hour, _)| *hour);
        Ok(Self { levels })
    }

    pub async fn get(&self) -> Result<u64> {
        let now = Local::now();
        let hour = now.hour() as f64 + now.minute() as f64 / 60.0;
        let value = self.value_at(hour);
        log::trace!("ALS (time): {value}");
        Ok(value)
    }

    fn value_at(&self, hour: f64) -> u64 {
        if self.levels.len() == 1 {
            return self.levels[0].1;
        }
        let next_index = self
            .levels
            .iter()
            .position(|(candidate, _)| *candidate as f64 > hour)
            .unwrap_or(0);
        let previous_index = if next_index == 0 {
            self.levels.len() - 1
        } else {
            next_index - 1
        };
        let (previous_hour, previous_value) = self.levels[previous_index];
        let (mut next_hour, next_value) = self.levels[next_index];
        let mut hour = hour;
        if next_index == 0 {
            next_hour += 24;
            if hour < previous_hour as f64 {
                hour += 24.0;
            }
        }
        let progress = (hour - previous_hour as f64) / (next_hour - previous_hour) as f64;
        (previous_value as f64 + (next_value as f64 - previous_value as f64) * progress).round()
            as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_levels() {
        let als = Als::new([(0, 0), (12, 100)].into_iter().collect()).unwrap();
        assert_eq!(50, als.value_at(6.0));
        assert_eq!(50, als.value_at(18.0));
    }

    #[test]
    fn validates_hours() {
        assert!(Als::new([(24, 0)].into_iter().collect()).is_err());
    }
}
