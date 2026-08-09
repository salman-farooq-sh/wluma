use crate::frame::compute_perceived_lightness_percent;
use crate::frame::object::Object;
use anyhow::{anyhow, Result};
use ash::ext::image_drm_format_modifier::Device as DrmModifierDevice;
use ash::khr::external_memory_fd::Device as KHRDevice;
use ash::{vk, Device, Entry, Instance};
use drm_fourcc::DrmFourcc;
use std::cell::RefCell;
use std::collections::HashMap;
use std::default::Default;
use std::ffi::{CStr, CString};
use std::ops::Drop;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const VULKAN_VERSION: u32 = vk::make_api_version(0, 1, 2, 0);

const FINAL_MIP_LEVEL: u32 = 4; // Don't generate mipmaps beyond this level - GPU is doing too poor of a job averaging the colors
const FENCES_TIMEOUT_NS: u64 = 1_000_000_000;

pub struct Vulkan {
    _entry: Entry, // must keep reference to prevent early memory release
    instance: Instance,
    device: Device,
    physical_device: vk::PhysicalDevice,
    khr_device: KHRDevice,
    drm_modifier_device: Option<DrmModifierDevice>,
    supports_drm_modifier: bool,
    modifier_cache: RefCell<HashMap<(u32, u32), Vec<u64>>>,
    buffer: Option<vk::Buffer>,
    buffer_memory: Option<vk::DeviceMemory>,
    command_pool: vk::CommandPool,
    command_buffers: Vec<vk::CommandBuffer>,
    queue: vk::Queue,
    fence: vk::Fence,
    image: Option<vk::Image>,
    image_memory: Option<vk::DeviceMemory>,
    image_resolution: Option<(u32, u32, u32)>,
    exportable_frame_image: Option<vk::Image>,
    exportable_frame_image_memory: Option<vk::DeviceMemory>,
    exportable_frame_image_fd: Option<OwnedFd>,
}

impl Vulkan {
    pub fn new() -> Result<Self> {
        let app_name = CString::new("wluma")?;
        let app_version: u32 = vk::make_api_version(
            0,
            env!("WLUMA_VERSION_MAJOR").parse()?,
            env!("WLUMA_VERSION_MINOR").parse()?,
            env!("WLUMA_VERSION_PATCH").parse()?,
        );

        let app_info = vk::ApplicationInfo::default()
            .application_name(&app_name)
            .application_version(app_version)
            .engine_name(&app_name)
            .engine_version(app_version)
            .api_version(VULKAN_VERSION);

        let instance_extensions = &[
            vk::KHR_EXTERNAL_MEMORY_CAPABILITIES_NAME.as_ptr(),
            vk::KHR_GET_PHYSICAL_DEVICE_PROPERTIES2_NAME.as_ptr(),
        ];

        let entry = Entry::linked();

        let create_info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_extension_names(instance_extensions);

        let instance = unsafe {
            entry
                .create_instance(&create_info, None)
                .map_err(anyhow::Error::msg)?
        };

        let physical_devices = unsafe {
            instance
                .enumerate_physical_devices()
                .map_err(anyhow::Error::msg)?
        };
        for (index, physical_device) in physical_devices.iter().enumerate() {
            let properties = unsafe { instance.get_physical_device_properties(*physical_device) };
            let name = unsafe { CStr::from_ptr(properties.device_name.as_ptr()) }.to_string_lossy();
            log::debug!(
                "Discovered Vulkan device {index}: '{name}', type={}, API {}.{}.{}, driver version {}",
                properties.device_type.as_raw(),
                vk::api_version_major(properties.api_version),
                vk::api_version_minor(properties.api_version),
                vk::api_version_patch(properties.api_version),
                properties.driver_version,
            );
        }
        let physical_device = *physical_devices
            .first()
            .ok_or(anyhow!("Unable to find a physical device"))?;
        let physical_device_properties =
            unsafe { instance.get_physical_device_properties(physical_device) };
        let physical_device_name =
            unsafe { CStr::from_ptr(physical_device_properties.device_name.as_ptr()) }
                .to_string_lossy();

        let queue_family_index =
            unsafe { instance.get_physical_device_queue_family_properties(physical_device) }
                .iter()
                .position(|properties| properties.queue_flags.contains(vk::QueueFlags::TRANSFER))
                .ok_or(anyhow!(
                    "Vulkan device has no transfer-capable queue family"
                ))? as u32;
        let queue_info = &[vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family_index)
            .queue_priorities(&[1.0])];

