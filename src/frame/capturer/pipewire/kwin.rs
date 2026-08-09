use anyhow::{anyhow, Context, Result};
use std::sync::{Arc, Mutex};
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_plasma::screencast::v1::client::zkde_screencast_stream_unstable_v1::ZkdeScreencastStreamUnstableV1;
use wayland_protocols_plasma::screencast::v1::client::zkde_screencast_unstable_v1::{
    Pointer, ZkdeScreencastUnstableV1,
};

#[derive(Clone)]
struct OutputContext {
    desired_output: String,
    name: Arc<Mutex<Option<String>>>,
}

#[derive(Default)]
struct State {
    manager: Option<ZkdeScreencastUnstableV1>,
    output: Option<WlOutput>,
    output_name: Option<String>,
    node: Option<u32>,
    failure: Option<String>,
}

pub(super) fn node(output_name: &str) -> Result<(Option<u32>, Option<String>)> {
    find(output_name, true)
}

pub(super) fn connector(output_name: &str) -> Result<String> {
    let (_, connector) = find(output_name, false)?;
    connector.ok_or_else(|| anyhow!("Unable to determine the connector for '{output_name}'"))
}

fn find(output_name: &str, create_stream: bool) -> Result<(Option<u32>, Option<String>)> {
    let connection = Connection::connect_to_env().context("Unable to connect to Wayland")?;
    let display = connection.display();
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    display.get_registry(
        &qh,
        OutputContext {
            desired_output: output_name.to_string(),
            name: Arc::new(Mutex::new(None)),
        },
    );

    let mut state = State::default();
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;

    let output = state
        .output
        .as_ref()
        .ok_or_else(|| anyhow!("Unable to match '{output_name}' to a Wayland output"))?;
    if !create_stream {
        return Ok((None, state.output_name));
    }
    let Some(manager) = state.manager.as_ref() else {
        return Ok((None, state.output_name));
    };
    manager.stream_output(output, Pointer::Hidden.into(), &qh, ());

    while state.node.is_none() && state.failure.is_none() {
        queue.blocking_dispatch(&mut state)?;
    }
    if let Some(error) = state.failure.take() {
        return Err(anyhow!(error));
    }

    let node = state.node.unwrap();
    std::thread::spawn(move || while queue.blocking_dispatch(&mut state).is_ok() {});
    log::debug!("Using KWin PipeWire stream node {node}");
    Ok((Some(node), None))
}

impl Dispatch<WlRegistry, OutputContext> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: <WlRegistry as Proxy>::Event,
        context: &OutputContext,
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
            if interface == WlOutput::interface().name {
                registry.bind::<WlOutput, _, _>(
                    name,
                    version.min(4),
                    qh,
                    OutputContext {
                        desired_output: context.desired_output.clone(),
                        name: Arc::new(Mutex::new(None)),
                    },
                );
            } else if interface == ZkdeScreencastUnstableV1::interface().name {
                state.manager = Some(registry.bind::<ZkdeScreencastUnstableV1, _, _>(
                    name,
                    version.min(4),
                    qh,
                    (),
                ));
            }
        }
    }
}

impl Dispatch<WlOutput, OutputContext> for State {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: <WlOutput as Proxy>::Event,
        context: &OutputContext,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_output::Event;

        let matches = match event {
            Event::Name { name } => {
                *context.name.lock().unwrap() = Some(name.clone());
                if state.output.as_ref() == Some(output) {
                    state.output_name = Some(name.clone());
                }
                name.contains(&context.desired_output)
            }
            Event::Description { description } => description.contains(&context.desired_output),
            _ => false,
        };
        if matches {
            match state.output.as_ref() {
                None => {
                    state.output = Some(output.clone());
                    state.output_name = context.name.lock().unwrap().clone();
                }
                Some(selected) if selected != output => {
                    log::error!(
                        "Multiple Wayland outputs match '{}'",
                        context.desired_output
                    );
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ZkdeScreencastUnstableV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZkdeScreencastUnstableV1,
        _: <ZkdeScreencastUnstableV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZkdeScreencastStreamUnstableV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZkdeScreencastStreamUnstableV1,
        event: <ZkdeScreencastStreamUnstableV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_protocols_plasma::screencast::v1::client::zkde_screencast_stream_unstable_v1::Event;

        match event {
            Event::Created { node } => state.node = Some(node),
            Event::Failed { error } => state.failure = Some(error),
            Event::Closed => state.failure = Some("KDE closed the screen stream".to_string()),
            _ => {}
        }
    }
}
