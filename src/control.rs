use anyhow::{anyhow, Context, Result};
use smol::channel::{Receiver, Sender};
use smol::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use smol::net::unix::{UnixListener, UnixStream};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SOCKET_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct Adjustment {
    pub percent: u8,
    pub relative: bool,
    pub increase: bool,
}

#[derive(Debug)]
pub enum Action {
    Get(String),
    Set(String, Adjustment),
    Pause(Option<String>, Option<Duration>),
    Resume(Option<String>),
    Toggle(Option<String>),
}

pub struct Command {
    pub action: Action,
    pub response: Sender<std::result::Result<String, String>>,
}

pub struct InstanceLock {
    _file: File,
}

#[derive(Clone)]
pub struct Hub(Arc<Mutex<State>>);

#[derive(Clone)]
struct State {
    als: AlsStatus,
    screens: BTreeMap<String, ScreenStatus>,
    monitors: BTreeMap<String, MonitorStatus>,
    watchers: Vec<Sender<Snapshot>>,
}

#[derive(Clone, PartialEq)]
struct AlsStatus {
    kind: String,
    value: Option<u64>,
}

#[derive(Clone, PartialEq)]
struct ScreenStatus {
    capturer: String,
    luma: Option<u8>,
}

#[derive(Clone, PartialEq)]
struct MonitorStatus {
    kind: String,
    brightness: Option<u8>,
    paused: bool,
}

#[derive(Clone, PartialEq)]
struct Snapshot {
    als: AlsStatus,
    screens: BTreeMap<String, ScreenStatus>,
    monitors: BTreeMap<String, MonitorStatus>,
}

impl Hub {
    pub fn new(als_kind: impl Into<String>) -> Self {
        Self(Arc::new(Mutex::new(State {
            als: AlsStatus {
                kind: als_kind.into(),
                value: None,
            },
            screens: BTreeMap::new(),
            monitors: BTreeMap::new(),
            watchers: Vec::new(),
        })))
    }

    pub fn set_als(&self, kind: &str, value: u64) {
        self.change(|state| {
            state.als.kind = kind.to_string();
            state.als.value = Some(value);
        });
    }

    pub fn add_output(&self, name: &str, kind: &str, capturer: Option<&str>) {
        self.change(|state| {
            state.monitors.insert(
                name.to_string(),
                MonitorStatus {
                    kind: kind.to_string(),
                    brightness: None,
                    paused: false,
                },
            );
            if let Some(capturer) = capturer {
                state.screens.insert(
                    name.to_string(),
                    ScreenStatus {
                        capturer: capturer.to_string(),
                        luma: None,
                    },
                );
            }
        });
    }

    pub fn remove_output(&self, name: &str) {
        self.change(|state| {
            state.screens.remove(name);
            state.monitors.remove(name);
        });
    }

    pub fn set_capturer(&self, name: &str, capturer: &str) {
        self.change(|state| {
            if let Some(screen) = state.screens.get_mut(name) {
                screen.capturer = capturer.to_string();
            }
        });
    }

    pub fn set_luma(&self, name: &str, luma: u8) {
        self.change(|state| {
            if let Some(screen) = state.screens.get_mut(name) {
                screen.luma = Some(luma);
            }
        });
    }

    pub fn set_brightness(&self, name: &str, brightness: u8) {
        self.change(|state| {
            if let Some(monitor) = state.monitors.get_mut(name) {
                monitor.brightness = Some(brightness);
            }
        });
    }

    pub fn set_paused(&self, name: &str, paused: bool) {
        self.change(|state| {
            if let Some(monitor) = state.monitors.get_mut(name) {
                monitor.paused = paused;
            }
        });
    }

    fn change(&self, update: impl FnOnce(&mut State)) {
        let mut state = self.0.lock().unwrap();
        let previous = state.snapshot();
        update(&mut state);
        let snapshot = state.snapshot();
        if snapshot != previous {
            state
                .watchers
                .retain(|watcher| watcher.try_send(snapshot.clone()).is_ok());
        }
    }

    fn snapshot(&self) -> Snapshot {
        self.0.lock().unwrap().snapshot()
    }

    fn subscribe(&self) -> Receiver<Snapshot> {
        let (tx, rx) = smol::channel::bounded(16);
        let mut state = self.0.lock().unwrap();
        let _ = tx.try_send(state.snapshot());
        state.watchers.push(tx);
        rx
    }
}

