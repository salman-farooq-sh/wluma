use anyhow::{anyhow, Context, Result};
use dbus::arg::RefArg;
use dbus::blocking::stdintf::org_freedesktop_dbus::{Properties, PropertiesPropertiesChanged};
use dbus::blocking::Connection as DbusConnection;
use dbus::message::{MatchRule, SignalArgs};
use smol::channel::Sender;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::ExtIdleNotificationV1;
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;

const MUTTER_DESTINATION: &str = "org.gnome.Mutter.IdleMonitor";
const MUTTER_PATH: &str = "/org/gnome/Mutter/IdleMonitor/Core";
const MUTTER_INTERFACE: &str = "org.gnome.Mutter.IdleMonitor";
const UPOWER_DESTINATION: &str = "org.freedesktop.UPower";
const UPOWER_PATH: &str = "/org/freedesktop/UPower";
const UPOWER_INTERFACE: &str = "org.freedesktop.UPower";
const DBUS_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_DELAY: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    PowerSourceChanged(PowerSource),
    Idled(PowerSource),
    Resumed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerSource {
    Ac,
    Battery,
}

struct State {
    source: PowerSource,
    ac_enabled: bool,
    battery_enabled: bool,
    ac_idled: bool,
    battery_idled: bool,
    idled: bool,
}

struct SharedState {
    state: Mutex<State>,
    events: Sender<Event>,
}

impl SharedState {
    fn new(events: Sender<Event>, ac_enabled: bool, battery_enabled: bool) -> Self {
        Self {
            state: Mutex::new(State {
                source: PowerSource::Ac,
                ac_enabled,
                battery_enabled,
                ac_idled: false,
                battery_idled: false,
                idled: false,
            }),
            events,
        }
    }

    fn set_source(&self, source: PowerSource) {
        let event = {
            let mut state = self.state.lock().unwrap();
            state.source = source;
            Self::event(&mut state)
        };
        let _ = self.events.send_blocking(Event::PowerSourceChanged(source));
        self.send(event);
    }

    fn set_idle(&self, source: PowerSource, idled: bool) {
        let event = {
            let mut state = self.state.lock().unwrap();
            match source {
                PowerSource::Ac => state.ac_idled = idled,
                PowerSource::Battery => state.battery_idled = idled,
            }
            Self::event(&mut state)
        };
        self.send(event);
    }

    fn resume(&self) {
        let event = {
            let mut state = self.state.lock().unwrap();
            state.ac_idled = false;
            state.battery_idled = false;
            Self::event(&mut state)
        };
        self.send(event);
    }

    fn event(state: &mut State) -> Option<Event> {
        let idled = match state.source {
            PowerSource::Ac => state.ac_enabled && state.ac_idled,
            PowerSource::Battery => state.battery_enabled && state.battery_idled,
        };
        if idled == state.idled {
            return None;
        }
        state.idled = idled;
        Some(if idled {
            Event::Idled(state.source)
        } else {
            Event::Resumed
        })
    }

    fn send(&self, event: Option<Event>) {
        if let Some(event) = event {
            let _ = self.events.send_blocking(event);
        }
    }
}

pub fn run(ac_timeout: Option<Duration>, battery_timeout: Option<Duration>, events: Sender<Event>) {
    let state = Arc::new(SharedState::new(
        events,
        ac_timeout.is_some(),
        battery_timeout.is_some(),
    ));
    monitor_power(state.clone());
    loop {
        let wayland_error = match run_wayland(ac_timeout, battery_timeout, state.clone()) {
            Ok(()) => anyhow!("Wayland idle notifier stopped"),
            Err(error) => error,
        };
        state.resume();
        let mutter_error = match run_mutter(ac_timeout, battery_timeout, state.clone()) {
            Ok(()) => anyhow!("Mutter idle monitor stopped"),
            Err(error) => error,
        };
        state.resume();
        log::warn!(
            "Unable to monitor idle state; retrying in {}s: Wayland: {wayland_error:#}; Mutter: {mutter_error:#}",
            RETRY_DELAY.as_secs()
        );
        std::thread::sleep(RETRY_DELAY);
    }
}

fn monitor_power(state: Arc<SharedState>) {
    std::thread::spawn(move || loop {
        if let Err(error) = watch_power(state.clone()) {
            log::warn!("Unable to monitor power source; assuming AC power: {error:#}");
            state.set_source(PowerSource::Ac);
            std::thread::sleep(RETRY_DELAY);
        }
    });
}

