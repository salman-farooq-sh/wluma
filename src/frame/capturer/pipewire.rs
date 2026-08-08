use crate::frame::object::Object;
use crate::frame::vulkan::Vulkan;
use crate::predictor::Controller;
use anyhow::{anyhow, Result};
use drm_fourcc::DrmFourcc;
use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;
use std::os::fd::BorrowedFd;

mod kde;
mod mutter;

pub fn run(output_name: &str, controller: Controller) {
    let node = kde::node(output_name)
        .and_then(|(node, connector)| match node {
            Some(node) => Ok(node),
            None => mutter::node(connector.as_deref().unwrap_or(output_name)),
        })
        .unwrap_or_else(|error| panic!("Unable to create PipeWire screen stream: {error:#}"));
    capture(node, controller)
        .unwrap_or_else(|error| panic!("Unable to capture PipeWire screen stream: {error:#}"));
}

struct Data {
    controller: Controller,
    format: spa::param::video::VideoInfoRaw,
    vulkan: Vulkan,
}

fn capture(node: u32, controller: Controller) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "wluma",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;
    let data = Data {
        controller,
        format: Default::default(),
        vulkan: Vulkan::new()?,
    };
    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, _, old, new| match new {
            pw::stream::StreamState::Error(error) => panic!("PipeWire stream failed: {error}"),
            pw::stream::StreamState::Unconnected if old != pw::stream::StreamState::Unconnected => {
                panic!("PipeWire stream disconnected");
            }
            _ => {}
        })
        .param_changed(|_, data, id, param| {
            if id == spa::param::ParamType::Format.as_raw() {
                if let Some(param) = param {
                    data.format
                        .parse(param)
                        .expect("Unable to parse PipeWire video format");
                }
            }
        })
        .process(|stream, state| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.len() != 1 || datas[0].type_() != spa::buffer::DataType::DmaBuf {
                panic!("PipeWire compositor did not provide a single-plane DMA-BUF");
            }
            let data = &datas[0];
            let chunk = data.chunk();
            if chunk.size() == 0 || chunk.flags().contains(spa::buffer::ChunkFlags::CORRUPTED) {
                return;
            }
            let stride = u32::try_from(chunk.stride())
                .ok()
                .filter(|stride| *stride > 0)
                .expect("PipeWire DMA-BUF has an invalid stride");
            if data.fd() < 0 {
                panic!("PipeWire DMA-BUF has an invalid file descriptor");
            }
            let fd = unsafe { BorrowedFd::borrow_raw(data.fd()) }
                .try_clone_to_owned()
                .expect("Unable to duplicate PipeWire DMA-BUF");
            let mut object = Object::new(
                state.format.size().width,
                state.format.size().height,
                1,
                drm_format(state.format.format())
                    .expect("PipeWire negotiated an unsupported video format")
                    as u32,
            );
            object.layout = Some((state.format.modifier(), chunk.offset(), stride));
            object.set_object(0, fd, data.as_raw().maxsize);
            let luma = state
                .vulkan
                .luma_percent_from_external_fd(&object)
                .expect("Unable to process PipeWire DMA-BUF with Vulkan");
            smol::block_on(state.controller.adjust(luma));
        })
        .register()?;

    let format = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::RGBx,
        ),
        modifier_property(),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle {
                width: 1920,
                height: 1080
            },
            pw::spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            pw::spa::utils::Rectangle {
                width: 16384,
                height: 16384
            }
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: 10, denom: 1 },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction { num: 10, denom: 1 }
        ),
    );
    let values = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(format),
    )?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).ok_or_else(|| anyhow!("Invalid PipeWire format"))?];
    stream.connect(
        spa::utils::Direction::Input,
        Some(node),
        pw::stream::StreamFlags::AUTOCONNECT,
        &mut params,
    )?;
    mainloop.run();
    Ok(())
}

fn modifier_property() -> spa::pod::Property {
    spa::pod::Property {
        key: spa::param::format::FormatProperties::VideoModifier.as_raw(),
        flags: spa::pod::PropertyFlags::MANDATORY | spa::pod::PropertyFlags::DONT_FIXATE,
        value: spa::pod::Value::Choice(spa::pod::ChoiceValue::Long(spa::utils::Choice(
            spa::utils::ChoiceFlags::empty(),
            spa::utils::ChoiceEnum::Enum {
                default: 0,
                alternatives: vec![0],
            },
        ))),
    }
}

fn drm_format(format: spa::param::video::VideoFormat) -> Result<DrmFourcc> {
    match format {
        spa::param::video::VideoFormat::BGRx => Ok(DrmFourcc::Xrgb8888),
        spa::param::video::VideoFormat::RGBx => Ok(DrmFourcc::Xbgr8888),
        _ => Err(anyhow!("Unsupported PipeWire video format {format:?}")),
    }
}