impl State {
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            als: self.als.clone(),
            screens: self.screens.clone(),
            monitors: self.monitors.clone(),
        }
    }
}

fn runtime_dir() -> Result<PathBuf> {
    let runtime =
        std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| anyhow!("XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime).join("wluma"))
}

pub fn socket_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("control.sock"))
}

impl InstanceLock {
    pub fn acquire() -> Result<Self> {
        let directory = runtime_dir()?;
        fs::create_dir_all(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let path = directory.join("daemon.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == ErrorKind::WouldBlock {
                return Err(anyhow!("another wluma daemon is already running"));
            }
            return Err(error.into());
        }
        Ok(Self { _file: file })
    }
}

pub async fn serve(hub: Hub, commands: Sender<Command>) -> Result<()> {
    let path = socket_path()?;
    let mut initial = true;
    loop {
        let listener = loop {
            match bind(&path, initial) {
                Ok(listener) => break listener,
                Err(error) if initial => return Err(error),
                Err(error) => {
                    log::warn!("Unable to restore control socket: {error:#}");
                    smol::Timer::after(SOCKET_CHECK_INTERVAL).await;
                }
            }
        };
        initial = false;
        let metadata = fs::metadata(&path)?;
        let identity = (metadata.dev(), metadata.ino());
        loop {
            enum Event {
                Connection(std::io::Result<UnixStream>),
                Check,
            }
            let event = smol::future::race(
                async { Event::Connection(listener.accept().await.map(|(stream, _)| stream)) },
                async {
                    smol::Timer::after(SOCKET_CHECK_INTERVAL).await;
                    Event::Check
                },
            )
            .await;
            match event {
                Event::Connection(Ok(stream)) => {
                    let hub = hub.clone();
                    let commands = commands.clone();
                    smol::spawn(async move {
                        if let Err(error) = handle(stream, hub, commands).await {
                            log::debug!("Control connection failed: {error:#}");
                        }
                    })
                    .detach();
                }
                Event::Connection(Err(error)) => return Err(error.into()),
                Event::Check if !socket_is_healthy(&path, identity) => {
                    log::warn!("Control socket disappeared or was damaged; restoring it");
                    break;
                }
                Event::Check => {}
            }
        }
    }
}

fn bind(path: &Path, check_existing: bool) -> Result<UnixListener> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    if check_existing && std::os::unix::net::UnixStream::connect(path).is_ok() {
        return Err(anyhow!("another wluma daemon is already running"));
    }
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

fn socket_is_healthy(path: &Path, identity: (u64, u64)) -> bool {
    fs::metadata(path).is_ok_and(|metadata| {
        (metadata.dev(), metadata.ino()) == identity
            && metadata.file_type().is_socket()
            && metadata.permissions().mode() & 0o777 == 0o600
    })
}

async fn handle(mut stream: UnixStream, hub: Hub, commands: Sender<Command>) -> Result<()> {
    let mut line = String::new();
    BufReader::new(&mut stream).read_line(&mut line).await?;
    let fields = line.trim_end().split('\t').collect::<Vec<_>>();
    match fields.as_slice() {
        ["status"] => write_response(&mut stream, &hub.snapshot().tables()).await?,
        ["status-json"] => write_response(&mut stream, &hub.snapshot().json()).await?,
        ["watch"] | ["watch-json"] => {
            let json = fields[0] == "watch-json";
            let updates = hub.subscribe();
            while let Ok(snapshot) = updates.recv().await {
                let line = if json {
                    snapshot.json()
                } else {
                    snapshot.line()
                };
                write_response(&mut stream, &line).await?;
            }
        }
        _ => {
            let action = parse_action(&fields)?;
            let (response_tx, response_rx) = smol::channel::bounded(1);
            commands
                .send(Command {
                    action,
                    response: response_tx,
                })
                .await?;
            match response_rx.recv().await? {
                Ok(value) => write_response(&mut stream, &value).await?,
                Err(error) => {
                    write_response(&mut stream, &format!("error\t{error}")).await?;
                }
            }
        }
    }
    Ok(())
}