fn watch_power(state: Arc<SharedState>) -> Result<()> {
    let connection = DbusConnection::new_system().context("Unable to connect to system D-Bus")?;
    let signal_state = state.clone();
    let rule = PropertiesPropertiesChanged::match_rule(None, None).with_path(UPOWER_PATH);
    connection.add_match(rule, move |changed: PropertiesPropertiesChanged, _, _| {
        if changed.interface_name == UPOWER_INTERFACE {
            if let Some(on_battery) = changed
                .changed_properties
                .get("OnBattery")
                .and_then(|value| value.0.as_i64())
            {
                signal_state.set_source(if on_battery == 0 {
                    PowerSource::Ac
                } else {
                    PowerSource::Battery
                });
            }
        }
        true
    })?;
    let active = Arc::new(AtomicBool::new(true));
    let signal_active = active.clone();
    let owner_rule = MatchRule::new_signal("org.freedesktop.DBus", "NameOwnerChanged")
        .with_path("/org/freedesktop/DBus");
    connection.add_match(
        owner_rule,
        move |(name, _, owner): (String, String, String), _, _| {
            if name == UPOWER_DESTINATION && owner.is_empty() {
                signal_active.store(false, Ordering::Relaxed);
            }
            true
        },
    )?;
    let proxy = connection.with_proxy(UPOWER_DESTINATION, UPOWER_PATH, DBUS_TIMEOUT);
    let on_battery: bool = proxy
        .get(UPOWER_INTERFACE, "OnBattery")
        .context("UPower is unavailable")?;
    state.set_source(if on_battery {
        PowerSource::Battery
    } else {
        PowerSource::Ac
    });
    log::info!(
        "Using UPower; current power source is {}",
        if on_battery { "battery" } else { "AC" }
    );
    while active.load(Ordering::Relaxed) {
        connection.process(Duration::from_secs(1))?;
    }
    Err(anyhow!("UPower disconnected"))
}

struct WaylandState {
    notifier: Option<ExtIdleNotifierV1>,
    ac_notification: Option<ExtIdleNotificationV1>,
    battery_notification: Option<ExtIdleNotificationV1>,
    seat: Option<WlSeat>,
    state: Arc<SharedState>,
}

fn run_wayland(
    ac_timeout: Option<Duration>,
    battery_timeout: Option<Duration>,
    state: Arc<SharedState>,
) -> Result<()> {
    let connection = Connection::connect_to_env().context("Unable to connect to Wayland")?;
    let display = connection.display();
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let mut wayland = WaylandState {
        notifier: None,
        ac_notification: None,
        battery_notification: None,
        seat: None,
        state,
    };
    display.get_registry(&qh, ());
    queue
        .roundtrip(&mut wayland)
        .context("Unable to query Wayland idle protocols")?;
    let notifier = wayland
        .notifier
        .clone()
        .ok_or_else(|| anyhow!("ext-idle-notify-v1 is unavailable"))?;
    let seat = wayland
        .seat
        .clone()
        .ok_or_else(|| anyhow!("No Wayland seat is available"))?;
    wayland.ac_notification = ac_timeout.map(|timeout| {
        notifier.get_idle_notification(timeout.as_millis() as u32, &seat, &qh, PowerSource::Ac)
    });
    wayland.battery_notification = battery_timeout.map(|timeout| {
        notifier.get_idle_notification(timeout.as_millis() as u32, &seat, &qh, PowerSource::Battery)
    });
    queue
        .roundtrip(&mut wayland)
        .context("Unable to create Wayland idle notifications")?;
    log::info!("Using ext-idle-notify-v1 for idle detection");
    loop {
        queue
            .blocking_dispatch(&mut wayland)
            .context("Wayland idle notifier disconnected")?;
    }
}

