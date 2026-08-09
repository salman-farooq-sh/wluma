use crate::config::WaylandProtocol;
use crate::frame::object::Object;
use crate::frame::vulkan::Vulkan;
use crate::predictor::Controller;
use std::os::fd::BorrowedFd;
use std::thread;
use std::time::Duration;
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::Connection;
use wayland_client::Dispatch;
use wayland_client::Proxy;
use wayland_client::QueueHandle;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::Options;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::Flags;
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1;
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1;
use wayland_protocols_wlr::export_dmabuf::v1::client::zwlr_export_dmabuf_frame_v1::ZwlrExportDmabufFrameV1;
use wayland_protocols_wlr::export_dmabuf::v1::client::zwlr_export_dmabuf_manager_v1::ZwlrExportDmabufManagerV1;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

const DELAY_SUCCESS: Duration = Duration::from_millis(100);
const DELAY_FAILURE: Duration = Duration::from_millis(1000);

pub struct Capturer {
    protocol: WaylandProtocol,
    is_processing_frame: bool,
    vulkan: Option<Vulkan>,
    output: Option<WlOutput>,
    output_global_id: Option<u32>,
    pending_frame: Option<Object>,
    dmabuf_formats: Vec<(u32, Vec<u64>)>,
    controller: Option<Controller>,
    // linux-dmabuf-v1
    dmabuf: Option<ZwpLinuxDmabufV1>,
    wl_buffer: Option<WlBuffer>,
    // ext-image-capture-source-v1
    img_capture_source_manager: Option<ExtOutputImageCaptureSourceManagerV1>,
    // ext-image-copy-capture-v1
    img_copy_capture_manager: Option<ExtImageCopyCaptureManagerV1>,
    img_copy_capture_session: Option<ExtImageCopyCaptureSessionV1>,
    // wlr-screencopy-unstable-v1
    screencopy_manager: Option<ZwlrScreencopyManagerV1>,
    // wlr-export-dmabuf-unstable-v1
    dmabuf_manager: Option<ZwlrExportDmabufManagerV1>,
}

#[derive(Clone)]
struct GlobalsContext {
    global_id: Option<u32>,
    desired_output: String,
}

impl Capturer {
    pub fn new(protocol: WaylandProtocol) -> Self {
        Self {
            protocol,
            is_processing_frame: false,
            vulkan: None,
            output: None,
            output_global_id: None,
            pending_frame: None,
            dmabuf_formats: Vec::new(),
            controller: None,
            // linux-dmabuf-v1
            dmabuf: None,
            wl_buffer: None,
            // ext-image-capture-source-v1
            img_capture_source_manager: None,
            // ext-image-copy-capture-v1
            img_copy_capture_manager: None,
            img_copy_capture_session: None,
            // wlr-screencopy-unstable-v1
            screencopy_manager: None,
            // wlr-export-dmabuf-unstable-v1
            dmabuf_manager: None,
        }
    }
}

