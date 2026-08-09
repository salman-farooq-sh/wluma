use anyhow::{anyhow, Result};
use dbus::arg::{prop_cast, OwnedFd, PropMap, RefArg, Variant};
use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
use dbus::blocking::Connection;
use dbus::message::MatchRule;
use dbus::Path;
use std::collections::HashMap;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DESTINATION: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const INTERFACE: &str = "org.freedesktop.portal.ScreenCast";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";
const TIMEOUT: Duration = Duration::from_secs(5);
static TOKEN: AtomicU64 = AtomicU64::new(0);

pub(super) struct Source {
    pub node: u32,
    pub remote: OwnedFd,
    pub connection: Connection,
}

pub(super) fn source(output_name: &str) -> Result<Source> {
    let connection = Connection::new_session()?;
    let proxy = connection.with_proxy(DESTINATION, PATH, TIMEOUT);

    let mut create_options = options();
    create_options.insert(
        "session_handle_token".to_string(),
        Variant(Box::new(token("session"))),
    );
    let result = request(&connection, create_options, |options| {
        proxy.method_call(INTERFACE, "CreateSession", (options,))
    })?;
    let session_path = prop_cast::<String>(&result, "session_handle")
        .ok_or_else(|| anyhow!("ScreenCast portal did not return a session handle"))?;
    let session_path = Path::new(session_path.clone())
        .map_err(|error| anyhow!(error))?
        .into_static();

    let version = proxy.get::<u32>(INTERFACE, "version").unwrap_or(0);
    let mut select_options = options();
    select_options.insert("types".to_string(), Variant(Box::new(1_u32)));
    select_options.insert("multiple".to_string(), Variant(Box::new(false)));
    if version >= 4 {
        select_options.insert("persist_mode".to_string(), Variant(Box::new(2_u32)));
        match restore_token(output_name) {
            Ok(Some(token)) => {
                select_options.insert("restore_token".to_string(), Variant(Box::new(token)));
            }
            Ok(None) => {}
            Err(error) => log::warn!("Unable to load PipeWire portal restore token: {error:#}"),
        }
    }
    request(&connection, select_options, |options| {
        proxy.method_call(INTERFACE, "SelectSources", (session_path.clone(), options))
    })?;

    log::info!("Select the monitor for output '{output_name}' in the screen sharing dialog");
    let result = request(&connection, options(), |options| {
        proxy.method_call(INTERFACE, "Start", (session_path.clone(), "", options))
    })?;
    if version >= 4 {
        let persistence = prop_cast::<String>(&result, "restore_token")
            .map(|token| save_restore_token(output_name, token))
            .unwrap_or_else(|| clear_restore_token(output_name));
        if let Err(error) = persistence {
            log::warn!("Unable to update PipeWire portal restore token: {error:#}");
        }
    }
    let node = result
        .get("streams")
        .and_then(|streams| streams.0.as_iter())
        .and_then(|mut streams| streams.next())
        .and_then(|stream| stream.as_iter())
        .and_then(|mut fields| fields.next())
        .and_then(|node| node.as_u64())
        .and_then(|node| u32::try_from(node).ok())
        .ok_or_else(|| anyhow!("ScreenCast portal did not return a PipeWire stream"))?;
    let empty = PropMap::new();
    let (remote,): (OwnedFd,) =
        proxy.method_call(INTERFACE, "OpenPipeWireRemote", (session_path, empty))?;

    log::debug!("Using portal PipeWire stream node {node}");
    Ok(Source {
        node,
        remote,
        connection,
    })
}

fn request<F>(connection: &Connection, mut options: PropMap, call: F) -> Result<PropMap>
where
    F: FnOnce(PropMap) -> Result<(Path<'static>,), dbus::Error>,
{
    let request_token = token("request");
    options.insert(
        "handle_token".to_string(),
        Variant(Box::new(request_token.clone())),
    );
    let sender = connection.unique_name().to_string()[1..].replace('.', "_");
    let request_path = Path::new(format!(
        "/org/freedesktop/portal/desktop/request/{sender}/{request_token}"
    ))
    .map_err(|error| anyhow!(error))?
    .into_static();
    let response = Arc::new(Mutex::new(None));
    let signal_response = response.clone();
    let rule = MatchRule::new_signal(REQUEST_INTERFACE, "Response").with_path(request_path.clone());
    let match_token = connection.add_match(rule, move |result: (u32, PropMap), _, _| {
        *signal_response.lock().unwrap() = Some(result);
        true
    })?;

    let (returned_path,) = match call(options) {
        Ok(result) => result,
        Err(error) => {
            connection.remove_match(match_token)?;
            return Err(error.into());
        }
    };
    if returned_path != request_path {
        connection.remove_match(match_token)?;
        return Err(anyhow!(
            "ScreenCast portal returned unexpected request path {returned_path}"
        ));
    }

    let (status, result) = loop {
        if let Some(result) = response.lock().unwrap().take() {
            break result;
        }
        connection.process(Duration::from_secs(1))?;
    };
    connection.remove_match(match_token)?;
    match status {
        0 => Ok(result),
        1 => Err(anyhow!("ScreenCast portal request was cancelled")),
        _ => Err(anyhow!("ScreenCast portal request failed")),
    }
}

fn options() -> PropMap {
    HashMap::<String, Variant<Box<dyn RefArg>>>::new()
}

fn restore_token(output_name: &str) -> Result<Option<String>> {
    match fs::read_to_string(restore_token_path(output_name)?) {
        Ok(token) if !token.trim().is_empty() => Ok(Some(token)),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn save_restore_token(output_name: &str, token: &str) -> Result<()> {
    let path = xdg::BaseDirectories::with_prefix("wluma")
        .place_state_file(restore_token_name(output_name))?;
    fs::write(path, token)?;
    Ok(())
}

fn clear_restore_token(output_name: &str) -> Result<()> {
    match fs::remove_file(restore_token_path(output_name)?) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn restore_token_path(output_name: &str) -> Result<std::path::PathBuf> {
    xdg::BaseDirectories::with_prefix("wluma")
        .get_state_file(restore_token_name(output_name))
        .ok_or_else(|| anyhow!("XDG state directory is unavailable"))
}

fn restore_token_name(output_name: &str) -> String {
    let output = output_name
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("xdg-desktop-portal-screencast-{output}.token")
}

fn token(prefix: &str) -> String {
    format!("wluma_{prefix}_{}", TOKEN.fetch_add(1, Ordering::Relaxed))
}