        let mut device_extensions = vec![
            vk::KHR_EXTERNAL_MEMORY_FD_NAME.as_ptr(),
            vk::EXT_EXTERNAL_MEMORY_DMA_BUF_NAME.as_ptr(),
        ];
        let supports_drm_modifier = unsafe {
            instance
                .enumerate_device_extension_properties(physical_device)
                .map_err(anyhow::Error::msg)?
                .iter()
                .any(|extension| {
                    CStr::from_ptr(extension.extension_name.as_ptr())
                        == vk::EXT_IMAGE_DRM_FORMAT_MODIFIER_NAME
                })
        };
        if supports_drm_modifier {
            device_extensions.push(vk::EXT_IMAGE_DRM_FORMAT_MODIFIER_NAME.as_ptr());
        }
        log::debug!(
            "Using Vulkan device '{physical_device_name}', API {}.{}.{}, driver version {}, transfer queue family {}, DRM modifier extension={supports_drm_modifier}",
            vk::api_version_major(physical_device_properties.api_version),
            vk::api_version_minor(physical_device_properties.api_version),
            vk::api_version_patch(physical_device_properties.api_version),
            physical_device_properties.driver_version,
            queue_family_index,
        );
        let features = vk::PhysicalDeviceFeatures::default();

        let device_create_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(queue_info)
            .enabled_extension_names(&device_extensions)
            .enabled_features(&features);

        let device = unsafe {
            instance
                .create_device(physical_device, &device_create_info, None)
                .map_err(anyhow::Error::msg)?
        };

        let khr_device = KHRDevice::new(&instance, &device);
        let drm_modifier_device =
            supports_drm_modifier.then(|| DrmModifierDevice::new(&instance, &device));

        let queue = unsafe { device.get_device_queue(queue_family_index, 0) };

        let pool_create_info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
            .queue_family_index(queue_family_index);

        let command_pool = unsafe {
            device
                .create_command_pool(&pool_create_info, None)
                .map_err(anyhow::Error::msg)?
        };