fn parse_action(fields: &[&str]) -> Result<Action> {
    match fields {
        ["get", name] => Ok(Action::Get((*name).to_string())),
        ["set", name, value] => Ok(Action::Set((*name).to_string(), parse_adjustment(value)?)),
        ["pause", target, duration] => Ok(Action::Pause(
            parse_target(target),
            if *duration == "-" {
                None
            } else {
                Some(Duration::from_secs(duration.parse()?))
            },
        )),
        ["resume", target] => Ok(Action::Resume(parse_target(target))),
        ["toggle", target] => Ok(Action::Toggle(parse_target(target))),
        _ => Err(anyhow!("invalid control request")),
    }
}

fn parse_target(value: &str) -> Option<String> {
    (value != "*").then(|| value.to_string())
}

pub fn parse_adjustment(value: &str) -> Result<Adjustment> {
    let invalid = || anyhow!("brightness must be a percentage from 0% to 100%");
    let (relative, increase, number) = if let Some(value) = value.strip_prefix('+') {
        (true, true, value.strip_suffix('%').ok_or_else(invalid)?)
    } else if let Some(value) = value.strip_prefix('-') {
        (true, false, value.strip_suffix('%').ok_or_else(invalid)?)
    } else if let Some(value) = value.strip_suffix("%+") {
        (true, true, value)
    } else if let Some(value) = value.strip_suffix("%-") {
        (true, false, value)
    } else {
        (false, true, value.strip_suffix('%').ok_or_else(invalid)?)
    };
    let percent = number
        .parse::<u8>()
        .context("brightness must be a percentage from 0% to 100%")?;
    if percent > 100 {
        return Err(anyhow!("brightness must be a percentage from 0% to 100%"));
    }
    Ok(Adjustment {
        percent,
        relative,
        increase,
    })
}

async fn write_response(stream: &mut UnixStream, value: &str) -> Result<()> {
    stream.write_all(value.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;
    Ok(())
}

pub async fn send(request: &str, stream: bool) -> Result<()> {
    let path = socket_path()?;
    if !stream {
        return send_once(&path, request).await;
    }

    let mut connected = false;
    loop {
        match UnixStream::connect(&path).await {
            Ok(mut socket) => {
                connected = true;
                if write_request(&mut socket, request).await.is_ok() {
                    let mut reader = BufReader::new(socket);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => println!("{}", line.trim_end()),
                        }
                    }
                }
            }
            Err(error) if !connected => {
                return Err(error).with_context(|| {
                    format!("unable to connect to wluma daemon at {}", path.display())
                });
            }
            Err(_) => {}
        }
        smol::Timer::after(RECONNECT_INTERVAL).await;
    }
}

async fn send_once(path: &Path, request: &str) -> Result<()> {
    let mut socket = UnixStream::connect(path)
        .await
        .with_context(|| format!("unable to connect to wluma daemon at {}", path.display()))?;
    write_request(&mut socket, request).await?;
    let mut reader = BufReader::new(socket);
    let mut line = String::new();
    while reader.read_line(&mut line).await? != 0 {
        let value = line.trim_end();
        if let Some(error) = value.strip_prefix("error\t") {
            return Err(anyhow!(error.to_string()));
        }
        println!("{value}");
        line.clear();
    }
    Ok(())
}

async fn write_request(socket: &mut UnixStream, request: &str) -> Result<()> {
    socket.write_all(request.as_bytes()).await?;
    socket.write_all(b"\n").await?;
    socket.flush().await?;
    Ok(())
}

impl Snapshot {
    fn tables(&self) -> String {
        let als_width = "ALS TYPE".len().max(self.als.kind.len());
        let mut lines = vec![
            format!("{:<als_width$}  VALUE", "ALS TYPE"),
            format!(
                "{:<als_width$}  {}",
                self.als.kind,
                optional(self.als.value, "")
            ),
            String::new(),
        ];
        let headers = ["OUTPUT", "TYPE", "CAPTURER", "LUMA", "BRIGHTNESS", "STATE"];
        let rows = self
            .monitors
            .iter()
            .map(|(name, monitor)| {
                let screen = self.screens.get(name);
                [
                    name.to_string(),
                    monitor.kind.clone(),
                    screen.map_or_else(|| "-".to_string(), |screen| screen.capturer.clone()),
                    optional(screen.and_then(|screen| screen.luma), "%"),
                    optional(monitor.brightness, "%"),
                    if monitor.paused { "paused" } else { "active" }.to_string(),
                ]
            })
            .collect::<Vec<_>>();
        let widths = std::array::from_fn::<_, 6, _>(|column| {
            rows.iter()
                .map(|row| row[column].len())
                .max()
                .unwrap_or(0)
                .max(headers[column].len())
        });
        lines.push(format_row(&headers, &widths));
        lines.extend(rows.iter().map(|row| format_row(row, &widths)));
        lines.join("\n")
    }