impl Capturer {
    pub fn run(&mut self, output_name: &str, controller: Controller) {
        let connection =
            Connection::connect_to_env().expect("Unable to connect to Wayland display");
        let display = connection.display();
        let mut event_queue = connection.new_event_queue();
        let qh = event_queue.handle();

        let ctx = GlobalsContext {
            global_id: None,
            desired_output: output_name.to_string(),
        };

        display.get_registry(&qh, ctx);

        // 1. process registry events
        event_queue
            .roundtrip(self)
            .expect("Unable to perform initial roundtrip");

        // 2. registry requested wl_output events, process those
        event_queue
            .roundtrip(self)
            .expect("Unable to perform 2nd initial roundtrip");

        if self.output.is_none() {
            log::warn!(
                "Unable to match config '{}' to any Wayland output.",
                output_name,
            );
        }

        let protocol_to_use = match self.protocol {
            WaylandProtocol::ExtImageCopyCaptureV1 => {
                if self.img_copy_capture_manager.is_none() {
                    panic!("Requested to use ext-image-copy-capture-v1 protocol, but it's not available");
                }
                if self.img_capture_source_manager.is_none() {
                    panic!("Requested to use ext-image-copy-capture-v1 protocol, but a required ext-image-capture-source-v1 protocol it's not available");
                }
                if self.dmabuf.is_none() {
                    panic!("Requested to use ext-image-copy-capture-v1 protocol, but a required linux-dmabuf-v1 protocol it's not available");
                }
                WaylandProtocol::ExtImageCopyCaptureV1
            }
            WaylandProtocol::WlrScreencopyUnstableV1 => {
                if self.screencopy_manager.is_none() {
                    panic!("Requested to use wlr-screencopy-unstable-v1 protocol, but it's not available");
                }
                if self.dmabuf.is_none() {
                    panic!("Requested to use wlr-screencopy-unstable-v1 protocol, but a required linux-dmabuf-v1 protocol it's not available");
                }
                WaylandProtocol::WlrScreencopyUnstableV1
            }
            WaylandProtocol::WlrExportDmabufUnstableV1 => {
                if self.dmabuf_manager.is_none() {
                    panic!("Requested to use wlr-export-dmabuf-unstable-v1 protocol, but it's not available");
                }
                WaylandProtocol::WlrExportDmabufUnstableV1
            }
            WaylandProtocol::Any => {
                if self.img_copy_capture_manager.is_some()
                    && self.img_capture_source_manager.is_some()
                    && self.dmabuf.is_some()
                {
                    WaylandProtocol::ExtImageCopyCaptureV1
                } else if self.screencopy_manager.is_some() && self.dmabuf.is_some() {
                    WaylandProtocol::WlrScreencopyUnstableV1
                } else if self.dmabuf_manager.is_some() {
                    WaylandProtocol::WlrExportDmabufUnstableV1
                } else {
                    panic!("No supported Wayland protocols found to capture screen contents, set capturer=\"none\" in the config, or report an issue if you believe it's a mistake");
                }
            }
        };
        log::debug!("Using {protocol_to_use} protocol to request frames");

        self.vulkan = Some(Vulkan::new().expect("Unable to initialize Vulkan"));
        self.controller = Some(controller);

        loop {
            if !self.is_processing_frame {
                if let Some(output) = self.output.as_ref() {
                    match protocol_to_use {
                        WaylandProtocol::ExtImageCopyCaptureV1 => {
                            if self.img_copy_capture_session.is_none() {
                                let capture_src = self
                                    .img_capture_source_manager
                                    .as_ref()
                                    .unwrap()
                                    .create_source(output, &event_queue.handle(), ());

                                self.img_copy_capture_session = Some(
                                    self.img_copy_capture_manager
                                        .as_ref()
                                        .unwrap()
                                        .create_session(
                                            &capture_src,
                                            Options::empty(),
                                            &event_queue.handle(),
                                            (),
                                        ),
                                );
                            }

                            if let Some(buffer) = self.wl_buffer.as_ref() {
                                let frame = self
                                    .img_copy_capture_session
                                    .as_ref()
                                    .unwrap()
                                    .create_frame(&event_queue.handle(), ());
                                let pending_frame = self.pending_frame.as_ref().unwrap();
                                frame.attach_buffer(buffer);
                                frame.damage_buffer(
                                    0,
                                    0,
                                    pending_frame.width as i32,
                                    pending_frame.height as i32,
                                );
                                frame.capture();

                                self.is_processing_frame = true;
                            }
                        }
                        WaylandProtocol::WlrScreencopyUnstableV1 => {
                            self.screencopy_manager.as_ref().unwrap().capture_output(
                                0,
                                output,
                                &event_queue.handle(),
                                (),
                            );
                            self.is_processing_frame = true;
                        }
                        WaylandProtocol::WlrExportDmabufUnstableV1 => {
                            self.dmabuf_manager.as_ref().unwrap().capture_output(
                                0,
                                output,
                                &event_queue.handle(),
                                (),
                            );
                            self.is_processing_frame = true;
                        }
                        WaylandProtocol::Any => unreachable!(),
                    }
                }
            }

            event_queue
                .blocking_dispatch(self)
                .expect("Error running wayland capturer main loop");
        }
    }
}

