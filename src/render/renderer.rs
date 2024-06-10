use std::{collections::{hash_map::Entry, HashMap}, sync::Arc, time::{Duration, Instant}};

use img::ImageFormat;
use raw_window_handle::HandleError;
use tap::TapFallible;
use thiserror::Error;

use vulkano::{
    buffer::{
        AllocateBufferError, Buffer, BufferContents, BufferCreateInfo, BufferUsage, IndexBuffer, Subbuffer
    }, command_buffer::{
        allocator::{
            CommandBufferAllocator, StandardCommandBufferAllocator, StandardCommandBufferAllocatorCreateInfo
        }, ClearAttachment, ClearRect, CommandBufferBeginInfo, CommandBufferLevel, CommandBufferUsage, CopyBufferToImageInfo, RecordingCommandBuffer, RenderPassBeginInfo, SubpassBeginInfo, SubpassContents, SubpassEndInfo
    }, descriptor_set::{
        allocator::{
            DescriptorSetAllocator, StandardDescriptorSetAllocator, StandardDescriptorSetAllocatorCreateInfo
        }, layout::{
            DescriptorSetLayout, DescriptorSetLayoutBinding, DescriptorSetLayoutCreateFlags, DescriptorSetLayoutCreateInfo, DescriptorType
        }, pool::{
            DescriptorPool, DescriptorPoolCreateFlags, DescriptorPoolCreateInfo
        }, DescriptorSet, WriteDescriptorSet
    }, device::{
        physical::{
            PhysicalDevice, PhysicalDeviceType
        }, Device, DeviceCreateInfo, DeviceExtensions, DeviceFeatures, DeviceProperties, Queue, QueueCreateInfo, QueueFlags
    }, format::{
        ClearValue, Format
    }, image::{
        sampler::{Sampler, SamplerCreateInfo}, view::{ImageView, ImageViewCreateInfo}, Image, ImageCreateInfo, ImageLayout, ImageTiling, ImageType, ImageUsage, SampleCount
    }, instance::{
        Instance,
        InstanceCreateInfo,
    }, memory::allocator::{
        AllocationCreateInfo, MemoryAllocator, MemoryTypeFilter, StandardMemoryAllocator
    }, pipeline::{
        graphics::{
            color_blend::{
                ColorBlendAttachmentState, ColorBlendState
            }, depth_stencil::{
                DepthState, DepthStencilState
            }, input_assembly::{
                InputAssemblyState,
                PrimitiveTopology,
            }, multisample::MultisampleState, rasterization::{
                CullMode,
                RasterizationState,
            }, subpass::PipelineSubpassType, vertex_input::{
                VertexInputAttributeDescription,
                VertexInputBindingDescription,
                VertexInputRate,
                VertexInputState,
            }, viewport::{
                Scissor,
                Viewport,
                ViewportState,
            }, GraphicsPipelineCreateInfo
        }, layout::{
            PipelineLayoutCreateFlags,
            PipelineLayoutCreateInfo,
        },
        Pipeline,
        GraphicsPipeline,
        PipelineBindPoint,
        PipelineLayout,
        PipelineShaderStageCreateInfo,
    }, render_pass::{
        Framebuffer, RenderPass, Subpass
    }, shader::{
        ShaderModule,
        ShaderStages,
    }, swapchain::{
        self, CompositeAlpha, Surface, Swapchain, SwapchainCreateInfo, SwapchainPresentInfo
    }, sync::{
        GpuFuture, HostAccessError
    }, LoadingError, Validated, Version, VulkanError, VulkanLibrary
};
use winit::{window::{WindowId, Window}, dpi::PhysicalSize};

use crate::render::{task::RenderTask, shaders};

#[derive(Debug, Clone, Default)]
pub struct RendererPreferences {
    pub preferred_physical_device: Option<String>,
    pub preferred_physical_device_type: Option<PhysicalDeviceType>,
    pub application_name: Option<String>,
    pub application_version: Option<Version>,
}

impl RendererPreferences {
    fn split(self) -> (LibraryPreferences, DevicePreferences) {
        (LibraryPreferences {
            application_name: self.application_name,
            application_version: self.application_version,
        }, DevicePreferences {
            preferred_physical_device: self.preferred_physical_device,
            preferred_physical_device_type: self.preferred_physical_device_type,
        })
    }
}

#[derive(Debug, Default)]
pub struct LibraryPreferences {
    pub application_name: Option<String>,
    pub application_version: Option<Version>,
}

#[derive(Debug, Default)]
pub struct DevicePreferences {
    pub preferred_physical_device: Option<String>,
    pub preferred_physical_device_type: Option<PhysicalDeviceType>,
}

impl DevicePreferences {
    /// For convenience preferred device check.
    fn is_preferred_device(&self, properties: &DeviceProperties) -> bool {
        let Some(ref preferred) = self.preferred_physical_device_type else {
            return false;
        };
        *preferred == properties.device_type
    }

    /// For convenience preferred name check.
    fn is_preferred_name(&self, properties: &DeviceProperties) -> bool {
        let Some(ref preferred) = self.preferred_physical_device else {
            return false;
        };
        *preferred == properties.device_name
    }

    // This is a "generally better" heuristic. (Higher is better.)
    fn score_physical_device(&self, physical_device: &PhysicalDevice) -> usize {
        let props = physical_device.properties();

        let name_score = if self.is_preferred_name(&props) { 1 } else { 0 };

        let pdt_pref_score = if self.is_preferred_device(&props) { 1 } else { 0 };

        // We'll just use an array here -- a match doesn't really help.
        // Ordered from least preferred to most. Later entries have higher indices so they're more
        // preferred.
        const PDT_ORD_ARR: [PhysicalDeviceType; 5] = [
            PhysicalDeviceType::Other,
            // I don't actually know if CPU is preferred less than or over the virtual GPU.
            PhysicalDeviceType::Cpu,
            PhysicalDeviceType::VirtualGpu,
            PhysicalDeviceType::IntegratedGpu,
            PhysicalDeviceType::DiscreteGpu,
        ];
        let pdt_ord_score = PDT_ORD_ARR.iter().position(|&pdt| pdt == props.device_type).unwrap_or(0);

        // This is the score without knowledge of the device. If we have a target name, that
        // overrides any device "attributes".
        let primary_score = (name_score << 1) + pdt_pref_score;
        // We'll scale this up by a flat factor so that we can add a discriminant to prefer
        // discrete GPUs.
        primary_score * (PDT_ORD_ARR.len() + 1) + pdt_ord_score
    }
}

pub struct LoadedModelHandles {
    texture_image: Option<Arc<Image>>,
    texture_descriptor_set: Option<Arc<DescriptorSet>>,
    vertex_offset: i32,
    index_offset: u32,
}

