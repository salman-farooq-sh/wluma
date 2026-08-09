use crate::device_file::read;
use anyhow::{anyhow, Error, Result};
use futures_util::{StreamExt, TryFutureExt};
use smol::fs::File;
use smol::lock::Mutex;
use std::collections::HashMap;
use std::ops::DerefMut;
use std::path::{Path, PathBuf};
use std::time::Duration;
use SensorType::*;

const SENSOR_PROXY_POLL_INTERVAL: Duration = Duration::from_millis(100);
const SYSFS_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[allow(clippy::large_enum_variant)]
enum SensorType {
    Illuminance {
        value: Mutex<File>,
        scale: f64,
        offset: f64,
    },
    Intensity {
        r: Mutex<File>,
        g: Mutex<File>,
        b: Mutex<File>,
    },
}

enum Source {
    SensorProxy(Mutex<super::sensor_proxy::Sensor>),
    Sysfs(Box<SensorType>),
}

pub struct Als {
    source: Source,
    thresholds: HashMap<u64, String>,
}

impl Als {
    pub async fn new(base_path: Option<&str>, thresholds: HashMap<u64, String>) -> Result<Self> {
        let source = match smol::unblock(super::sensor_proxy::Sensor::new).await {
            Ok(sensor) => {
                if base_path.is_some() {
                    log::warn!(
                        "Using iio-sensor-proxy; remove the deprecated IIO 'path' from your config."
                    );
                } else {
                    log::debug!("Using iio-sensor-proxy for ambient light");
                }
                Source::SensorProxy(Mutex::new(sensor))
            }
            Err(proxy_error) => {
                log::debug!("Unable to use iio-sensor-proxy: {proxy_error}");
                let base_path = base_path.ok_or_else(|| {
                    anyhow!(
                        "Unable to use iio-sensor-proxy and no IIO path is configured: {proxy_error}"
                    )
                })?;
                Source::Sysfs(Box::new(find_sensor(base_path).await?))
            }
        };

        Ok(Self { source, thresholds })
    }

    pub async fn get(&self) -> Result<String> {
        let raw = self.get_raw().await?;
        let profile = super::find_profile(raw, &self.thresholds);

        log::trace!("ALS (iio): {} ({})", profile, raw);
        Ok(profile)
    }

    pub fn poll_interval(&self) -> Duration {
        match &self.source {
            Source::SensorProxy(_) => SENSOR_PROXY_POLL_INTERVAL,
            Source::Sysfs(_) => SYSFS_POLL_INTERVAL,
        }
    }

    async fn get_raw(&self) -> Result<u64> {
        match &self.source {
            Source::SensorProxy(sensor) => Ok(sensor.lock().await.get_raw().await),
            Source::Sysfs(sensor) => Ok(match sensor.as_ref() {
                Illuminance {
                    value,
                    scale,
                    offset,
                } => (read(value.lock().await.deref_mut()).await? + offset) * scale,
                Intensity { r, g, b } => {
                    -0.32466 * read(r.lock().await.deref_mut()).await?
                        + 1.57837 * read(g.lock().await.deref_mut()).await?
                        + -0.73191 * read(b.lock().await.deref_mut()).await?
                }
            } as u64),
        }
    }
}

async fn find_sensor(base_path: &str) -> Result<SensorType> {
    smol::fs::read_dir(base_path)
        .await
        .map_err(|e| anyhow!("Can't enumerate iio devices: {e}"))?
        .filter_map(|r| async { r.ok() })
        .filter_map(|entry| async move {
            let path = entry.path();
            // TODO should probably start from the `parse_illuminance_input` in the next major version
            parse_illuminance_raw(path.clone())
                .or_else(|_| parse_illuminance_input(path.clone()))
                .or_else(|_| parse_intensity_raw(path.clone()))
                .or_else(|_| parse_intensity_rgb(path.clone()))
                .await
                .ok()
                .map(|sensor| (path, sensor))
        })
        .boxed()
        .next()
        .await
        .map(|(path, sensor)| {
            log::debug!("Using IIO ambient light sensor '{}'", path.display());
            sensor
        })
        .ok_or_else(|| anyhow!("No supported IIO ambient light sensor found"))
}

async fn parse_illuminance_raw(path: PathBuf) -> Result<SensorType> {
    Ok(Illuminance {
        value: Mutex::new(
            open_file(&path, "in_illuminance_raw")
                .or_else(|_| open_file(&path, "in_illuminance0_raw"))
                .await?,
        ),
        scale: {
            open_file(&path, "in_illuminance_scale")
                .or_else(|_| open_file(&path, "in_illuminance0_scale"))
                .and_then(move |mut f| async move { read(&mut f).await })
                .await
                .unwrap_or(1_f64)
        },
        offset: {
            open_file(&path, "in_illuminance_offset")
                .or_else(|_| open_file(&path, "in_illuminance0_offset"))
                .and_then(move |mut f| async move { read(&mut f).await })
                .await
                .unwrap_or(0_f64)
        },
    })
}

async fn parse_intensity_raw(path: PathBuf) -> Result<SensorType> {
    async fn try_open_and_read(path: &Path, name: &str) -> Result<f64> {
        let mut f = open_file(path, name).await?;
        read(&mut f).await
    }

    Ok(Illuminance {
        value: Mutex::new(open_file(&path, "in_intensity_both_raw").await?),
        scale: try_open_and_read(&path, "in_intensity_scale")
            .await
            .unwrap_or(1_f64),
        offset: try_open_and_read(&path, "in_intensity_offset")
            .await
            .unwrap_or(0_f64),
    })
}

async fn parse_illuminance_input(path: PathBuf) -> Result<SensorType> {
    Ok(Illuminance {
        value: Mutex::new(
            open_file(&path, "in_illuminance_input")
                .or_else(|_| open_file(&path, "in_illuminance0_input"))
                .await?,
        ),
        scale: 1_f64,
        offset: 0_f64,
    })
}

async fn parse_intensity_rgb(path: PathBuf) -> Result<SensorType> {
    Ok(Intensity {
        r: Mutex::new(open_file(&path, "in_intensity_red_raw").await?),
        g: Mutex::new(open_file(&path, "in_intensity_green_raw").await?),
        b: Mutex::new(open_file(&path, "in_intensity_blue_raw").await?),
    })
}

async fn open_file(path: &Path, name: &str) -> Result<File> {
    File::open(path.join(name)).await.map_err(Error::msg)
}