// ==== Globals ====

impl Dispatch<WlOutput, GlobalsContext> for Capturer {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: <WlOutput as Proxy>::Event,
        ctx: &GlobalsContext,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_output::Event;

        match event {
            Event::Description { description } if description.contains(&ctx.desired_output) => {
                if state.output.is_none() {
                    log::debug!(
                        "Using output '{}' for config '{}'",
                        description,
                        ctx.desired_output,
                    );
                    state.output = Some(output.clone());
                    state.output_global_id = ctx.global_id;
                } else {
                    log::error!("Cannot use output '{}' for config '{}' because another output was already matched with it, skipping this output.", description, ctx.desired_output);
                }
            }

            _ => {}
        }
    }
}

impl Dispatch<WlRegistry, GlobalsContext> for Capturer {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: <WlRegistry as Proxy>::Event,
        ctx: &GlobalsContext,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_registry::Event;

        match event {
            Event::Global {
                name,
                interface,
                version,
            } => {
                match &interface[..] {
                    _ if interface == WlOutput::interface().name => {
                        registry.bind::<WlOutput, _, _>(
                            name,
                            version,
                            qh,
                            GlobalsContext {
                                global_id: Some(name),
                                desired_output: ctx.desired_output.clone(),
                            },
                        );
                    }
                    _ if interface == ZwlrExportDmabufManagerV1::interface().name => {
                        log::debug!("Detected support for wlr-export-dmabuf-unstable-v1 protocol");
                        state.dmabuf_manager = Some(
                            registry.bind::<ZwlrExportDmabufManagerV1, _, _>(name, version, qh, ()),
                        );
                    }
                    _ if interface == ZwpLinuxDmabufV1::interface().name => {
                        log::debug!("Detected support for linux-dmabuf-v1 protocol");
                        state.dmabuf =
                            Some(registry.bind::<ZwpLinuxDmabufV1, _, _>(name, version, qh, ()));
                    }
                    _ if interface == ZwlrScreencopyManagerV1::interface().name => {
                        log::debug!("Detected support for wlr-screencopy-unstable-v1 protocol");
                        state.screencopy_manager = Some(
                            registry.bind::<ZwlrScreencopyManagerV1, _, _>(name, version, qh, ()),
                        );
                    }
                    _ if interface == ExtOutputImageCaptureSourceManagerV1::interface().name => {
                        log::debug!("Detected support for ext-image-capture-source-v1 protocol");
                        state.img_capture_source_manager =
                            Some(registry.bind::<ExtOutputImageCaptureSourceManagerV1, _, _>(
                                name,
                                version,
                                qh,
                                (),
                            ));
                    }
                    _ if interface == ExtImageCopyCaptureManagerV1::interface().name => {
                        log::debug!("Detected support for ext-image-copy-capture-v1 protocol");
                        state.img_copy_capture_manager =
                            Some(registry.bind::<ExtImageCopyCaptureManagerV1, _, _>(
                                name,
                                version,
                                qh,
                                (),
                            ));
                    }
                    _ => {}
                };
            }

            Event::GlobalRemove { name } if Some(name) == state.output_global_id => {
                log::debug!("Disconnected screen {}", ctx.desired_output);
                state.output = None;
                state.output_global_id = None;
            }
            _ => {}
        }
    }
}

// ==== wlr-export-dmabuf-unstable-v1 protocol ====