        let command_buffer_allocate_info = vk::CommandBufferAllocateInfo::default()
            .command_buffer_count(1)
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY);

        let command_buffers = unsafe {
            device
                .allocate_command_buffers(&command_buffer_allocate_info)
                .map_err(anyhow::Error::msg)?
        };

        let fence_create_info = vk::FenceCreateInfo::default();
        let fence = unsafe {
            device
                .create_fence(&fence_create_info, None)
                .map_err(anyhow::Error::msg)?
        };

        Ok(Self {
            _entry: entry,
            instance,
            physical_device,
            device,
            khr_device,
            drm_modifier_device,
            supports_drm_modifier,
            modifier_cache: RefCell::new(HashMap::new()),
            command_pool,
            command_buffers,
            queue,
            fence,
            image: None,
            image_memory: None,
            image_resolution: None,
            buffer: None,
            buffer_memory: None,
            exportable_frame_image: None,
            exportable_frame_image_memory: None,
            exportable_frame_image_fd: None,
        })
    }

    pub fn importable_modifiers(&self, format: u32) -> Result<Vec<u64>> {
        self.dma_buf_modifiers(format, vk::ExternalMemoryFeatureFlags::IMPORTABLE)
    }

    pub fn exportable_modifiers(&self, format: u32) -> Result<Vec<u64>> {
        self.dma_buf_modifiers(format, vk::ExternalMemoryFeatureFlags::EXPORTABLE)
    }

    fn dma_buf_modifiers(
        &self,
        format: u32,
        required_external_feature: vk::ExternalMemoryFeatureFlags,
    ) -> Result<Vec<u64>> {
        let cache_key = (format, required_external_feature.as_raw());
        if let Some(modifiers) = self.modifier_cache.borrow().get(&cache_key) {
            return Ok(modifiers.clone());
        }
        if !self.supports_drm_modifier {
            self.modifier_cache
                .borrow_mut()
                .insert(cache_key, Vec::new());
            return Ok(Vec::new());
        }

        let drm_format = DrmFourcc::try_from(format)?;
        log::debug!(
            "Querying Vulkan DMA-BUF modifiers for DRM format {drm_format}, required_external_feature={:#x}",
            required_external_feature.as_raw(),
        );
        let format = map_drm_format(format)?;
        let mut modifier_list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut format_properties = vk::FormatProperties2::default().push_next(&mut modifier_list);
        unsafe {
            self.instance.get_physical_device_format_properties2(
                self.physical_device,
                format,
                &mut format_properties,
            );
        }

        let mut modifiers = vec![
            vk::DrmFormatModifierPropertiesEXT::default();
            modifier_list.drm_format_modifier_count as usize
        ];
        let mut modifier_list = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(&mut modifiers);
        let mut format_properties = vk::FormatProperties2::default().push_next(&mut modifier_list);
        unsafe {
            self.instance.get_physical_device_format_properties2(
                self.physical_device,
                format,
                &mut format_properties,
            );
        }

        let required_format_features = vk::FormatFeatureFlags::TRANSFER_SRC
            | vk::FormatFeatureFlags::BLIT_SRC
            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR;
        let mut supported = Vec::new();
        for modifier in modifiers {
            if self.supports_dma_buf_modifier(
                format,
                &modifier,
                required_format_features,
                required_external_feature,
            ) {
                supported.push(modifier.drm_format_modifier);
            }
        }
        log::debug!(
            "Vulkan supported DMA-BUF modifiers: DRM format={drm_format}, Vulkan format={}, required_external_feature={:#x}, modifiers={supported:#x?}",
            format.as_raw(),
            required_external_feature.as_raw(),
        );
        self.modifier_cache
            .borrow_mut()
            .insert(cache_key, supported.clone());
        Ok(supported)
    }

    fn supports_dma_buf_modifier(
        &self,
        format: vk::Format,
        modifier: &vk::DrmFormatModifierPropertiesEXT,
        required_format_features: vk::FormatFeatureFlags,
        required_external_feature: vk::ExternalMemoryFeatureFlags,
    ) -> bool {
        let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
            .drm_format_modifier(modifier.drm_format_modifier)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let format_info = vk::PhysicalDeviceImageFormatInfo2::default()
            .push_next(&mut external_info)
            .push_next(&mut modifier_info)
            .format(format)
            .ty(vk::ImageType::TYPE_2D)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::TRANSFER_SRC);
        let mut external_properties = vk::ExternalImageFormatProperties::default();
        let mut format_properties =
            vk::ImageFormatProperties2::default().push_next(&mut external_properties);

        let result = unsafe {
            self.instance.get_physical_device_image_format_properties2(
                self.physical_device,
                &format_info,
                &mut format_properties,
            )
        };
        let properties = external_properties.external_memory_properties;
        let supported = result.is_ok()
            && modifier.drm_format_modifier_plane_count == 1
            && modifier
                .drm_format_modifier_tiling_features
                .contains(required_format_features)
            && properties
                .external_memory_features
                .contains(required_external_feature)
            && properties
                .compatible_handle_types
                .contains(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        log::debug!(
            "Vulkan DMA-BUF modifier capability: Vulkan format={}, modifier={:#018x}, planes={}, tiling_features={:#x}, required_tiling_features={:#x}, query={result:?}, external_memory_features={:#x}, required_external_feature={:#x}, compatible_handle_types={:#x}, export_from_imported_handle_types={:#x}, supported={supported}",
            format.as_raw(),
            modifier.drm_format_modifier,
            modifier.drm_format_modifier_plane_count,
            modifier.drm_format_modifier_tiling_features.as_raw(),
            required_format_features.as_raw(),
            properties.external_memory_features.as_raw(),
            required_external_feature.as_raw(),
            properties.compatible_handle_types.as_raw(),
            properties.export_from_imported_handle_types.as_raw(),
        );
        supported
    }

    pub fn luma_percent_from_external_fd(&mut self, frame: &Object) -> Result<u8> {
        let (frame_image, frame_image_memory) = self.init_frame_image(frame)?;

        let result = self.luma_percent(&frame_image)?;

        unsafe {
            self.device.destroy_image(frame_image, None);
            self.device.free_memory(frame_image_memory, None);
        }

        Ok(result)
    }

    pub fn luma_percent_from_internal_fd(&mut self) -> Result<u8> {
        let frame_image = self.exportable_frame_image.unwrap();

        let result = self.luma_percent(&frame_image)?;

        Ok(result)
    }

    fn luma_percent(&self, frame_image: &vk::Image) -> Result<u8> {
        let image = self
            .image
            .ok_or(anyhow!("Unable to borrow the Vulkan image"))?;
        let buffer_memory = self
            .buffer_memory
            .ok_or(anyhow!("Unable to borrow buffer memory"))?;

        self.begin_commands()?;

        self.add_barrier(
            frame_image,
            0,
            1,
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::AccessFlags::default(),
            vk::AccessFlags::TRANSFER_READ,
            vk::PipelineStageFlags::TOP_OF_PIPE,
        );

        let (target_mip_level, mip_width, mip_height) = self.generate_mipmaps(frame_image, &image);

        self.copy_mipmap(&image, target_mip_level, mip_width, mip_height)?;

        self.submit_commands()?;

        let pixels = mip_width as usize * mip_height as usize;
        let rgbas = unsafe {
            let buffer_pointer = self
                .device
                .map_memory(
                    buffer_memory,
                    0,
                    vk::WHOLE_SIZE,
                    vk::MemoryMapFlags::empty(),
                )
                .map_err(anyhow::Error::msg)?;
            std::slice::from_raw_parts(buffer_pointer as *mut u8, pixels * 4)
        };

        let result = compute_perceived_lightness_percent(rgbas, true, pixels);

        unsafe {
            self.device.unmap_memory(buffer_memory);
        }

        Ok(result)
    }

    fn init_image(&mut self, frame: &Object) -> Result<()> {
        let mip_levels = f64::max(frame.width.into(), frame.height.into())
            .log2()
            .floor() as u32;

        if let Some((w, h, _)) = self.image_resolution {
            if (w, h) == (frame.width, frame.height) {
                // Image is already initialized, resolution did not change
                return Ok(());
            }
        }

        let image_create_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .extent(vk::Extent3D {
                width: frame.width,
                height: frame.height,
                depth: 1,
            })
            .mip_levels(mip_levels)
            .array_layers(1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .samples(vk::SampleCountFlags::TYPE_1)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);

        let image = unsafe {
            self.device
                .create_image(&image_create_info, None)
                .map_err(anyhow::Error::msg)?
        };
        let image_memory_req = unsafe { self.device.get_image_memory_requirements(image) };

        let image_allocate_info = vk::MemoryAllocateInfo::default()
            .allocation_size(image_memory_req.size)
            .memory_type_index(0);

        let image_memory = unsafe {
            self.device
                .allocate_memory(&image_allocate_info, None)
                .map_err(anyhow::Error::msg)?
        };

        unsafe {
            self.device
                .bind_image_memory(image, image_memory, 0)
                .map_err(anyhow::Error::msg)?
        };

        if let Some(old_image) = self.image.replace(image) {
            unsafe {
                self.device.destroy_image(old_image, None);
            }
        }
        if let Some(old_image_memory) = self.image_memory.replace(image_memory) {
            unsafe {
                self.device.free_memory(old_image_memory, None);
            }
        }

        let buffer_size = 4
            * (frame.width >> (mip_levels - FINAL_MIP_LEVEL))
            * (frame.height >> (mip_levels - FINAL_MIP_LEVEL));

        let buffer_info = vk::BufferCreateInfo::default()
            .size(buffer_size as u64)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);

        let buffer = unsafe {
            self.device
                .create_buffer(&buffer_info, None)
                .map_err(anyhow::Error::msg)?
        };

        let buffer_memory_req = unsafe { self.device.get_buffer_memory_requirements(buffer) };

        let device_memory_properties = unsafe {
            self.instance
                .get_physical_device_memory_properties(self.physical_device)
        };

        let memory_type_index = find_memory_type_index(
            &buffer_memory_req,
            &device_memory_properties,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .ok_or(anyhow!(
            "Unable to find suitable memory type for the buffer"
        ))?;

        let allocate_info = vk::MemoryAllocateInfo {
            allocation_size: buffer_memory_req.size,
            memory_type_index,
            ..Default::default()
        };

        let buffer_memory = unsafe {
            self.device
                .allocate_memory(&allocate_info, None)
                .map_err(anyhow::Error::msg)?
        };

        unsafe {
            self.device
                .bind_buffer_memory(buffer, buffer_memory, 0)
                .map_err(anyhow::Error::msg)?
        };

        if let Some(buffer) = self.buffer.replace(buffer) {
            unsafe {
                self.device.destroy_buffer(buffer, None);
            }
        }
        if let Some(buffer_memory) = self.buffer_memory.replace(buffer_memory) {
            unsafe {
                self.device.free_memory(buffer_memory, None);
            }
        }

        self.image_resolution
            .replace((frame.width, frame.height, mip_levels));

        Ok(())
    }

    fn init_frame_image(&mut self, frame: &Object) -> Result<(vk::Image, vk::DeviceMemory)> {
        if frame.layout.is_some() && !self.supports_drm_modifier {
            return Err(anyhow!(
                "Vulkan device does not support DRM format modifiers"
            ));
        }
        assert_eq!(
            1, frame.num_objects,
            "Frames with multiple objects are not supported yet, use WLR_DRM_NO_MODIFIERS=1 as described in README and follow issue #8"
        );

        // External memory info
        let mut frame_image_memory_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let (modifier, offset, stride) = frame.layout.unwrap_or_default();
        log::trace!(
            "Importing DMA-BUF into Vulkan: DRM format={}, size={}x{}, objects={}, modifier={modifier:#018x}, offset={offset}, stride={stride}, object_size={}",
            frame.format,
            frame.width,
            frame.height,
            frame.num_objects,
            frame.sizes[0],
        );
        if frame.layout.is_some() && !self.importable_modifiers(frame.format)?.contains(&modifier) {
            return Err(anyhow!(
                "Vulkan cannot import DRM format {} with modifier {modifier:#018x} as a transfer source",
                frame.format
            ));
        }
        let plane_layouts = [vk::SubresourceLayout {
            offset: offset as u64,
            size: 0,
            row_pitch: stride as u64,
            array_pitch: 0,
            depth_pitch: 0,
        }];
        let mut modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(modifier)
            .plane_layouts(&plane_layouts);

        // Image create info
        let mut frame_image_create_info = vk::ImageCreateInfo::default()
            .push_next(&mut frame_image_memory_info)
            .image_type(vk::ImageType::TYPE_2D)
            .format(map_drm_format(frame.format)?)
            .extent(vk::Extent3D {
                width: frame.width,
                height: frame.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .tiling(if frame.layout.is_some() {
                vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT
            } else {
                vk::ImageTiling::LINEAR
            })
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .samples(vk::SampleCountFlags::TYPE_1)
            .usage(vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        if frame.layout.is_some() {
            frame_image_create_info = frame_image_create_info.push_next(&mut modifier_info);
        }

        let frame_image = unsafe {
            self.device
                .create_image(&frame_image_create_info, None)
                .map_err(anyhow::Error::msg)?
        };

        // Memory requirements info
        let frame_image_memory_req_info =
            vk::ImageMemoryRequirementsInfo2::default().image(frame_image);

        // Prepare the structures to get memory requirements into, then get the memory requirements
        let mut frame_image_mem_dedicated_req = vk::MemoryDedicatedRequirements::default();

        let mut frame_image_mem_req =
            vk::MemoryRequirements2::default().push_next(&mut frame_image_mem_dedicated_req);

        unsafe {
            self.device.get_image_memory_requirements2(
                &frame_image_memory_req_info,
                &mut frame_image_mem_req,
            );
        }

        // Bit i in memory_type_bits is set if the ith memory type in the
        // VkPhysicalDeviceMemoryProperties structure is supported for the image memory.
        // We just use the first type supported (from least significant bit's side)

        // Find suitable memory type index
        let memory_type_index = memory_type_index(
            frame_image_mem_req.memory_requirements.memory_type_bits,
            "imported frame image",
        )?;

        // Import memory app_info
        // Construct the memory alloctation info according to the requirements
        // If the image needs dedicated memory, add MemoryDedicatedAllocateInfo to the info chain
        let mut frame_import_memory_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(frame.fds[0]);

        // dedicated allocation info
        let mut frame_image_memory_dedicated_info =
            vk::MemoryDedicatedAllocateInfo::default().image(frame_image);

        // Memory allocate info
        let mut frame_image_allocate_info = vk::MemoryAllocateInfo::default()
            .push_next(&mut frame_import_memory_info)
            .allocation_size(frame_image_mem_req.memory_requirements.size)
            .memory_type_index(memory_type_index);

        if frame_image_mem_dedicated_req.requires_dedicated_allocation == vk::TRUE
            || frame_image_mem_dedicated_req.prefers_dedicated_allocation == vk::TRUE
        {
            frame_image_allocate_info =
                frame_image_allocate_info.push_next(&mut frame_image_memory_dedicated_info);
        }

        // Allocate memory and bind it to the image
        let frame_image_memory = unsafe {
            self.device
                .allocate_memory(&frame_image_allocate_info, None)
                .map_err(anyhow::Error::msg)?
        };

        unsafe {
            self.device
                .bind_image_memory(frame_image, frame_image_memory, 0)
                .map_err(anyhow::Error::msg)?;
        };

        // Also ensure the internal image is initialized with the same dimensions
        self.init_image(frame)?;

        Ok((frame_image, frame_image_memory))
    }

    pub fn init_exportable_frame_image(
        &mut self,
        frame: &Object,
        allowed_modifiers: &[u64],
    ) -> Result<(i32, u32, u32, u64)> {
        assert_eq!(
            1, frame.num_objects,
            "Frames with multiple objects are not supported yet, use WLR_DRM_NO_MODIFIERS=1 as described in README and follow issue #8"
        );

        let supported_modifiers = self.exportable_modifiers(frame.format)?;
        let modifiers: Vec<_> = allowed_modifiers
            .iter()
            .copied()
            .filter(|modifier| supported_modifiers.contains(modifier))
            .collect();
        if modifiers.is_empty() {
            return Err(anyhow!(
                "No compositor-provided DRM modifier for format {} can be exported by Vulkan",
                frame.format
            ));
        }

        let mut frame_image_memory_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut modifier_info =
            vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&modifiers);

        let frame_image_create_info = vk::ImageCreateInfo::default()
            .push_next(&mut frame_image_memory_info)
            .push_next(&mut modifier_info)
            .image_type(vk::ImageType::TYPE_2D)
            .format(map_drm_format(frame.format)?)
            .extent(vk::Extent3D {
                width: frame.width,
                height: frame.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .samples(vk::SampleCountFlags::TYPE_1)
            .usage(vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);

        let frame_image = unsafe {
            self.device
                .create_image(&frame_image_create_info, None)
                .map_err(anyhow::Error::msg)?
        };

        // Memory requirements info
        let frame_image_memory_req_info =
            vk::ImageMemoryRequirementsInfo2::default().image(frame_image);

        // Prepare the structures to get memory requirements into, then get the memory requirements
        let mut frame_image_mem_dedicated_req = vk::MemoryDedicatedRequirements::default();

        let mut frame_image_mem_req =
            vk::MemoryRequirements2::default().push_next(&mut frame_image_mem_dedicated_req);

        unsafe {
            self.device.get_image_memory_requirements2(
                &frame_image_memory_req_info,
                &mut frame_image_mem_req,
            );
        }

        // Bit i in memory_type_bits is set if the ith memory type in the
        // VkPhysicalDeviceMemoryProperties structure is supported for the image memory.
        // We just use the first type supported (from least significant bit's side)

        // Find suitable memory type index
        let memory_requirements = frame_image_mem_req.memory_requirements;
        let dedicated_required =
            frame_image_mem_dedicated_req.requires_dedicated_allocation == vk::TRUE;
        let dedicated_preferred =
            frame_image_mem_dedicated_req.prefers_dedicated_allocation == vk::TRUE;
        let memory_type_index =
            memory_type_index(memory_requirements.memory_type_bits, "exported frame image")?;

        // Specify that the memory can be exported
        let mut frame_import_memory_info = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

        // dedicated allocation info
        let mut frame_image_memory_dedicated_info =
            vk::MemoryDedicatedAllocateInfo::default().image(frame_image);

        // Allocate memory
        let mut frame_image_allocate_info = vk::MemoryAllocateInfo::default()
            .push_next(&mut frame_import_memory_info)
            .allocation_size(memory_requirements.size)
            .memory_type_index(memory_type_index);

        if dedicated_required || dedicated_preferred {
            frame_image_allocate_info =
                frame_image_allocate_info.push_next(&mut frame_image_memory_dedicated_info);
        }

        // Allocate memory and bind it to the image
        let frame_image_memory = unsafe {
            self.device
                .allocate_memory(&frame_image_allocate_info, None)
                .map_err(anyhow::Error::msg)?
        };

        // Bind memory to the image
        unsafe {
            self.device
                .bind_image_memory(frame_image, frame_image_memory, 0)
                .map_err(anyhow::Error::msg)?;
        }

        // Get the file descriptor
        let memory_fd_info = vk::MemoryGetFdInfoKHR::default()
            .memory(frame_image_memory)
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

        let fd = unsafe {
            OwnedFd::from_raw_fd(
                self.khr_device
                    .get_memory_fd(&memory_fd_info)
                    .map_err(anyhow::Error::msg)?,
            )
        };

        let subresource = vk::ImageSubresource::default()
            .aspect_mask(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT)
            .mip_level(0)
            .array_layer(0);

        let layout = unsafe {
            self.device
                .get_image_subresource_layout(frame_image, subresource)
        };

        let offset = u32::try_from(layout.offset).map_err(|_| {
            anyhow!(
                "Vulkan DMA-BUF plane offset {} does not fit the Wayland protocol",
                layout.offset
            )
        })?;
        let stride = u32::try_from(layout.row_pitch).map_err(|_| {
            anyhow!(
                "Vulkan DMA-BUF row pitch {} does not fit the Wayland protocol",
                layout.row_pitch
            )
        })?;
        let mut modifier_properties = vk::ImageDrmFormatModifierPropertiesEXT::default();
        unsafe {
            self.drm_modifier_device
                .as_ref()
                .ok_or(anyhow!(
                    "Vulkan device does not support DRM format modifiers"
                ))?
                .get_image_drm_format_modifier_properties(frame_image, &mut modifier_properties)
                .map_err(anyhow::Error::msg)?;
        }
        let modifier = modifier_properties.drm_format_modifier;
        log::debug!(
            "Exporting Vulkan DMA-BUF: DRM format={}, size={}x{}, modifier={modifier:#018x}, memory_planes={}, offset={offset}, stride={stride}, layout_size={}, allocation_size={}, memory_type={memory_type_index}, dedicated_required={}, dedicated_preferred={}",
            frame.format,
            frame.width,
            frame.height,
            1,
            layout.size,
            memory_requirements.size,
            dedicated_required,
            dedicated_preferred,
        );

        let raw_fd = fd.as_raw_fd();

        if let Some(old_image) = self.exportable_frame_image.replace(frame_image) {
            unsafe {
                self.device.destroy_image(old_image, None);
            }
        };

        if let Some(old_image_memory) = self
            .exportable_frame_image_memory
            .replace(frame_image_memory)
        {
            unsafe {
                self.device.free_memory(old_image_memory, None);
            }
        }

        self.exportable_frame_image_fd = Some(fd);

        // Also ensure the internal image is initialized with the same dimensions
        self.init_image(frame)?;

        Ok((raw_fd, offset, stride, modifier))
    }

    #[allow(clippy::too_many_arguments)]
    fn add_barrier(
        &self,
        image: &vk::Image,
        base_mip_level: u32,
        mip_levels: u32,
        old_layout: vk::ImageLayout,
        new_layout: vk::ImageLayout,
        src_access_mask: vk::AccessFlags,
        dst_access_mask: vk::AccessFlags,
        src_stage_mask: vk::PipelineStageFlags,
    ) {
        let image_barrier = vk::ImageMemoryBarrier::default()
            .old_layout(old_layout)
            .new_layout(new_layout)
            .image(*image)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .base_mip_level(base_mip_level)
                    .level_count(mip_levels)
                    .layer_count(1),
            )
            .src_access_mask(src_access_mask)
            .dst_access_mask(dst_access_mask);

        unsafe {
            self.device.cmd_pipeline_barrier(
                self.command_buffers[0],
                src_stage_mask,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[image_barrier],
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn blit(
        &self,
        src_image: &vk::Image,
        src_width: u32,
        src_height: u32,
        src_mip_level: u32,
        dst_image: &vk::Image,
        dst_width: u32,
        dst_height: u32,
        dst_mip_level: u32,
    ) {
        let blit_info = vk::ImageBlit::default()
            .src_offsets([
                vk::Offset3D { x: 0, y: 0, z: 0 },
                vk::Offset3D {
                    x: src_width as i32,
                    y: src_height as i32,
                    z: 1,
                },
            ])
            .src_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .mip_level(src_mip_level)
                    .layer_count(1),
            )
            .dst_offsets([
                vk::Offset3D { x: 0, y: 0, z: 0 },
                vk::Offset3D {
                    x: dst_width as i32,
                    y: dst_height as i32,
                    z: 1,
                },
            ])
            .dst_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .mip_level(dst_mip_level)
                    .layer_count(1),
            );

        unsafe {
            self.device.cmd_blit_image(
                self.command_buffers[0],
                *src_image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                *dst_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[blit_info],
                vk::Filter::LINEAR,
            );
        }
    }

    fn generate_mipmaps(&self, frame_image: &vk::Image, image: &vk::Image) -> (u32, u32, u32) {
        let (mut mip_width, mut mip_height, mip_levels) = self.image_resolution.unwrap();

        self.add_barrier(
            image,
            0,
            mip_levels,
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::AccessFlags::default(),
            vk::AccessFlags::TRANSFER_WRITE,
            vk::PipelineStageFlags::TOP_OF_PIPE,
        );

        self.blit(
            frame_image,
            mip_width,
            mip_height,
            0,
            image,
            mip_width,
            mip_height,
            0,
        );

        let target_mip_level = mip_levels - FINAL_MIP_LEVEL;
        for i in 1..=target_mip_level {
            self.add_barrier(
                image,
                i - 1,
                1,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::TRANSFER_READ,
                vk::PipelineStageFlags::TRANSFER,
            );

            let next_mip_width = if mip_width > 1 { mip_width / 2 } else { 1 };
            let next_mip_height = if mip_height > 1 { mip_height / 2 } else { 1 };

            self.blit(
                image,
                mip_width,
                mip_height,
                i - 1,
                image,
                next_mip_width,
                next_mip_height,
                i,
            );

            mip_width = next_mip_width;
            mip_height = next_mip_height;
        }

        (target_mip_level, mip_width, mip_height)
    }

    fn copy_mipmap(
        &self,
        image: &vk::Image,
        mip_level: u32,
        width: u32,
        height: u32,
    ) -> Result<()> {
        self.add_barrier(
            image,
            mip_level,
            1,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::TRANSFER_READ,
            vk::PipelineStageFlags::TRANSFER,
        );

        let buffer_image_copy = vk::BufferImageCopy::default()
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .mip_level(mip_level)
                    .layer_count(1),
            )
            .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
            .image_extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            });

        let buffer = self.buffer.ok_or(anyhow!("Unable to borrow buffer"))?;

        unsafe {
            self.device.cmd_copy_image_to_buffer(
                self.command_buffers[0],
                *image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                buffer,
                &[buffer_image_copy],
            );
        }

        Ok(())
    }

    fn begin_commands(&self) -> Result<()> {
        let command_buffer_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);

        unsafe {
            self.device
                .begin_command_buffer(self.command_buffers[0], &command_buffer_info)
                .map_err(anyhow::Error::msg)?;
        }

        Ok(())
    }

    fn submit_commands(&self) -> Result<()> {
        unsafe {
            // End the command buffer
            self.device
                .end_command_buffer(self.command_buffers[0])
                .map_err(anyhow::Error::msg)?;
        };

        let submit_info = vk::SubmitInfo::default().command_buffers(&self.command_buffers);

        unsafe {
            // Submit the command buffers to the queue
            self.device
                .queue_submit(self.queue, &[submit_info], self.fence)
                .map_err(anyhow::Error::msg)?;

            // Wait for the fences
            self.device
                .wait_for_fences(&[self.fence], true, FENCES_TIMEOUT_NS)
                .map_err(anyhow::Error::msg)?;

            // Reset fences
            self.device
                .reset_fences(&[self.fence])
                .map_err(anyhow::Error::msg)?;
        }

        Ok(())
    }
}