#[derive(Debug, Error)]
pub enum RendererInitializationError {
    #[error("{0}")]
    Library(#[from] LoadingError),
    #[error("instance initialization: {0}")]
    Instance(Validated<VulkanError>),
    #[error("physical device enumeration: {0}")]
    Physical(VulkanError),
    #[error("no physical device -- no software fallback")]
    NoPhysicalDevice,
    #[error("no valid physical device -- no physical device with graphics queue family")]
    PhysicalDeviceMissingGraphicsCapabilities,
    #[error("failed to create queue: {0}")]
    QueueCreationFailed(Validated<VulkanError>),
    #[error("failed to create buffer: {0}")]
    BufferCreationFailed(Validated<AllocateBufferError>),
    #[error("no queue for device")]
    NoCommandQueue,
    #[error("failed to create commmand buffer: {0}")]
    CommandBufferCreationFailed(Validated<VulkanError>),
    #[error("failed to compile shader {0}: {1}")]
    ShaderLoadFail(&'static str, Validated<VulkanError>),
    #[error("surface extension enumeration: {0}")]
    SurfaceCheck(#[from] HandleError),
}

struct RenderTimer {
    start: Instant,
    inflight_load: Option<Instant>,
    loads: Vec<Duration>,
    inflight_command_record: Option<Instant>,
    command_record_duration: Option<Duration>,
    inflight_draw: Option<Instant>,
    draw_duration: Option<Duration>,
}

impl RenderTimer {
    fn begin() -> Self {
        Self {
            start: Instant::now(),
            inflight_load: None,
            loads: Vec::with_capacity(20),
            inflight_command_record: None,
            command_record_duration: None,
            inflight_draw: None,
            draw_duration: None,
        }
    }

    fn record_load_start(&mut self) {
        self.inflight_load = Some(Instant::now());
    }

    fn record_load_end(&mut self) {
        let Some(start) = self.inflight_load else {
            return;
        };
        self.loads.push(Instant::now() - start);
    }

    fn record_command_record_start(&mut self) {
        self.inflight_command_record = Some(Instant::now());
    }

    fn record_command_record_end(&mut self) {
        let Some(start) = self.inflight_command_record else {
            return;
        };
        self.command_record_duration = Some(Instant::now() - start);
    }

    fn record_draw_start(&mut self) {
        self.inflight_draw = Some(Instant::now());
    }

    fn record_draw_end(&mut self) {
        let Some(start) = self.inflight_draw else {
            return;
        };
        self.draw_duration = Some(Instant::now() - start);
    }

    fn record_end(self) {
        let over = Instant::now() - self.start;
        let a = 1000. / over.as_millis() as f64;
        trc::debug!("RENDER-PASS-TIMING fps={a} loads={:?} submit={:?} draw={:?} total={:?}", self.loads, self.command_record_duration, self.draw_duration, over);
    }
}

pub struct QueueBookmarks {
    family_idx: u32,
}

pub struct ShaderBlock {
    vertex: Arc<ShaderModule>,
    geometry: Arc<ShaderModule>,
    fragment: Arc<ShaderModule>,
}

pub struct RendererDescriptorLayouts {
    general: Arc<DescriptorSetLayout>,
    texture: Arc<DescriptorSetLayout>,
}

pub struct RendererBuilder {
    pub windowing: Arc<Window>,
    pub prefs: RendererPreferences,
}

/// Initialization helpers.
impl RendererBuilder {
    fn initialize_vulkan(windowing: Arc<Window>, prefs: LibraryPreferences) -> Result<Arc<Instance>, RendererInitializationError> {
        let vulkan_library = VulkanLibrary::new()
            .tap_err(|e| trc::error!("TOTALITY-RENDERER-INIT-FAILED primary_source=vulkan_lib error=missing_error {e}"))?;
        let required_extensions = Surface::required_extensions(&windowing)
            .tap_err(|e| trc::error!("TOTALITY-RENDERER-INIT-FAILED primary_source=surface_check {e}"))?;
        let vulkan = Instance::new(
            vulkan_library,
            InstanceCreateInfo {
                enabled_extensions: required_extensions,
                engine_name: Some("totality".to_owned()),
                engine_version: Version::default(),
                application_name: prefs.application_name,
                application_version: prefs.application_version.unwrap_or_default(),
                // enabled_layers: vec!["VK_LAYER_KHRONOS_validation".to_owned(), "VK_LAYER_LUNARG_api_dump".to_owned()],
                ..Default::default()
            }
        )
            .tap_err(|e| trc::error!("TOTALITY-RENDERER-INIT-FAILED source=driver error=instance {e}"))
            .map_err(RendererInitializationError::Instance)?;

        Ok(vulkan)
    }

    /// Find best (most performant or preferred) valid device.
    fn select_physical_device(vulkan: &Arc<Instance>, prefs: DevicePreferences) -> Result<Arc<PhysicalDevice>, RendererInitializationError> {
        let iter = vulkan.enumerate_physical_devices()
            .tap_err(|e| trc::error!("TOTALITY-RENDERER-INIT-FAILED source=physical_devices error=enumeration_failure {e}"))
            .map_err(RendererInitializationError::Physical)?;
        let minimum_device_extensions = DeviceExtensions {
            khr_swapchain: true,
            ..DeviceExtensions::empty()
        };
        let mut devices = iter.filter(|pd| pd.supported_extensions().contains(&minimum_device_extensions)).collect::<Vec<_>>();
        // Sort from least score to most score since that puts the highest value device at the end
        // to make it easier to remove.
        devices.sort_by_cached_key(|pd| prefs.score_physical_device(pd));
        let Some(device) = devices.pop() else {
            trc::error!("TOTALITY-RENDERER-INIT-FAILED source=physical_devices error=no_device");
            return Err(RendererInitializationError::NoPhysicalDevice);
        };

        Ok(device)
    }

    /// Select a queue family. We need something that can *actually* render something.
    fn select_queue_family(physical: &Arc<PhysicalDevice>) -> Result<QueueBookmarks, RendererInitializationError> {
        // Assume there's at least one physical device and that the first device is valid.
        let queue_props = physical.queue_family_properties();
        // Just pick the first valid one for now, we'll come back.
        // TODO Be smarter about selecting a queue family, or be dynamic about it.
        // TODO disqualify queues that don't support a surface
        let Some(idx) = queue_props.iter().position(|family| family.queue_flags.contains(QueueFlags::GRAPHICS)) else {
            return Err(RendererInitializationError::PhysicalDeviceMissingGraphicsCapabilities);
        };

        let Ok(idx_u32) = idx.try_into() else {
            panic!("number of queues is small enough to fit into a u32")
        };

        Ok(QueueBookmarks { family_idx: idx_u32 })
    }

    fn create_logical_device(physical: Arc<PhysicalDevice>, queue_bookmarks: &QueueBookmarks) -> Result<(Arc<Device>, Arc<Queue>), RendererInitializationError> {
        let required_extensions = DeviceExtensions {
            khr_swapchain: true,
            ..DeviceExtensions::empty()
        };
        let device_result = Device::new(
            physical,
            DeviceCreateInfo {
                enabled_features: DeviceFeatures {
                    geometry_shader: true,
                    ..DeviceFeatures::empty()
                },
                queue_create_infos: vec![QueueCreateInfo {
                    queue_family_index: queue_bookmarks.family_idx,
                    ..Default::default()
                }],
                enabled_extensions: required_extensions,
                ..Default::default()
            },
        );
        let (device, mut queues_iter) = match device_result {
            Ok(device) => device,
            Err(e) => {
                trc::error!("TOTALITY-RENDERER-INIT-FAILED source=device_queue error=failed_creation {e}");
                return Err(RendererInitializationError::QueueCreationFailed(e));
            },
        };
        let Some(queue) = queues_iter.next() else {
            return Err(RendererInitializationError::NoCommandQueue);
        };

        Ok((device, queue))
    }

    fn allocate_vertex_buffer<T: BufferContents + ?Sized>(device_memory_alloc: Arc<dyn MemoryAllocator>) -> Result<Subbuffer<T>, RendererInitializationError> {
        let result = Buffer::new_unsized(
            device_memory_alloc,
            BufferCreateInfo {
                usage: BufferUsage::VERTEX_BUFFER | BufferUsage::TRANSFER_DST,
                // flags: BufferCreateFlags::default(),
                // sharing: (),
                // size: (),
                // usage: (),
                // external_memory_handle_types: (),
                ..Default::default()
            },
            AllocationCreateInfo {
                // memory_type_bits: (),
                // allocate_preference: (),
                memory_type_filter: MemoryTypeFilter::PREFER_DEVICE | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                ..Default::default()
            },
            // This should be fine. A vertex is 3*32 bits -- 12 bytes.
            // Let's just assume each scene will have < 5_000_000 vertices. So let's just allocate
            // 60 MB. Then add an extra vector.
            60 * 1024 * 1024
        );
        let buffer = match result {
            Ok(b) => b,
            Err(e) => {
                trc::error!("TOTALITY-RENDERER-INIT-FAILED source=vertex_buffer error=failed_creation {e}");
                return Err(RendererInitializationError::BufferCreationFailed(e));
            },
        };

        Ok(buffer)
    }

    fn allocate_face_buffer<T: BufferContents + ?Sized>(device_memory_alloc: Arc<dyn MemoryAllocator>) -> Result<Subbuffer<T>, RendererInitializationError> {
        let result = Buffer::new_unsized(
            device_memory_alloc,
            BufferCreateInfo {
                usage: BufferUsage::INDEX_BUFFER | BufferUsage::TRANSFER_DST,
                // flags: BufferCreateFlags::default(),
                // sharing: (),
                // size: (),
                // usage: (),
                // external_memory_handle_types: (),
                ..Default::default()
            },
            AllocationCreateInfo {
                // memory_type_bits: (),
                // allocate_preference: (),
                memory_type_filter: MemoryTypeFilter::PREFER_DEVICE | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                ..Default::default()
            },
            // This should be fine. A face is 3*32 bits -- 12 bytes.
            // Let's just assume each scene will have < 5_000_000 faces. So let's just allocate
            // 60 MB. Then add an extra vector.
            60 * 1024 * 1024
        );
        let buffer = match result {
            Ok(b) => b,
            Err(e) => {
                trc::error!("TOTALITY-RENDERER-INIT-FAILED source=face_buffer error=failed_creation {e}");
                return Err(RendererInitializationError::BufferCreationFailed(e))?;
            },
        };

        Ok(buffer)
    }

    fn allocate_count_buffer<T: BufferContents + ?Sized>(device_memory_alloc: Arc<dyn MemoryAllocator>) -> Result<Subbuffer<T>, RendererInitializationError> {
        let result = Buffer::new_unsized(
            device_memory_alloc,
            BufferCreateInfo {
                usage: BufferUsage::UNIFORM_BUFFER | BufferUsage::TRANSFER_DST,
                // flags: (),
                // sharing: (),
                // usage: (),
                // external_memory_handle_types: (),
                ..Default::default()
            },
            AllocationCreateInfo {
                // memory_type_bits: (),
                // allocate_preference: (),
                memory_type_filter: MemoryTypeFilter::PREFER_DEVICE | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                ..Default::default()
            },
            // This allows for approximately 1024 instances.
            // Assuming a 4x4 32bit ...
            16,
        );
        let buffer = match result {
            Ok(b) => b,
            Err(e) => {
                trc::error!("TOTALITY-RENDERER-INIT-FAILED source=count_buffer error=failed_creation {e}");
                return Err(RendererInitializationError::BufferCreationFailed(e));
            },
        };
        Ok(buffer)
    }

    fn allocate_mesh_uniform_buffer<T: BufferContents + ?Sized>(device_memory_alloc: Arc<dyn MemoryAllocator>) -> Result<Subbuffer<T>, RendererInitializationError> {
        let result = Buffer::new_unsized(
            device_memory_alloc,
            BufferCreateInfo {
                usage: BufferUsage::UNIFORM_BUFFER | BufferUsage::TRANSFER_DST,
                // flags: (),
                // sharing: (),
                // usage: (),
                // external_memory_handle_types: (),
                ..Default::default()
            },
            AllocationCreateInfo {
                // memory_type_bits: (),
                // allocate_preference: (),
                memory_type_filter: MemoryTypeFilter::PREFER_DEVICE | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                ..Default::default()
            },
            // This allows for approximately 1024 instances.
            // Assuming a 4x4 32bit ...
            64 * 1024,
        );
        let buffer = match result {
            Ok(b) => b,
            Err(e) => {
                trc::error!("TOTALITY-RENDERER-INIT-FAILED source=matrix_buffer error=failed_creation {e}");
                return Err(RendererInitializationError::BufferCreationFailed(e));
            },
        };
        Ok(buffer)
    }

    fn allocate_light_buffer<T: BufferContents + ?Sized>(device_memory_alloc: Arc<dyn MemoryAllocator>) -> Result<Subbuffer<T>, RendererInitializationError> {
        let result = Buffer::new_unsized(
            device_memory_alloc,
            BufferCreateInfo {
                usage: BufferUsage::UNIFORM_BUFFER | BufferUsage::TRANSFER_DST,
                // flags: (),
                // sharing: (),
                // usage: (),
                // external_memory_handle_types: (),
                ..Default::default()
            },
            AllocationCreateInfo {
                // memory_type_bits: (),
                // allocate_preference: (),
                memory_type_filter: MemoryTypeFilter::PREFER_DEVICE | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                ..Default::default()
            },
            // This allows for approximately 500 instances.
            // Assuming a 4x4 32bit float matrix for orientation, positioning, and scaling -- 16 *
            // 32 / 8 = 2^6 bytes per array. 2^6 * 500 = 32000. Let's just use ~50KB.
            96 * 1024,
        );
        let buffer = match result {
            Ok(b) => b,
            Err(e) => {
                trc::error!("TOTALITY-RENDERER-INIT-FAILED source=light_buffer error=failed_creation {e}");
                return Err(RendererInitializationError::BufferCreationFailed(e));
            },
        };
        Ok(buffer)
    }

    fn allocate_material_buffer<T: BufferContents + ?Sized>(device_memory_alloc: Arc<dyn MemoryAllocator>) -> Result<Subbuffer<T>, RendererInitializationError> {
        let result = Buffer::new_unsized(
            device_memory_alloc,
            BufferCreateInfo {
                usage: BufferUsage::UNIFORM_BUFFER | BufferUsage::TRANSFER_DST,
                // flags: (),
                // sharing: (),
                // usage: (),
                // external_memory_handle_types: (),
                ..Default::default()
            },
            AllocationCreateInfo {
                // memory_type_bits: (),
                // allocate_preference: (),
                memory_type_filter: MemoryTypeFilter::PREFER_DEVICE | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                ..Default::default()
            },
            // This allows for approximately 500 instances.
            // Assuming a 4x4 32bit float matrix for orientation, positioning, and scaling -- 16 *
            // 32 / 8 = 2^6 bytes per array. 2^6 * 500 = 32000. Let's just use ~50KB.
            16 * 1024,
        );
        let buffer = match result {
            Ok(b) => b,
            Err(e) => {
                trc::error!("TOTALITY-RENDERER-INIT-FAILED source=material_buffer error=failed_creation {e}");
                return Err(RendererInitializationError::BufferCreationFailed(e))
            },
        };
        Ok(buffer)
    }

    fn allocate_texture_staging_buffer<T: BufferContents + ?Sized>(device_memory_alloc: Arc<dyn MemoryAllocator>) -> Result<Subbuffer<T>, RendererInitializationError> {
        let result = Buffer::new_unsized(
            device_memory_alloc,
            BufferCreateInfo {
                usage: BufferUsage::UNIFORM_TEXEL_BUFFER | BufferUsage::TRANSFER_DST | BufferUsage::TRANSFER_SRC,
                // flags: (),
                // sharing: (),
                // size: (),
                // usage: (),
                // external_memory_handle_types: (),
                ..Default::default()
            },
            AllocationCreateInfo {
                // memory_type_bits: (),
                // allocate_preference: (),
                memory_type_filter: MemoryTypeFilter::PREFER_DEVICE | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                ..Default::default()
            },
            50 * 1024 * 1024
        );
        let buffer = match result {
            Ok(b) => b,
            Err(e) => {
                trc::error!("TOTALITY-RENDERER-INIT-FAILED source=staging_texture_buffer error=failed_creation {e}");
                return Err(RendererInitializationError::BufferCreationFailed(e))
            },
        };
        Ok(buffer)
    }

    fn allocate_wireframe_buffer<T: BufferContents + ?Sized>(device_memory_alloc: Arc<dyn MemoryAllocator>) -> Result<Subbuffer<T>, RendererInitializationError> {
        let results = Buffer::new_unsized(
            device_memory_alloc,
            BufferCreateInfo {
                usage: BufferUsage::UNIFORM_BUFFER | BufferUsage::TRANSFER_DST,
                // flags: (),
                // sharing: (),
                // size: (),
                // external_memory_handle_types: (),
                ..Default::default()
            },
            AllocationCreateInfo {
                // memory_type_bits: (),
                // allocate_preference: (),
                memory_type_filter: MemoryTypeFilter::PREFER_DEVICE | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                ..Default::default()
            },
            16
        );
        let buffer = match results {
            Ok(b) => b,
            Err(e) => {
                trc::error!("TOTALITY-RENDERER-INIT-FAILED source=wireframe_buffer error=failed_creation {e}");
                return Err(RendererInitializationError::BufferCreationFailed(e));
            },
        };
        Ok(buffer)
    }

    fn allocate_cam_matrix_buffer<T: BufferContents + ?Sized>(device_memory_alloc: Arc<dyn MemoryAllocator>) -> Result<Subbuffer<T>, RendererInitializationError> {
        let results = Buffer::new_unsized(
            device_memory_alloc,
            BufferCreateInfo {
                usage: BufferUsage::UNIFORM_BUFFER | BufferUsage::TRANSFER_DST,
                // flags: (),
                // sharing: (),
                // size: (),
                // external_memory_handle_types: (),
                ..Default::default()
            },
            AllocationCreateInfo {
                // memory_type_bits: (),
                // allocate_preference: (),
                memory_type_filter: MemoryTypeFilter::PREFER_DEVICE | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                ..Default::default()
            },
            64
        );
        let buffer = match results {
            Ok(b) => b,
            Err(e) => {
                trc::error!("TOTALITY-RENDERER-INIT-FAILED source=cam_matrix_buffer error=failed_creation {e}");
                return Err(RendererInitializationError::BufferCreationFailed(e));
            },
        };
        Ok(buffer)
    }

    fn allocate_configuration_buffer<T: BufferContents + ?Sized>(device_memory_alloc: Arc<dyn MemoryAllocator>) -> Result<Subbuffer<T>, RendererInitializationError> {
        let result = Buffer::new_unsized(
            device_memory_alloc,
            BufferCreateInfo {
                usage: BufferUsage::UNIFORM_BUFFER | BufferUsage::TRANSFER_DST,
                // flags: (),
                // sharing: (),
                // size: (),
                // external_memory_handle_types: (),
                ..Default::default()
            },
            AllocationCreateInfo {
                // memory_type_bits: (),
                // allocate_preference: (),
                memory_type_filter: MemoryTypeFilter::PREFER_DEVICE | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                ..Default::default()
            },
            50 * 1024 * 1024
        );
        let buffer = match result {
            Ok(b) => b,
            Err(e) => {
                trc::error!("TOTALITY-RENDERER-INIT-FAILED source=configuration_buffer error=failed_creation {e}");
                return Err(RendererInitializationError::BufferCreationFailed(e));
            },
        };
        Ok(buffer)
    }

    fn create_shaders(device: &Arc<Device>) -> Result<ShaderBlock, RendererInitializationError> {
        let vertex = match shaders::basic_vert::load(device.clone()) {
            Ok(s) => s,
            Err(e) => {
                trc::error!("TOTALITY-RENDERER-INIT-FAILED source=shader shader=basic_vert {e}");
                return Err(RendererInitializationError::ShaderLoadFail("basic_vert", e));
            },
        };
        let geometry = match shaders::basic_geom::load(device.clone()) {
            Ok(s) => s,
            Err(e) => {
                trc::error!("TOTALITY-RENDERER-INIT-FAILED source=shader shader=basic_geom {e}");
                return Err(RendererInitializationError::ShaderLoadFail("basic_geom", e));
            },
        };
        let fragment = match shaders::basic_frag::load(device.clone()) {
            Ok(s) => s,
            Err(e) => {
                trc::error!("TOTALITY-RENDERER-INIT-FAILED source=shader shader=basic_frag {e}");
                return Err(RendererInitializationError::ShaderLoadFail("basic_frag", e));
            },
        };

        Ok(ShaderBlock {
            vertex,
            geometry,
            fragment,
        })
    }

    fn create_descriptor_set_layouts(device: &Arc<Device>) -> RendererDescriptorLayouts {
        let general = DescriptorSetLayout::new(Arc::clone(device), DescriptorSetLayoutCreateInfo {
            flags: DescriptorSetLayoutCreateFlags::empty(),
            bindings: [
                (0, DescriptorSetLayoutBinding {
                    descriptor_count: 1,
                    stages: ShaderStages::VERTEX | ShaderStages::FRAGMENT,
                    ..DescriptorSetLayoutBinding::descriptor_type(DescriptorType::UniformBuffer)
                }),
                (1, DescriptorSetLayoutBinding {
                    descriptor_count: 1,
                    stages: ShaderStages::VERTEX | ShaderStages::FRAGMENT,
                    ..DescriptorSetLayoutBinding::descriptor_type(DescriptorType::UniformBuffer)
                }),
                (2, DescriptorSetLayoutBinding {
                    descriptor_count: 1,
                    stages: ShaderStages::VERTEX | ShaderStages::FRAGMENT,
                    ..DescriptorSetLayoutBinding::descriptor_type(DescriptorType::UniformBuffer)
                }),
                (3, DescriptorSetLayoutBinding {
                    descriptor_count: 1,
                    stages: ShaderStages::VERTEX | ShaderStages::FRAGMENT,
                    ..DescriptorSetLayoutBinding::descriptor_type(DescriptorType::UniformBuffer)
                }),
                (4, DescriptorSetLayoutBinding {
                    descriptor_count: 1,
                    stages: ShaderStages::VERTEX | ShaderStages::FRAGMENT,
                    ..DescriptorSetLayoutBinding::descriptor_type(DescriptorType::UniformBuffer)
                }),
                (5, DescriptorSetLayoutBinding {
                    descriptor_count: 1,
                    stages: ShaderStages::VERTEX | ShaderStages::FRAGMENT,
                    ..DescriptorSetLayoutBinding::descriptor_type(DescriptorType::UniformBuffer)
                }),
            ].into_iter().collect(),
            ..DescriptorSetLayoutCreateInfo::default()
        }).unwrap();

        let texture = DescriptorSetLayout::new(Arc::clone(&device), DescriptorSetLayoutCreateInfo {
            flags: DescriptorSetLayoutCreateFlags::empty(),
            bindings: [
                (0, DescriptorSetLayoutBinding {
                    descriptor_count: 1,
                    stages: ShaderStages::FRAGMENT,
                    ..DescriptorSetLayoutBinding::descriptor_type(DescriptorType::SampledImage)
                }),
                (1, DescriptorSetLayoutBinding {
                    descriptor_count: 1,
                    stages: ShaderStages::FRAGMENT,
                    ..DescriptorSetLayoutBinding::descriptor_type(DescriptorType::Sampler)
                }),
            ].into_iter().collect(),
            ..DescriptorSetLayoutCreateInfo::default()
        }).unwrap();

        return RendererDescriptorLayouts {
            general,
            texture,
        };
    }

    fn create_general_render_pipeline_layout(device: &Arc<Device>, general_descriptor_set_layout: Arc<DescriptorSetLayout>, texture_descriptor_set_layout: Arc<DescriptorSetLayout>) -> Arc<PipelineLayout> {
        PipelineLayout::new(Arc::clone(device), PipelineLayoutCreateInfo {
            flags: PipelineLayoutCreateFlags::default(),
            set_layouts: vec![
                general_descriptor_set_layout,
                texture_descriptor_set_layout,
            ],
            push_constant_ranges: vec![],
            ..PipelineLayoutCreateInfo::default()
        }).unwrap()
    }

    fn allocate_descriptor_pool(device: &Arc<Device>) -> DescriptorPool {
        DescriptorPool::new(Arc::clone(&device), DescriptorPoolCreateInfo {
            max_sets: 11, // 1 universal descriptor set, 10 textures allowed.
            pool_sizes: [
                (DescriptorType::UniformBuffer, 3),
                (DescriptorType::SampledImage, 10),
                (DescriptorType::Sampler, 10),
            ].into_iter().collect(),
            flags: DescriptorPoolCreateFlags::empty(),
            ..Default::default()
        }).unwrap()
    }

    fn create_descriptor_set_allocator(device: &Arc<Device>, layouts: &RendererDescriptorLayouts) -> Arc<StandardDescriptorSetAllocator> {
        Arc::new(StandardDescriptorSetAllocator::new(
            Arc::clone(&device),
            StandardDescriptorSetAllocatorCreateInfo {
                set_count: layouts.general.bindings().len() + layouts.texture.bindings().len() * 10,
                update_after_bind: false,
                ..Default::default()
            }
        ))
    }
}

impl RendererBuilder {
    pub fn init(self) -> Result<Renderer, RendererInitializationError> {
        let (vulkan_prefs, device_prefs) = self.prefs.split();

        let vulkan = Self::initialize_vulkan(self.windowing, vulkan_prefs)?;
        let physical = Self::select_physical_device(&vulkan, device_prefs)?;
        let queue_bookmarks = Self::select_queue_family(&physical)?;
        let (device, selected_device_queue) = Self::create_logical_device(physical.clone(), &queue_bookmarks)?;
        // TODO Reevaluate if we can adjust the memory allocator.
        let device_memory_alloc = Arc::new(StandardMemoryAllocator::new_default(Arc::clone(&device)));

        // Let's just allocate giant chunks for:
        //   - vertices
        //   - triangles
        //   - number of values in each
        //   - model instances
        //   - lights
        //   - materials
        //   - texture staging
        //   - configuration
        //   - wireframe rendering
        //   - camera matrix
        let vertex_buffer = Self::allocate_vertex_buffer(device_memory_alloc.clone())?;
        let face_buffer = Self::allocate_face_buffer(device_memory_alloc.clone())?;
        let uniform_counts_buffer = Self::allocate_count_buffer(device_memory_alloc.clone())?;
        let uniform_per_mesh_buffer = Self::allocate_mesh_uniform_buffer(device_memory_alloc.clone())?;
        let uniform_light_buffer = Self::allocate_light_buffer(device_memory_alloc.clone())?;
        let uniform_material_buffer = Self::allocate_material_buffer(device_memory_alloc.clone())?;
        let staging_texture_buffer = Self::allocate_texture_staging_buffer(device_memory_alloc.clone())?;
        let uniform_wireframe_buffer = Self::allocate_wireframe_buffer(device_memory_alloc.clone())?;
        let uniform_cam_matrix_buffer = Self::allocate_cam_matrix_buffer(device_memory_alloc.clone())?;
        let configuration_buffer = Self::allocate_configuration_buffer(device_memory_alloc.clone())?;

        let command_buffer_alloc = Arc::new(StandardCommandBufferAllocator::new(Arc::clone(&device), StandardCommandBufferAllocatorCreateInfo::default()));

        let shaders = Self::create_shaders(&device)?;

        let descriptor_set_layouts = Self::create_descriptor_set_layouts(&device);

        let pipeline_layout = Self::create_general_render_pipeline_layout(&device, Arc::clone(&descriptor_set_layouts.general), Arc::clone(&descriptor_set_layouts.texture));

        let descriptor_pool = Self::allocate_descriptor_pool(&device);
        let descriptor_set_allocator: Arc<dyn DescriptorSetAllocator> = Self::create_descriptor_set_allocator(&device, &descriptor_set_layouts,);

        let descriptor_set = DescriptorSet::new(
            Arc::clone(&descriptor_set_allocator),
            Arc::clone(&descriptor_set_layouts.general),
            [
                WriteDescriptorSet::buffer(0, uniform_counts_buffer.clone()),
                WriteDescriptorSet::buffer(1, uniform_per_mesh_buffer.clone()),
                WriteDescriptorSet::buffer(2, uniform_light_buffer.clone()),
                WriteDescriptorSet::buffer(3, uniform_material_buffer.clone()),
                WriteDescriptorSet::buffer(4, uniform_wireframe_buffer.clone()),
                WriteDescriptorSet::buffer(5, uniform_cam_matrix_buffer.clone()),
            ],
            [],
        ).unwrap();
        let empty_texture_image = Image::new(
            Arc::clone(&device_memory_alloc) as _,
            ImageCreateInfo {
                image_type: ImageType::Dim2d,
                format: Format::R32G32B32A32_SFLOAT,
                extent: [1, 1, 1],
                usage: ImageUsage::SAMPLED,
                ..Default::default()
            },
            AllocationCreateInfo {
                ..Default::default()
            },
        ).unwrap();
        let empty_texture_image_view = ImageView::new_default(empty_texture_image).unwrap();
        let empty_texture_descriptor_set = DescriptorSet::new(
            Arc::clone(&descriptor_set_allocator),
            Arc::clone(&descriptor_set_layouts.texture),
            [
                WriteDescriptorSet::image_view(0, empty_texture_image_view),
                WriteDescriptorSet::sampler(1, Sampler::new(Arc::clone(&device), SamplerCreateInfo::simple_repeat_linear_no_mipmap()).unwrap()),
            ],
            [],
        ).unwrap();
        let texture_sampler = Sampler::new(
            Arc::clone(&device),
            SamplerCreateInfo {
                ..SamplerCreateInfo::simple_repeat_linear_no_mipmap()
            },
        ).unwrap();

        Ok(Renderer {
            vulkan,

            shaders,

            physical_device: physical,
            device,
            selected_device_queue,

            device_memory_alloc,

            vertex_buffer,
            face_buffer,
            uniform_counts_buffer,
            uniform_per_mesh_buffer,
            uniform_light_buffer,
            uniform_material_buffer,
            uniform_wireframe_buffer,
            uniform_cam_matrix_buffer,
            staging_texture_buffer,
            configuration_buffer,

            command_buffer_alloc,

            pipeline_layout,

            descriptor_set_layouts,
            descriptor_pool,
            descriptor_set_allocator,
            descriptor_set,
            empty_texture_descriptor_set,
            texture_sampler,

            // 1 since that's the typical usecase. 0 would be unused.
            windowed_swapchain: HashMap::with_capacity(1),

            loaded_models: HashMap::new(),
            index_free_byte_start: 0,
            index_free_start: 0,
            vertex_free_byte_start: 0,
            vertex_free_start: 0,

            pipeline_cache: None,
        })
    }
}

pub struct Renderer {
    vulkan: Arc<Instance>,

    shaders: ShaderBlock,

    physical_device: Arc<PhysicalDevice>,
    device: Arc<Device>,
    selected_device_queue: Arc<Queue>,

    device_memory_alloc: Arc<StandardMemoryAllocator>,

    vertex_buffer: Subbuffer<[u8]>,
    face_buffer: Subbuffer<[u8]>,
    uniform_counts_buffer: Subbuffer<[u8]>,
    uniform_per_mesh_buffer: Subbuffer<[u8]>,
    uniform_light_buffer: Subbuffer<[u8]>,
    uniform_material_buffer: Subbuffer<[u8]>,
    uniform_wireframe_buffer: Subbuffer<[u8]>,
    uniform_cam_matrix_buffer: Subbuffer<[u8]>,
    configuration_buffer: Subbuffer<[u8]>,
    staging_texture_buffer: Subbuffer<[u8]>,

    command_buffer_alloc: Arc<dyn CommandBufferAllocator>,

    pipeline_layout: Arc<PipelineLayout>,

    descriptor_set_layouts: RendererDescriptorLayouts,
    descriptor_pool: DescriptorPool,
    descriptor_set_allocator: Arc<dyn DescriptorSetAllocator>,
    descriptor_set: Arc<DescriptorSet>,
    empty_texture_descriptor_set: Arc<DescriptorSet>,
    texture_sampler: Arc<Sampler>,

    // TODO: better map for window ids?
    windowed_swapchain: HashMap<WindowId, RendererWindowSwapchain>,

    // Mesh id to (vertex, index) buffer offsets.
    loaded_models: HashMap<u64, LoadedModelHandles>,
    vertex_free_byte_start: u64,
    index_free_byte_start: u64,
    vertex_free_start: i32,
    index_free_start: u32,

    pipeline_cache: Option<([u32; 3], u32, WindowId, Arc<GraphicsPipeline>)>,
}


impl Renderer {
    fn copy_sized_slice_to_buffer<U: ?Sized, T: Sized + Copy + std::fmt::Debug + bytemuck::Pod>(buffer: &Subbuffer<U>, to_copy: &[T]) -> Result<(), HostAccessError> {
        let mapped_buffer = unsafe {
            buffer.mapped_slice()?.as_mut()
        };

        let cast_slice = bytemuck::cast_slice(to_copy);
        let num_bytes_to_copy = cast_slice.len();
        mapped_buffer[..num_bytes_to_copy].copy_from_slice(cast_slice);

        Ok(())
    }

    pub fn invalidate_model_memory(&mut self) {
        self.loaded_models.clear();
    }

    pub fn render_to(&mut self, window: Arc<Window>, task: RenderTask<'_>) -> Result<(), Validated<VulkanError>> {
        struct RenderTimer {
            start: Instant,
            inflight_load: Option<Instant>,
            loads: Vec<Duration>,
            swapchain_start: Option<Instant>,
            swapchain_duration: Option<Duration>,
            framebuffer_acquisition_start: Option<Instant>,
            framebuffer_acquisition_duration: Option<Duration>,
            pipeline_construction_start: Option<Instant>,
            pipeline_construction_duration: Option<Duration>,
            inflight_command_record: Option<Instant>,
            command_record_duration: Option<Duration>,
            inflight_draw: Option<Instant>,
            draw_duration: Option<Duration>,
        }

        impl RenderTimer {
            fn begin() -> Self {
                Self {
                    start: Instant::now(),
                    inflight_load: None,
                    loads: Vec::with_capacity(20),
                    swapchain_start: None,
                    swapchain_duration: None,
                    framebuffer_acquisition_start: None,
                    framebuffer_acquisition_duration: None,
                    pipeline_construction_start: None,
                    pipeline_construction_duration: None,
                    inflight_command_record: None,
                    command_record_duration: None,
                    inflight_draw: None,
                    draw_duration: None,
                }
            }

            fn record_load_start(&mut self) {
                self.inflight_load = Some(Instant::now());
            }

            fn record_load_end(&mut self) {
                let Some(start) = self.inflight_load else {
                    return;
                };
                self.loads.push(Instant::now() - start);
            }

            fn record_command_record_start(&mut self) {
                self.inflight_command_record = Some(Instant::now());
            }

            fn record_command_record_end(&mut self) {
                let Some(start) = self.inflight_command_record else {
                    return;
                };
                self.command_record_duration = Some(Instant::now() - start);
            }

            fn record_draw_start(&mut self) {
                self.inflight_draw = Some(Instant::now());
            }

            fn record_draw_end(&mut self) {
                let Some(start) = self.inflight_draw else {
                    return;
                };
                self.draw_duration = Some(Instant::now() - start);
            }

            fn record_swapchain_start(&mut self) {
                self.swapchain_start = Some(Instant::now());
            }

            fn record_swapchain_end(&mut self) {
                let Some(start) = self.swapchain_start else {
                    return;
                };
                self.swapchain_duration = Some(Instant::now() - start);
            }

            fn record_framebuffer_acquisition_start(&mut self) {
                self.framebuffer_acquisition_start = Some(Instant::now());
            }

            fn record_framebuffer_acquisition_end(&mut self) {
                let Some(start) = self.framebuffer_acquisition_start else {
                    return;
                };
                self.framebuffer_acquisition_duration = Some(Instant::now() - start);
            }

            fn record_pipeline_construction_start(&mut self) {
                self.pipeline_construction_start = Some(Instant::now());
            }

            fn record_pipeline_construction_end(&mut self) {
                let Some(start) = self.pipeline_construction_start else {
                    return;
                };
                self.pipeline_construction_duration = Some(Instant::now() - start);
            }

            fn record_end(self) {
                let over = Instant::now() - self.start;
                let a = 1000. / over.as_millis() as f64;
                trc::debug!(
                    "RENDER-PASS-TIMING \
                        fps={a} \
                        swapchain_duration={:?} \
                        loads={:?} \
                        framebuffer_acq={:?} \
                        pipeline_submission={:?} \
                        submit={:?} \
                        draw={:?} \
                        total={:?}\
                    ",
                    self.swapchain_duration,
                    self.loads,
                    self.framebuffer_acquisition_duration,
                    self.pipeline_construction_duration,
                    self.command_record_duration,
                    self.draw_duration,
                    over,
                );
            }
        }

        let mut perf = RenderTimer::begin();


        perf.record_swapchain_start();
        let mut e = self.windowed_swapchain.entry(window.id());
        let window_swapchain = match e {
            Entry::Vacant(v) => {
                self.pipeline_cache = None;
                v.insert(RendererWindowSwapchain::generate_swapchain(
                        &self.vulkan,
                        &window,
                        &self.physical_device,
                        &self.device,
                        &self.device_memory_alloc
                ).unwrap())
            },
            Entry::Occupied(ref mut o) => {
                let swapchain_information = o.get_mut();
                if swapchain_information.is_stale_for_window(&window) {
                    trc::trace!("RENDER-PASS-SWAPCHAIN-REGEN");
                    self.pipeline_cache = None;
                    swapchain_information.regenerated_swapchain(&window, &self.physical_device, &self.device, &self.device_memory_alloc).unwrap();
                }
                swapchain_information
            },
        };
        perf.record_swapchain_end();

        trc::debug!("RENDER-PASS-INIT");

        {
            for draw in task.draws.iter() {
                let mesh_id = draw.mesh.mesh_id;
                if self.loaded_models.contains_key(&mesh_id) {
                    trc::trace!("RENDER-COPY mesh={mesh_id} already_loaded=true");
                    continue;
                }

                perf.record_load_start();

                Self::copy_sized_slice_to_buffer(&self.vertex_buffer.clone().slice(self.vertex_free_byte_start..), draw.mesh.vec_vv.as_slice()).unwrap();
                Self::copy_sized_slice_to_buffer(&self.face_buffer.clone().slice(self.index_free_byte_start..), draw.mesh.ff.as_slice()).unwrap();
                // We'll assume the image's format is RGB.
                let (texture_image, texture_descriptor_set) = if let Some(ref file_path) = draw.mesh.tex_file {
                    trc::trace!("RENDER-TEXTURE mesh={mesh_id} texture={file_path:?}");
                    assert!(file_path.ends_with(".png"), "only pngs are handled, and only ARGB pngs");
                    let f = std::fs::File::open(file_path).expect("provided file exists");
                    let texture_file = img::load(std::io::BufReader::new(f), ImageFormat::Png).unwrap().into_rgba32f();
                    let (texture_width, texture_height) = (texture_file.width(), texture_file.height());
                    Self::copy_sized_slice_to_buffer(&self.staging_texture_buffer.clone(), texture_file.into_raw().as_slice()).unwrap();
                    let texture_image = Image::new(
                        Arc::clone(&self.device_memory_alloc) as _,
                        ImageCreateInfo {
                            format: Format::R32G32B32A32_SFLOAT,
                            view_formats: vec![Format::R32G32B32A32_SFLOAT],
                            extent: [texture_width, texture_height, 1],
                            usage: ImageUsage::INPUT_ATTACHMENT | ImageUsage::COLOR_ATTACHMENT | ImageUsage::SAMPLED | ImageUsage::TRANSFER_DST,
                            tiling: ImageTiling::Optimal,
                            initial_layout: ImageLayout::Undefined,
                            ..Default::default()
                        },
                        AllocationCreateInfo {
                            memory_type_filter: MemoryTypeFilter::PREFER_DEVICE,
                            ..Default::default()
                        },
                    ).unwrap();
                    let texture_image_view = ImageView::new(
                        Arc::clone(&texture_image),
                        ImageViewCreateInfo {
                            format: Format::R32G32B32A32_SFLOAT,
                            ..ImageViewCreateInfo::from_image(&texture_image)
                        },
                    ).unwrap();

                    // TODO Batch these somehow.
                    let mut image_copy_builder = RecordingCommandBuffer::new(
                        Arc::clone(&self.command_buffer_alloc),
                        self.selected_device_queue.queue_family_index(),
                        CommandBufferLevel::Primary,
                        CommandBufferBeginInfo {
                            usage: CommandBufferUsage::OneTimeSubmit,
                            inheritance_info: None,
                            ..Default::default()
                        },
                    ).unwrap();
                    image_copy_builder
                        .copy_buffer_to_image(CopyBufferToImageInfo {
                            ..CopyBufferToImageInfo::buffer_image(self.staging_texture_buffer.clone(), Arc::clone(&texture_image))
                        })
                        .unwrap();
                    let image_copy_buffer = image_copy_builder.end().unwrap();
                    vulkano::sync::now(Arc::clone(&self.device))
                        .then_execute(Arc::clone(&self.selected_device_queue), image_copy_buffer)
                        .unwrap()
                        .flush()
                        .unwrap();

                    let texture_descriptor_set = DescriptorSet::new(
                        Arc::clone(&self.descriptor_set_allocator),
                        Arc::clone(&self.descriptor_set_layouts.texture),
                        [
                            WriteDescriptorSet::image_view(0, texture_image_view),
                            WriteDescriptorSet::sampler(
                                1,
                                Arc::clone(&self.texture_sampler),
                            ),
                        ],
                        [],
                    ).unwrap();

                    (Some(texture_image), Some(texture_descriptor_set))
                } else {
                    (None, None)
                };

                self.loaded_models.insert(draw.mesh.mesh_id,
                    LoadedModelHandles {
                        texture_image,
                        texture_descriptor_set,
                        vertex_offset: self.vertex_free_start,
                        index_offset: self.index_free_start,
                    }
                );

                self.vertex_free_start += draw.mesh.vec_vv.len() as i32;
                self.index_free_start += draw.mesh.ff.len() as u32;
                let vblen = bytemuck::cast_slice::<_, u8>(draw.mesh.vec_vv.as_slice()).len() as u64;
                let fblen = bytemuck::cast_slice::<_, u8>(draw.mesh.ff.as_slice()).len() as u64;
                self.vertex_free_byte_start += vblen;
                self.index_free_byte_start += fblen;

                trc::trace!("RENDER-COPY mesh={mesh_id} vertex_start={} vertex_len={vblen} face_start={} face_len={fblen}", self.vertex_free_byte_start, self.index_free_byte_start);

                perf.record_load_end();
            }

            perf.record_load_start();
            Self::copy_sized_slice_to_buffer(&self.uniform_per_mesh_buffer, task.instancing_information_bytes().as_slice()).unwrap();
            Self::copy_sized_slice_to_buffer(&self.uniform_light_buffer, task.lights.to_bytes().as_slice()).unwrap();
            Self::copy_sized_slice_to_buffer(&self.uniform_counts_buffer, &[0u32, task.lights.0.len() as u32, 0u32, 0u32]).unwrap();
            Self::copy_sized_slice_to_buffer(&self.uniform_wireframe_buffer, &[if task.draw_wireframe { 1u32 } else { 0u32 }, 0u32, 0u32, 0u32]).unwrap();
            Self::copy_sized_slice_to_buffer(&self.uniform_cam_matrix_buffer, task.cam.get_vp_mat().as_slice()).unwrap();
            perf.record_load_end();
        }

        perf.record_framebuffer_acquisition_start();
        let (active_framebuffer, afidx, framebuffer_future) = {
            let (mut preferred, mut suboptimal, mut acquire_next_image) = swapchain::acquire_next_image(Arc::clone(&window_swapchain.swapchain), None).unwrap();
            const MAX_RECREATION_OCCURRENCES: usize = 3;
            let mut times_recreated = 0;
            while suboptimal && times_recreated < MAX_RECREATION_OCCURRENCES {
                times_recreated += 1;
                window_swapchain.regenerated_swapchain(&window, &self.physical_device, &self.device, &self.device_memory_alloc).unwrap();
                let n = swapchain::acquire_next_image(Arc::clone(&window_swapchain.swapchain), None).unwrap();
                (preferred, suboptimal, acquire_next_image) = n;
            }
            (&window_swapchain.images[preferred as usize], preferred, acquire_next_image)
        };
        perf.record_framebuffer_acquisition_end();

        perf.record_pipeline_construction_start();
        let subpass = Subpass::from(Arc::clone(&window_swapchain.render_pass), 0).unwrap();
        let active_extent = active_framebuffer.image.extent();
        let color_attachment_count = subpass.num_color_attachments();
        let pipeline = if let Some(pipeline) = 'pipeline_cache_check: {
            let Some((cached_extent, cached_color_attachment_count, cached_window_id, cached_pipeline)) = self.pipeline_cache.as_ref() else {
                trc::trace!("RENDER-PASS-PIPELINE-CACHE cache_miss reason=no_cache");
                break 'pipeline_cache_check None;
            };
            if *cached_extent != active_extent {
                trc::trace!("RENDER-PASS-PIPELINE-CACHE cache_miss reason=extent old={:?} new={:?}", cached_extent, active_extent);
                break 'pipeline_cache_check None;
            }
            if *cached_color_attachment_count != color_attachment_count {
                trc::trace!("RENDER-PASS-PIPELINE-CACHE cache_miss reason=attachment_count old={:?} new={:?}", cached_color_attachment_count, color_attachment_count);
                break 'pipeline_cache_check None;
            }
            if *cached_window_id != window.id() {
                trc::trace!("RENDER-PASS-PIPELINE-CACHE cache_miss reason=window_id old={:?} new={:?}", cached_window_id, window.id());
                break 'pipeline_cache_check None;
            }
            Some(cached_pipeline)
        } {
            pipeline.clone()
        } else {
            let pipeline = GraphicsPipeline::new(
                Arc::clone(&self.device),
                None,
                GraphicsPipelineCreateInfo {
                    stages: [
                        PipelineShaderStageCreateInfo::new(self.shaders.vertex.entry_point("main").unwrap()),
                        PipelineShaderStageCreateInfo::new(self.shaders.fragment.entry_point("main").unwrap()),
                        PipelineShaderStageCreateInfo::new(self.shaders.geometry.entry_point("main").unwrap()),
                    ].into_iter().collect(),
                    rasterization_state: Some(RasterizationState {
                        cull_mode: CullMode::Back,
                        ..RasterizationState::default()
                    }),
                    input_assembly_state: Some(InputAssemblyState {
                        topology: PrimitiveTopology::TriangleList,
                        ..Default::default()
                    }),
                    vertex_input_state: Some(
                        VertexInputState::new()
                            .binding(
                                0,
                                VertexInputBindingDescription {
                                    stride: 12 + 12 + 8,
                                    input_rate: VertexInputRate::Vertex,
                                    ..Default::default()
                                },
                            )
                            .attribute(
                                0,
                                VertexInputAttributeDescription {
                                    binding: 0,
                                    format: Format::R32G32B32_SFLOAT,
                                    offset: 0,
                                    ..Default::default()
                                },
                            )
                            .binding(
                                1,
                                VertexInputBindingDescription {
                                    stride: 12 + 12 + 8,
                                    input_rate: VertexInputRate::Vertex,
                                    ..Default::default()
                                },
                            )
                            .attribute(
                                1,
                                VertexInputAttributeDescription {
                                    binding: 0,
                                    format: Format::R32G32B32_SFLOAT,
                                    offset: 12,
                                    ..Default::default()
                                },
                            )
                            .binding(
                                2,
                                VertexInputBindingDescription {
                                    stride: 12 + 12 + 8,
                                    input_rate: VertexInputRate::Vertex,
                                    ..Default::default()
                                },
                            )
                            .attribute(
                                2,
                                VertexInputAttributeDescription {
                                    binding: 0,
                                    format: Format::R32G32_SFLOAT,
                                    offset: 12 + 12,
                                    ..Default::default()
                                },
                            )
                    ),
                    viewport_state: Some(ViewportState {
                        viewports: [Viewport {
                            offset: [0.0; 2],
                            extent: [active_framebuffer.image.extent()[0] as f32, active_framebuffer.image.extent()[1] as f32],
                            depth_range: 0.0..=1.0
                        }].into_iter().collect(),
                        scissors: [Scissor {
                            offset: [0; 2],
                            extent: [active_framebuffer.image.extent()[0], active_framebuffer.image.extent()[1]],
                        }].into_iter().collect(),
                        ..ViewportState::default()
                    }),
                    multisample_state: Some(MultisampleState {
                        rasterization_samples: SampleCount::Sample1,
                        ..Default::default()
                    }),
                    color_blend_state: Some(ColorBlendState::with_attachment_states(
                        subpass.num_color_attachments(),
                        ColorBlendAttachmentState::default(),
                    )),
                    depth_stencil_state: Some(DepthStencilState {
                        depth: Some(DepthState::simple()),
                        ..Default::default()
                    }),
                    subpass: Some(PipelineSubpassType::BeginRenderPass(subpass)),
                    ..GraphicsPipelineCreateInfo::layout(Arc::clone(&self.pipeline_layout))
                }
            ).unwrap();
            trc::debug!("RENDER-PASS-PIPELINE descriptor_set={}", pipeline.num_used_descriptor_sets());
            self.pipeline_cache = Some((active_extent, color_attachment_count, window.id(), pipeline.clone()));
            pipeline
        };
        perf.record_pipeline_construction_end();

        perf.record_command_record_start();

        let base_queue = &self.selected_device_queue;
        let mut builder = RecordingCommandBuffer::new(
            Arc::clone(&self.command_buffer_alloc),
            self.selected_device_queue.queue_family_index(),
            CommandBufferLevel::Primary,
            CommandBufferBeginInfo {
                usage: CommandBufferUsage::OneTimeSubmit,
                inheritance_info: None,
                ..Default::default()
            }
        )
            .tap_err(|e| trc::error!("TOTALITY-RENDERER-RENDER-TO-FAILED source=clear_pipeline error=command_buffer_alloc {e}"))?;
        builder
            .begin_render_pass(
                RenderPassBeginInfo {
                    clear_values: vec![
                        Some(task.clear_color.clone().into()),
                        Some(ClearValue::Depth(1f32))
                    ],
                    ..RenderPassBeginInfo::framebuffer(Arc::clone(&active_framebuffer.framebuffer))
                },
                SubpassBeginInfo {
                    contents: SubpassContents::Inline,
                    ..Default::default()
                }
            )
            .unwrap()
            .clear_attachments(
                [
                    ClearAttachment::Color {
                        color_attachment: 0,
                        clear_value: task.clear_color,
                    }
                ].into_iter().collect(),
                [
                    ClearRect {
                        offset: [0, 0],
                        extent: [active_framebuffer.image.extent()[0], active_framebuffer.image.extent()[1]],
                        array_layers: 0..1,
                    }
                ].into_iter().collect()
            )
            .unwrap()
            .bind_pipeline_graphics(pipeline)
            .unwrap()
            .bind_vertex_buffers(0, self.vertex_buffer.clone())
            .unwrap()
            .bind_vertex_buffers(1, self.vertex_buffer.clone())
            .unwrap()
            .bind_vertex_buffers(2, self.vertex_buffer.clone())
            .unwrap()
            .bind_descriptor_sets(
                PipelineBindPoint::Graphics,
                Arc::clone(&self.pipeline_layout),
                0,
                (Arc::clone(&self.descriptor_set), Arc::clone(&self.empty_texture_descriptor_set))
            )
            .unwrap()
            .bind_index_buffer(IndexBuffer::U32(self.face_buffer.clone().reinterpret()))
            .unwrap();
        let mut current_instance_buffer_idx = 0;
        for draw in task.draws.iter() {
            let handle = &self.loaded_models[&draw.mesh.mesh_id];
            let vert_count = draw.mesh.vec_vv.len() as i32;
            let index_count = draw.mesh.ff.len() as u32;
            let instance_count = draw.instancing_information.len() as u32;
            trc::trace!(
                "RENDER-PASS-DRAW vertex_start={} vertex_count={vert_count} index_start={} index_count={index_count} instance_start={current_instance_buffer_idx} instance_count={instance_count}",
                handle.vertex_offset,
                handle.index_offset,
            );
            let texture_descriptor_set = if let Some(ref descriptor_set) = handle.texture_descriptor_set {
                descriptor_set
            } else {
                &self.empty_texture_descriptor_set
            };
            builder
                .bind_descriptor_sets(
                    PipelineBindPoint::Graphics,
                    Arc::clone(&self.pipeline_layout),
                    1,
                    Arc::clone(&texture_descriptor_set),
                )
                .unwrap();
            unsafe {
                builder
                    .draw_indexed(
                        index_count,
                        instance_count,
                        handle.index_offset,
                        handle.vertex_offset,
                        current_instance_buffer_idx,
                    )
            }.unwrap();
            current_instance_buffer_idx += instance_count;
            trc::trace!("RENDER-PASS-DRAW-COMPLETE");
        }
        builder
            .end_render_pass(SubpassEndInfo { ..Default::default() })
            .unwrap();
        let clear_buffer = builder.end().unwrap();

        perf.record_command_record_end();

        perf.record_draw_start();

        vulkano::sync::now(Arc::clone(&self.device))
            .join(framebuffer_future)
            .then_execute(Arc::clone(&base_queue), clear_buffer)
            .unwrap()
            .then_swapchain_present(Arc::clone(base_queue), SwapchainPresentInfo::swapchain_image_index(Arc::clone(&window_swapchain.swapchain), afidx))
            .flush()
            .unwrap();

        perf.record_draw_end();

        perf.record_end();
        trc::debug!("RENDER-PASS-COMPLETE");

        Ok(())
    }
}

pub struct DepthImage {
    pub view: Arc<ImageView>,
    pub image: Arc<Image>,
}

pub struct FramebufferedImage {
    pub framebuffer: Arc<Framebuffer>,
    pub view: Arc<ImageView>,
    pub image: Arc<Image>,
}

pub struct RendererWindowSwapchain {
     cached_dimensions: PhysicalSize<u32>,
     surface: Arc<Surface>,
     composite_alpha: CompositeAlpha,
     swapchain: Arc<Swapchain>,
     render_pass: Arc<RenderPass>,
     depth_image: DepthImage,
     images: Vec<FramebufferedImage>,
}

impl RendererWindowSwapchain {
    fn is_stale_for_window(&self, window: &Arc<Window>) -> bool {
        let dimensions = window.inner_size();
        self.cached_dimensions == dimensions
    }

    fn generate_swapchain(vulkan: &Arc<Instance>, window: &Arc<Window>, pd: &Arc<PhysicalDevice>, device: &Arc<Device>, mem_alloc: &Arc<StandardMemoryAllocator>) -> Result<Self, Validated<VulkanError>> {
        let surface = Surface::from_window(
            Arc::clone(&vulkan),
            Arc::clone(window)
        ).unwrap();

        let (dimensions, composite_alpha, render_pass, swapchain, depth_image, images) = Self::generate_swapchain_from_surface(&surface, window, pd, device, mem_alloc)?;

        Ok(RendererWindowSwapchain { cached_dimensions: dimensions, surface, composite_alpha, swapchain, render_pass, depth_image, images })
    }

    fn regenerated_swapchain(&mut self, window: &Arc<Window>, pd: &Arc<PhysicalDevice>, device: &Arc<Device>, mem_alloc: &Arc<StandardMemoryAllocator>) -> Result<(), Validated<VulkanError>> {
        let (dimensions, composite_alpha, render_pass, swapchain, depth_image, images) = Self::generate_swapchain_from_surface(&self.surface, window, pd, device, mem_alloc)?;

        self.cached_dimensions = dimensions;
        self.composite_alpha = composite_alpha;
        self.swapchain = swapchain;
        self.render_pass = render_pass;
        self.depth_image = depth_image;
        self.images = images;

        Ok(())
    }

    fn generate_swapchain_from_surface(surface: &Arc<Surface>, window: &Arc<Window>, pd: &Arc<PhysicalDevice>, device: &Arc<Device>, mem_alloc: &Arc<StandardMemoryAllocator>) -> Result<(
        PhysicalSize<u32>,
        CompositeAlpha,
        Arc<RenderPass>,
        Arc<Swapchain>,
        DepthImage,
        Vec<FramebufferedImage>,
    ), Validated<VulkanError>> {
        let capabilities = pd.surface_capabilities(&surface, Default::default())
            .tap_err(|e| trc::error!("TOTALITY-RENDERER-RENDER-TO-FAILED source=surface_capability {e}"))?;
        let dimensions = window.inner_size();
        // Assume one is available.
        let composite_alpha = capabilities.supported_composite_alpha.into_iter().next().unwrap();
        let format = pd
            .surface_formats(&surface, Default::default())
            .expect("surface lookup done")[0].0;

        let (swapchain, raw_images) = Swapchain::new(
            Arc::clone(device),
            Arc::clone(surface),
            SwapchainCreateInfo {
                composite_alpha,
                image_format: format,
                image_extent: dimensions.into(),
                image_usage: ImageUsage::COLOR_ATTACHMENT | ImageUsage::TRANSFER_DST,
                min_image_count: capabilities.min_image_count + 1,
                ..Default::default()
            },
        )?;

        let depth_image = Image::new(
            Arc::clone(mem_alloc) as _,
            ImageCreateInfo {
                image_type: ImageType::Dim2d,
                format: Format::D16_UNORM,
                extent: raw_images[0].extent(),
                usage: ImageUsage::DEPTH_STENCIL_ATTACHMENT | ImageUsage::TRANSIENT_ATTACHMENT,
                ..Default::default()
            },
            AllocationCreateInfo {
                ..Default::default()
            },
        ).unwrap();
        let depth_attachment = ImageView::new_default(Arc::clone(&depth_image)).unwrap();

        let render_pass = vulkano::single_pass_renderpass!(
            Arc::clone(device),
            attachments: {
                color: {
                    format: format,
                    samples: 1,
                    load_op: Clear,
                    store_op: Store,
                },
                depth_stencil: {
                    format: Format::D16_UNORM,
                    samples: 1,
                    load_op: Clear,
                    store_op: DontCare,
                },
            },
            pass: {
                color: [color],
                depth_stencil: {depth_stencil},
            },
        ).tap_err(|e| trc::error!("TOTALITY-RENDERER-RENDER-TO-FAILED source=render_pass {e}"))?;


        let images = raw_images.into_iter().map(|image| {
            let view = ImageView::new_default(Arc::clone(&image))?;
            let framebuffer = Framebuffer::new(
                Arc::clone(&render_pass),
                vulkano::render_pass::FramebufferCreateInfo {
                    attachments: vec![Arc::clone(&view), Arc::clone(&depth_attachment)],
                    ..Default::default()
                },
            )?;
            Ok(FramebufferedImage {
                image,
                view,
                framebuffer,
            })
        }).collect::<Result<Vec<_>, Validated<VulkanError>>>()?;

        Ok((dimensions, composite_alpha, render_pass, swapchain, DepthImage {
            image: depth_image,
            view: depth_attachment,
        }, images))
    }
}