impl Dispatch<ZwlrExportDmabufManagerV1, ()> for Capturer {
    fn event(
        _: &mut Self,
        _: &ZwlrExportDmabufManagerV1,
        _: <ZwlrExportDmabufManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrExportDmabufFrameV1, ()> for Capturer {
    fn event(
        state: &mut Self,
        frame: &ZwlrExportDmabufFrameV1,
        event: <ZwlrExportDmabufFrameV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_protocols_wlr::export_dmabuf::v1::client::zwlr_export_dmabuf_frame_v1::Event;

        match event {
            Event::Frame {
                width,
                height,
                num_objects,
                format,
                mod_high,
                mod_low,
                ..
            } => {
                let modifier = ((mod_high as u64) << 32) | mod_low as u64;
                log::trace!(
                    "wlr-export-dmabuf frame: DRM format={format}, size={width}x{height}, objects={num_objects}, modifier={modifier:#018x}"
                );
                if num_objects > 1 {
                    log::error!(
                        "The compositor sent a multi-object DMA-BUF, which wluma cannot import. Set WLR_DRM_NO_MODIFIERS=1 before launching the compositor"
                    );
                }
                let mut pending_frame = Object::new(width, height, num_objects, format);
                pending_frame.layout = Some((modifier, 0, 0));
                state.pending_frame = Some(pending_frame);
            }

            Event::Object {
                index,
                fd,
                size,
                offset,
                stride,
                ..
            } => {
                log::trace!(
                    "wlr-export-dmabuf object: index={index}, size={size}, offset={offset}, stride={stride}"
                );
                let pending_frame = state.pending_frame.as_mut().unwrap();
                pending_frame.layout = Some((pending_frame.layout.unwrap().0, offset, stride));
                pending_frame.set_object(index, fd, size);
            }

            Event::Ready { .. } => {
                let luma = state
                    .vulkan
                    .as_mut()
                    .unwrap()
                    .luma_percent_from_external_fd(&state.pending_frame.take().unwrap())
                    .expect("Unable to compute luma percent");

                // TODO: replace with await
                smol::block_on(state.controller.as_mut().unwrap().adjust(luma));

                frame.destroy();

                thread::sleep(DELAY_SUCCESS);
                state.is_processing_frame = false;
            }

            Event::Cancel { reason } => {
                log::debug!("Frame was cancelled, reason: {reason:?}");
                state.pending_frame.take();
                frame.destroy();

                thread::sleep(DELAY_FAILURE);
                state.is_processing_frame = false;
            }

            _ => unreachable!(),
        }
    }
}

// ==== linux-dmabuf-v1 protocol ====

impl Dispatch<ZwpLinuxDmabufV1, ()> for Capturer {
    fn event(
        _: &mut Self,
        _: &ZwpLinuxDmabufV1,
        event: <ZwpLinuxDmabufV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::Event;

        match event {
            Event::Format { format } => {
                log::debug!("linux-dmabuf-v1 advertised DRM format={format}");
            }
            Event::Modifier {
                format,
                modifier_hi,
                modifier_lo,
            } => {
                let modifier = ((modifier_hi as u64) << 32) | modifier_lo as u64;
                log::debug!(
                    "linux-dmabuf-v1 advertised DRM format={format}, modifier={modifier:#018x}"
                );
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, ()> for Capturer {
    fn event(
        _: &mut Self,
        _: &ZwpLinuxBufferParamsV1,
        _: <ZwpLinuxBufferParamsV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlBuffer, ()> for Capturer {
    fn event(
        _: &mut Self,
        _: &WlBuffer,
        _: <WlBuffer as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// ==== wlr-screencopy-unstable-v1 protocol ====

impl Dispatch<ZwlrScreencopyManagerV1, ()> for Capturer {
    fn event(
        _: &mut Self,
        _: &ZwlrScreencopyManagerV1,
        _: <ZwlrScreencopyManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for Capturer {
    fn event(
        state: &mut Self,
        frame: &ZwlrScreencopyFrameV1,
        event: <ZwlrScreencopyFrameV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::Event;

        match event {
            Event::LinuxDmabuf {
                width,
                height,
                format,
            } => {
                if let Some(pending_frame) = state.pending_frame.as_ref() {
                    if pending_frame.width != width
                        || pending_frame.height != height
                        || pending_frame.format != format
                    {
                        if let Some(buffer) = state.wl_buffer.take() {
                            buffer.destroy()
                        }
                    }
                }

                if state.wl_buffer.is_none() {
                    log::debug!(
                        "wlr-screencopy DMA-BUF constraints: DRM format={format}, size={width}x{height}, using explicit linear modifier"
                    );
                    let pending_frame = Object::new(width, height, 1, format);
                    let dmabuf_params = state.dmabuf.as_ref().unwrap().create_params(qh, ());
                    let allowed_modifiers = [0];
                    let (fd, offset, stride, modifier) = state
                        .vulkan
                        .as_mut()
                        .unwrap()
                        .init_exportable_frame_image(&pending_frame, &allowed_modifiers)
                        .expect("Unable to init exportable frame image");

                    let fd = unsafe { BorrowedFd::borrow_raw(fd) };

                    dmabuf_params.add(
                        fd,
                        0,
                        offset,
                        stride,
                        (modifier >> 32) as u32,
                        (modifier & 0xFFFFFFFF) as u32,
                    );

                    let wl_buffer = dmabuf_params.create_immed(
                        width as i32,
                        height as i32,
                        format,
                        Flags::empty(),
                        qh,
                        (),
                    );

                    dmabuf_params.destroy();
                    state.wl_buffer = Some(wl_buffer);
                    state.pending_frame = Some(pending_frame);
                }

                let buffer = state.wl_buffer.as_ref().unwrap();
                if frame.version() >= 2 {
                    frame.copy_with_damage(buffer);
                } else {
                    frame.copy(buffer);
                }
            }

            Event::Ready { .. } => {
                let luma = state
                    .vulkan
                    .as_mut()
                    .unwrap()
                    .luma_percent_from_internal_fd()
                    .expect("Unable to compute luma percent");

                // TODO: replace with await
                smol::block_on(state.controller.as_mut().unwrap().adjust(luma));

                frame.destroy();

                thread::sleep(DELAY_SUCCESS);
                state.is_processing_frame = false;
            }

            Event::Failed => {
                log::debug!("Frame copy failed");
                frame.destroy();

                if let Some(buffer) = state.wl_buffer.take() {
                    buffer.destroy()
                }

                thread::sleep(DELAY_FAILURE);
                state.is_processing_frame = false;
            }

            _ => {}
        }
    }
}

// ==== ext-image-capture-source-v1 protocol ====

impl Dispatch<ExtOutputImageCaptureSourceManagerV1, ()> for Capturer {
    fn event(
        _: &mut Self,
        _: &ExtOutputImageCaptureSourceManagerV1,
        _: <ExtOutputImageCaptureSourceManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCaptureSourceV1, ()> for Capturer {
    fn event(
        _: &mut Self,
        _: &ExtImageCaptureSourceV1,
        _: <ExtImageCaptureSourceV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// ==== ext-image-copy-capture-v1 protocol ====

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for Capturer {
    fn event(
        _: &mut Self,
        _: &ExtImageCopyCaptureManagerV1,
        _: <ExtImageCopyCaptureManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for Capturer {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: <ExtImageCopyCaptureSessionV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::Event;

        match event {
            Event::BufferSize { width, height } => {
                log::debug!("ext-image-copy-capture DMA-BUF size constraint: {width}x{height}");
                state.pending_frame = Some(Object::new(width, height, 1, 0));
            }

            Event::DmabufDevice { device } => {
                log::debug!(
                    "ext-image-copy-capture DMA-BUF allocation device dev_t bytes={device:02x?}"
                );
            }

            Event::DmabufFormat { format, modifiers } => {
                let modifiers: Vec<_> = modifiers
                    .chunks_exact(8)
                    .map(|bytes| u64::from_ne_bytes(bytes.try_into().unwrap()))
                    .collect();
                log::debug!(
                    "ext-image-copy-capture DMA-BUF format constraint: DRM format={format}, modifiers={modifiers:#x?}"
                );
                state.dmabuf_formats.push((format, modifiers));
            }

            Event::Done => {
                if let Some(buffer) = state.wl_buffer.take() {
                    buffer.destroy()
                }

                let dimensions = {
                    let pending_frame = state.pending_frame.as_ref().unwrap();
                    (pending_frame.width, pending_frame.height)
                };
                let selection = state.dmabuf_formats.iter().find_map(|(format, offered)| {
                    let supported = state
                        .vulkan
                        .as_ref()
                        .unwrap()
                        .exportable_modifiers(*format)
                        .ok()?;
                    let modifiers: Vec<_> = offered
                        .iter()
                        .copied()
                        .filter(|modifier| supported.contains(modifier))
                        .collect();
                    (!modifiers.is_empty()).then_some((*format, modifiers))
                });
                state.dmabuf_formats.clear();
                let (format, modifiers) = selection.expect(
                    "No compositor-provided DMA-BUF format and modifier can be exported by Vulkan",
                );
                log::debug!(
                    "ext-image-copy-capture selected DMA-BUF constraints: DRM format={format}, size={}x{}, modifiers={modifiers:#x?}",
                    dimensions.0,
                    dimensions.1,
                );
                state.pending_frame = Some(Object::new(dimensions.0, dimensions.1, 1, format));
                let pending_frame = state.pending_frame.as_ref().unwrap();

                let dmabuf_params = state.dmabuf.as_ref().unwrap().create_params(qh, ());
                let (fd, offset, stride, modifier) = state
                    .vulkan
                    .as_mut()
                    .unwrap()
                    .init_exportable_frame_image(pending_frame, &modifiers)
                    .expect("Unable to init exportable frame image");

                let fd = unsafe { BorrowedFd::borrow_raw(fd) };

                dmabuf_params.add(
                    fd,
                    0,
                    offset,
                    stride,
                    (modifier >> 32) as u32,
                    (modifier & 0xFFFFFFFF) as u32,
                );

                let wl_buffer = dmabuf_params.create_immed(
                    pending_frame.width as i32,
                    pending_frame.height as i32,
                    pending_frame.format,
                    Flags::empty(),
                    qh,
                    (),
                );

                dmabuf_params.destroy();

                state.wl_buffer = Some(wl_buffer);
            }

            Event::Stopped => {
                log::debug!("Image copy session stopped");
                state.img_copy_capture_session.take().unwrap().destroy();
                if let Some(buffer) = state.wl_buffer.take() {
                    buffer.destroy()
                }

                thread::sleep(DELAY_FAILURE);
                state.is_processing_frame = false;
            }

            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for Capturer {
    fn event(
        state: &mut Self,
        frame: &ExtImageCopyCaptureFrameV1,
        event: <ExtImageCopyCaptureFrameV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::Event;

        match event {
            Event::Ready => {
                let luma = state
                    .vulkan
                    .as_mut()
                    .unwrap()
                    .luma_percent_from_internal_fd()
                    .expect("Unable to compute luma percent");

                // TODO: replace with await
                smol::block_on(state.controller.as_mut().unwrap().adjust(luma));

                frame.destroy();

                thread::sleep(DELAY_SUCCESS);
                state.is_processing_frame = false;
            }

            Event::Failed { reason } => {
                log::debug!("Frame copy failed, reason: {reason:?}");
                frame.destroy();

                thread::sleep(DELAY_FAILURE);
                state.is_processing_frame = false;
            }

            _ => {}
        }
    }
}