impl Dispatch<WlRegistry, ()> for WaylandState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: <WlRegistry as Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_registry::Event;
        if let Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == ExtIdleNotifierV1::interface().name {
                state.notifier =
                    Some(registry.bind::<ExtIdleNotifierV1, _, _>(name, version.min(2), qh, ()));
            } else if interface == WlSeat::interface().name && state.seat.is_none() {
                state.seat = Some(registry.bind::<WlSeat, _, _>(
                    name,
                    version.min(WlSeat::interface().version),
                    qh,
                    (),
                ));
            }
        }
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for WaylandState {
    fn event(
        _: &mut Self,
        _: &ExtIdleNotifierV1,
        _: <ExtIdleNotifierV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotificationV1, PowerSource> for WaylandState {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: <ExtIdleNotificationV1 as Proxy>::Event,
        source: &PowerSource,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::Event;
        match event {
            Event::Idled => state.state.set_idle(*source, true),
            Event::Resumed => state.state.set_idle(*source, false),
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for WaylandState {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

fn run_mutter(
    ac_timeout: Option<Duration>,
    battery_timeout: Option<Duration>,
    state: Arc<SharedState>,
) -> Result<()> {
    let connection = DbusConnection::new_session().context("Unable to connect to session D-Bus")?;
    let (signals_tx, signals_rx) = mpsc::channel();
    let rule = MatchRule::new_signal(MUTTER_INTERFACE, "WatchFired").with_path(MUTTER_PATH);
    connection.add_match(rule, move |(id,): (u32,), _, _| {
        let _ = signals_tx.send(id);
        true
    })?;
    let proxy = connection.with_proxy(MUTTER_DESTINATION, MUTTER_PATH, DBUS_TIMEOUT);
    let ac_watch = ac_timeout
        .map(|timeout| {
            proxy.method_call(
                MUTTER_INTERFACE,
                "AddIdleWatch",
                (timeout.as_millis() as u64,),
            )
        })
        .transpose()
        .context("Mutter IdleMonitor is unavailable")?
        .map(|(watch,): (u32,)| watch);
    let battery_watch = battery_timeout
        .map(|timeout| {
            proxy.method_call(
                MUTTER_INTERFACE,
                "AddIdleWatch",
                (timeout.as_millis() as u64,),
            )
        })
        .transpose()?
        .map(|(watch,): (u32,)| watch);
    let mut active_watch = None;
    log::info!("Using Mutter IdleMonitor for idle detection");
    loop {
        connection.process(Duration::from_secs(1))?;
        while let Ok(id) = signals_rx.try_recv() {
            if Some(id) == active_watch {
                state.resume();
                active_watch = None;
                continue;
            }
            let source = if Some(id) == ac_watch {
                Some(PowerSource::Ac)
            } else if Some(id) == battery_watch {
                Some(PowerSource::Battery)
            } else {
                None
            };
            if let Some(source) = source {
                state.set_idle(source, true);
                if active_watch.is_none() {
                    let proxy =
                        connection.with_proxy(MUTTER_DESTINATION, MUTTER_PATH, DBUS_TIMEOUT);
                    let (watch,): (u32,) =
                        proxy.method_call(MUTTER_INTERFACE, "AddUserActiveWatch", ())?;
                    active_watch = Some(watch);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smol::channel;

    #[test]
    fn switching_power_source_selects_its_idle_state() {
        let (events_tx, events_rx) = channel::unbounded();
        let state = SharedState::new(events_tx, true, true);
        state.set_idle(PowerSource::Battery, true);
        assert!(events_rx.is_empty());

        state.set_source(PowerSource::Battery);
        assert_eq!(
            events_rx.try_recv().unwrap(),
            Event::PowerSourceChanged(PowerSource::Battery)
        );
        assert_eq!(
            events_rx.try_recv().unwrap(),
            Event::Idled(PowerSource::Battery)
        );

        state.set_source(PowerSource::Ac);
        assert_eq!(
            events_rx.try_recv().unwrap(),
            Event::PowerSourceChanged(PowerSource::Ac)
        );
        assert_eq!(events_rx.try_recv().unwrap(), Event::Resumed);
    }

    #[test]
    fn switching_source_while_both_are_idle_does_not_enter_idle_again() {
        let (events_tx, events_rx) = channel::unbounded();
        let state = SharedState::new(events_tx, true, true);
        state.set_idle(PowerSource::Ac, true);
        assert_eq!(events_rx.try_recv().unwrap(), Event::Idled(PowerSource::Ac));
        state.set_idle(PowerSource::Battery, true);

        state.set_source(PowerSource::Battery);

        assert_eq!(
            events_rx.try_recv().unwrap(),
            Event::PowerSourceChanged(PowerSource::Battery)
        );
        assert!(events_rx.is_empty());
    }

    #[test]
    fn disabled_power_source_never_enters_idle() {
        let (events_tx, events_rx) = channel::unbounded();
        let state = SharedState::new(events_tx, false, true);
        state.set_idle(PowerSource::Ac, true);
        assert!(events_rx.is_empty());

        state.set_idle(PowerSource::Battery, true);
        assert!(events_rx.is_empty());
        state.set_source(PowerSource::Battery);
        assert_eq!(
            events_rx.try_recv().unwrap(),
            Event::PowerSourceChanged(PowerSource::Battery)
        );
        assert_eq!(
            events_rx.try_recv().unwrap(),
            Event::Idled(PowerSource::Battery)
        );
    }

    #[test]
    fn activity_resumes_every_power_source() {
        let (events_tx, events_rx) = channel::unbounded();
        let state = SharedState::new(events_tx, true, true);
        state.set_idle(PowerSource::Ac, true);
        assert_eq!(events_rx.try_recv().unwrap(), Event::Idled(PowerSource::Ac));

        state.resume();
        assert_eq!(events_rx.try_recv().unwrap(), Event::Resumed);

        state.set_source(PowerSource::Battery);
        assert_eq!(
            events_rx.try_recv().unwrap(),
            Event::PowerSourceChanged(PowerSource::Battery)
        );
        assert!(events_rx.is_empty());
    }
}