impl Drop for Vulkan {
    fn drop(&mut self) {
        unsafe {
            self.device
                .device_wait_idle()
                .expect("Unable to wait for device to become idle");

            if let Some(image) = self.image {
                self.device.destroy_image(image, None);
            }
            if let Some(image_memory) = self.image_memory {
                self.device.free_memory(image_memory, None);
            }
            if let Some(image) = self.exportable_frame_image {
                self.device.destroy_image(image, None);
            }
            if let Some(image_memory) = self.exportable_frame_image_memory {
                self.device.free_memory(image_memory, None);
            }

            self.device.destroy_fence(self.fence, None);
            if let Some(buffer) = self.buffer {
                self.device.destroy_buffer(buffer, None);
            }
            if let Some(buffer_memory) = self.buffer_memory {
                self.device.free_memory(buffer_memory, None);
            }
            self.device
                .free_command_buffers(self.command_pool, &self.command_buffers);
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

fn memory_type_index(memory_type_bits: u32, resource: &str) -> Result<u32> {
    if memory_type_bits == 0 {
        Err(anyhow!("No Vulkan memory type supports the {resource}"))
    } else {
        Ok(memory_type_bits.trailing_zeros())
    }
}

fn find_memory_type_index(
    memory_req: &vk::MemoryRequirements,
    memory_prop: &vk::PhysicalDeviceMemoryProperties,
    flags: vk::MemoryPropertyFlags,
) -> Option<u32> {
    memory_prop.memory_types[..memory_prop.memory_type_count as _]
        .iter()
        .enumerate()
        .find(|(index, memory_type)| {
            (1 << index) & memory_req.memory_type_bits != 0
                && memory_type.property_flags & flags == flags
        })
        .map(|(index, _)| index as _)
}

fn map_drm_format(format: u32) -> Result<vk::Format> {
    let drm = DrmFourcc::try_from(format)?;
    match drm {
        DrmFourcc::Rgbx4444 => Ok(vk::Format::R4G4B4A4_UNORM_PACK16),
        DrmFourcc::Bgrx4444 => Ok(vk::Format::B4G4R4A4_UNORM_PACK16),
        DrmFourcc::Rgb565 => Ok(vk::Format::R5G6B5_UNORM_PACK16),
        DrmFourcc::Bgr565 => Ok(vk::Format::B5G6R5_UNORM_PACK16),
        DrmFourcc::Xrgb1555 => Ok(vk::Format::A1R5G5B5_UNORM_PACK16),
        DrmFourcc::Rgbx5551 => Ok(vk::Format::R5G5B5A1_UNORM_PACK16),
        DrmFourcc::Bgrx5551 => Ok(vk::Format::B5G5R5A1_UNORM_PACK16),
        DrmFourcc::Xrgb2101010 => Ok(vk::Format::A2R10G10B10_UNORM_PACK32),
        DrmFourcc::Xbgr2101010 => Ok(vk::Format::A2B10G10R10_UNORM_PACK32),
        DrmFourcc::Xbgr16161616f => Ok(vk::Format::R16G16B16A16_SFLOAT),
        DrmFourcc::Xbgr8888 => Ok(vk::Format::R8G8B8A8_UNORM),
        DrmFourcc::Bgrx8888 => Ok(vk::Format::B8G8R8A8_UNORM),
        DrmFourcc::Xrgb8888 => Ok(vk::Format::B8G8R8A8_UNORM),
        _ => Err(anyhow!(
            "Unsupported DRM format: {format}. Please report on GitHub."
        )),
    }
}