    fn line(&self) -> String {
        let mut parts = vec![format!("als={}", optional(self.als.value, ""))];
        for (name, monitor) in &self.monitors {
            let luma = self.screens.get(name).and_then(|screen| screen.luma);
            parts.push(format!(
                "{name}:luma={},brightness={},{}",
                optional(luma, "%"),
                optional(monitor.brightness, "%"),
                if monitor.paused { "paused" } else { "active" }
            ));
        }
        parts.join(" ")
    }

    fn json(&self) -> String {
        let outputs = self
            .monitors
            .iter()
            .map(|(name, monitor)| {
                let screen = self.screens.get(name);
                format!(
                    "{{\"name\":{},\"type\":{},\"capturer\":{},\"luma\":{},\"brightness\":{},\"paused\":{}}}",
                    json_string(name),
                    json_string(&monitor.kind),
                    screen.map_or_else(|| "null".to_string(), |screen| json_string(&screen.capturer)),
                    json_option(screen.and_then(|screen| screen.luma)),
                    json_option(monitor.brightness),
                    monitor.paused
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"als\":{{\"type\":{},\"value\":{}}},\"outputs\":[{}]}}",
            json_string(&self.als.kind),
            json_option(self.als.value),
            outputs
        )
    }
}

fn format_row<T: AsRef<str>, const N: usize>(values: &[T; N], widths: &[usize; N]) -> String {
    values
        .iter()
        .zip(widths)
        .map(|(value, width)| format!("{:<width$}", value.as_ref()))
        .collect::<Vec<_>>()
        .join("  ")
        .trim_end()
        .to_string()
}

fn optional<T: std::fmt::Display>(value: Option<T>, suffix: &str) -> String {
    value.map_or_else(|| "-".to_string(), |value| format!("{value}{suffix}"))
}

fn json_option<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "null".to_string(), |value| value.to_string())
}

fn json_string(value: &str) -> String {
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '\"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character.is_control() => {
                output.push_str(&format!("\\u{:04x}", character as u32))
            }
            character => output.push(character),
        }
    }
    output.push('\"');
    output
}

#[cfg(test)]
mod tests {
    use super::{parse_adjustment, Hub};

    #[test]
    fn parses_brightness_adjustments() {
        let absolute = parse_adjustment("60%").unwrap();
        assert_eq!(absolute.percent, 60);
        assert!(!absolute.relative);

        for value in ["+5%", "5%+"] {
            let adjustment = parse_adjustment(value).unwrap();
            assert_eq!(adjustment.percent, 5);
            assert!(adjustment.relative);
            assert!(adjustment.increase);
        }

        for value in ["-5%", "5%-"] {
            let adjustment = parse_adjustment(value).unwrap();
            assert_eq!(adjustment.percent, 5);
            assert!(adjustment.relative);
            assert!(!adjustment.increase);
        }

        assert!(parse_adjustment("60").is_err());
        assert!(parse_adjustment("+5").is_err());
        assert!(parse_adjustment("101%").is_err());
    }

    #[test]
    fn aligns_status_columns_for_long_output_names() {
        let hub = Hub::new("iio-sensor-proxy");
        hub.set_als("iio-sensor-proxy", 925);
        hub.add_output("DP-1", "ddc", Some("wayland"));
        hub.set_luma("DP-1", 16);
        hub.set_brightness("DP-1", 99);
        hub.add_output("dell::kbd_backlight", "backlight", None);
        hub.set_brightness("dell::kbd_backlight", 0);

        assert_eq!(
            hub.snapshot().tables(),
            "ALS TYPE          VALUE\n\
             iio-sensor-proxy  925\n\
             \n\
             OUTPUT               TYPE       CAPTURER  LUMA  BRIGHTNESS  STATE\n\
             DP-1                 ddc        wayland   16%   99%         active\n\
             dell::kbd_backlight  backlight  -         -     0%          active"
        );
    }
}
