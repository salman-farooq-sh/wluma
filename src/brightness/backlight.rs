use crate::device_file::{read, write};
use anyhow::{anyhow, Error, Result};
use dbus::channel::Sender;
use dbus::{self, blocking::Connection, Message};
use inotify::{Inotify, WatchMask};
use smol::fs::{File, OpenOptions};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

const TRANSITION_STEP_MS: u64 = 16;
const BRIGHTNESS_STEPS: u64 = 1000;

fn requires_polling(path: &Path) -> bool {
    path.parent()
        .and_then(Path::file_name)
        .is_some_and(|subsystem| subsystem == "leds")
}

struct Dbus {
    connection: Connection,
    message: Message,
}

pub struct Backlight {
    file: File,
    min_brightness: u64,
    max_brightness: u64,
    inotify: Inotify,
    current: Option<u64>,
    dbus: Option<Dbus>,
    has_write_permission: bool,
    poll_brightness: bool,
}

impl Backlight {
    pub async fn new(path: &str, min_brightness: u64) -> Result<Self> {
        let brightness_path = Path::new(path).join("brightness");

        let current_brightness = fs::read(&brightness_path)?;

        let has_write_permission = fs::write(&brightness_path, current_brightness).is_ok();

        let (file, dbus) = if has_write_permission {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&brightness_path)
                .await?;

            log::debug!("Using direct write on {} to change brightness value", path);
            (file, None)
        } else {
            let file = File::open(&brightness_path).await?;

            let id = Path::new(path)
                .file_name()
                .and_then(|x| x.to_str())
                .ok_or(anyhow!("Unable to identify backlight ID"))?;

            let subsystem = Path::new(path)
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|x| x.to_str())
                .and_then(|x| match x {
                    "backlight" | "leds" => Some(x),
                    _ => None,
                })
                .ok_or(anyhow!(
                    "Unable to identify backlight subsystem out of {path}, please open an issue on GitHub"
                ))?;

            let message = Message::new_method_call(
                "org.freedesktop.login1",
                "/org/freedesktop/login1/session/auto",
                "org.freedesktop.login1.Session",
                "SetBrightness",
            )
            .ok()
            .map(|m| m.append2(subsystem, id));

            let connection = Connection::new_system().ok().and_then(|connection| {
                message.map(|message| Dbus {
                    connection,
                    message,
                })
            });

            log::debug!("Using DBUS for {} to change brightness value", path);
            (file, connection)
        };

        let max_brightness = fs::read_to_string(Path::new(path).join("max_brightness"))?
            .trim()
            .parse()?;

        let inotify = Inotify::init()?;
        inotify.watches().add(&brightness_path, WatchMask::MODIFY)?;

        let brightness_hw_changed_path = Path::new(path).join("brightness_hw_changed");
        if Path::new(&brightness_hw_changed_path).exists() {
            inotify
                .watches()
                .add(&brightness_hw_changed_path, WatchMask::MODIFY)?;
        }

        let poll_brightness = requires_polling(Path::new(path));

        Ok(Self {
            file,
            min_brightness,
            max_brightness,
            inotify,
            current: None,
            dbus,
            has_write_permission,
            poll_brightness,
        })
    }

    pub async fn get(&mut self) -> Result<u64> {
        async fn update(this: &mut Backlight) -> Result<u64> {
            let value = read(&mut this.file).await? as u64;
            this.current = Some(value.clamp(this.min_brightness, this.max_brightness));
            Ok(value)
        }

        if self.poll_brightness {
            return update(self).await;
        }

        let mut buffer = [0u8; 1024];
        match (self.inotify.read_events(&mut buffer), self.current) {
            (_, None) => update(self).await,
            (Ok(mut events), Some(cached)) => {
                if events.next().is_none() {
                    Ok(cached)
                } else {
                    update(self).await
                }
            }
            (Err(err), Some(cached)) if err.kind() == ErrorKind::WouldBlock => Ok(cached),
            (Err(err), _) => Err(err.into()),
        }
    }

    pub fn min(&self) -> u64 {
        self.min_brightness
    }

    pub fn max(&self) -> u64 {
        self.max_brightness
    }

    pub fn transition_step_ms(&self) -> u64 {
        TRANSITION_STEP_MS
    }

    pub fn change_threshold(&self) -> u64 {
        self.max_brightness
            .saturating_sub(self.min_brightness)
            .div_ceil(BRIGHTNESS_STEPS)
            .max(1)
    }

    pub async fn set(&mut self, value: u64) -> Result<u64> {
        let value = value.clamp(self.min_brightness, self.max_brightness);

        if self.has_write_permission {
            write(&mut self.file, value as f64).await?;
        } else if let Some(dbus) = &self.dbus {
            let mut message = dbus
                .message
                .duplicate()
                .map_err(Error::msg)?
                .append1(value as u32);
            message.set_no_reply(true);
            dbus.connection
                .send(message)
                .map_err(|_| anyhow!("Unable to send brightness change message via dbus"))?;
        } else {
            Err(std::io::Error::from(ErrorKind::PermissionDenied))?
        }

        self.current = Some(value);

        // Consume file events to not trigger get() update
        let mut buffer = [0u8; 1024];
        match self.inotify.read_events(&mut buffer) {
            Err(err) if err.kind() == ErrorKind::WouldBlock => Ok(value),
            Err(err) => Err(err.into()),
            _ => Ok(value),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn led_brightness_requires_polling() {
        assert!(requires_polling(Path::new(
            "/sys/class/leds/dell::kbd_backlight"
        )));
    }

    #[test]
    fn display_brightness_uses_inotify() {
        assert!(!requires_polling(Path::new(
            "/sys/class/backlight/intel_backlight"
        )));
    }
}
