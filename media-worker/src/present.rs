//! Shows decoded 10-bit video in an X11 window through Vulkan.
//!
//! GStreamer's sinks take 8-bit pictures only. HDR needs a 10-bit swapchain
//! in the HDR10 colour space, which Gamescope's Vulkan layer offers for a
//! window that no child window covers. The picture's own colour description
//! picks the swapchain: PQ goes out as HDR10, everything else as SDR.
//!
//! The stream overlay is drawn here too, from a picture of it. As a child
//! window it would cover the video, and Gamescope then shows the window's
//! X11 content, which cannot be HDR.

use crate::overlay::Layer;
use ash::{ext, khr, vk};
use gamestream::HdrDisplay;
use std::sync::Arc;
use gstreamer_video as gst_video;
use gstreamer_video::VideoFrameExt;
use std::ffi::CStr;
use x11_dl::xlib;

type Result<T> = std::result::Result<T, String>;

fn failed(what: &'static str) -> impl Fn(vk::Result) -> String {
    move |error| format!("Vulkan could not {what}: {error}")
}

/// Luma weights of red and blue for a colour matrix.
const BT2020: (f32, f32) = (0.2627, 0.0593);
const BT709: (f32, f32) = (0.2126, 0.0722);
const BT601: (f32, f32) = (0.299, 0.114);

/// The matrix, by columns, that takes a P010 sample to R'G'B': luma, Cb and
/// Cr as the sampler returns them (the 10-bit code in the high bits of 16),
/// then 1.
fn colour_matrix((kr, kb): (f32, f32), full_range: bool) -> [[f32; 4]; 4] {
    // A sampled value times this is the 10-bit code.
    let code = 65535.0 / 64.0;
    let (luma_scale, luma_zero, chroma_scale) = if full_range { (1023.0, 0.0, 1023.0) } else { (876.0, 64.0, 896.0) };
    let (y, y0) = (code / luma_scale, -luma_zero / luma_scale);
    let (c, c0) = (code / chroma_scale, -512.0 / chroma_scale);
    let kg = 1.0 - kr - kb;
    let (red_cr, blue_cb) = (2.0 * (1.0 - kr), 2.0 * (1.0 - kb));
    let (green_cb, green_cr) = (-kb * blue_cb / kg, -kr * red_cr / kg);
    [
        [y, y, y, 0.0],
        [0.0, green_cb * c, blue_cb * c, 0.0],
        [red_cr * c, green_cr * c, 0.0, 0.0],
        [y0 + red_cr * c0, y0 + (green_cb + green_cr) * c0, y0 + blue_cb * c0, 1.0],
    ]
}

/// Where a picture goes in a window: as large as fits, centred, in its shape.
fn fit(picture: (u32, u32), window: (u32, u32)) -> (f32, f32, f32, f32) {
    let (pw, ph, ww, wh) = (picture.0 as f32, picture.1 as f32, window.0 as f32, window.1 as f32);
    let scale = (ww / pw).min(wh / ph);
    let (width, height) = (pw * scale, ph * scale);
    ((ww - width) / 2.0, (wh - height) / 2.0, width, height)
}

/// The surface format for a picture: HDR10 for PQ, else SDR, 10-bit if the
/// surface has it.
fn surface_format(offered: &[vk::SurfaceFormatKHR], pq: bool) -> Option<vk::SurfaceFormatKHR> {
    let ten_bit = [vk::Format::A2B10G10R10_UNORM_PACK32, vk::Format::A2R10G10B10_UNORM_PACK32];
    let eight_bit = [vk::Format::B8G8R8A8_UNORM, vk::Format::R8G8B8A8_UNORM];
    let space = if pq { vk::ColorSpaceKHR::HDR10_ST2084_EXT } else { vk::ColorSpaceKHR::SRGB_NONLINEAR };
    let find = |formats: &[vk::Format]| formats.iter()
        .find_map(|format| offered.iter().find(|offer| offer.format == *format && offer.color_space == space).copied());
    find(&ten_bit).or_else(|| if pq { None } else { find(&eight_bit) })
}

struct Target {
    pq: bool,
    swapchain: vk::SwapchainKHR,
    extent: vk::Extent2D,
    pass: vk::RenderPass,
    pipeline: vk::Pipeline,
    overlay_pipeline: vk::Pipeline,
    views: Vec<vk::ImageView>,
    framebuffers: Vec<vk::Framebuffer>,
    /// Signals that an image is drawn. One for each image: the compositor
    /// can still wait on the last frame's when the next frame is drawn.
    drawn: Vec<vk::Semaphore>,
}

struct Plane {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    extent: vk::Extent2D,
    /// Bytes of one pixel.
    pixel: usize,
}

/// A buffer the CPU fills, from which a frame is copied to its textures.
struct Staging {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: *mut u8,
}

impl Plane {
    fn bytes(&self) -> u64 {
        self.extent.width as u64 * self.extent.height as u64 * self.pixel as u64
    }
}

/// Copies `rows` rows of `row` bytes, `stride` apart in `data`, to `to`.
unsafe fn copy_rows(data: &[u8], stride: usize, row: usize, rows: usize, to: *mut u8) -> Result<()> {
    for line in 0..rows {
        let source = data.get(line * stride..line * stride + row).ok_or("A picture is smaller than its description")?;
        unsafe { std::ptr::copy_nonoverlapping(source.as_ptr(), to.add(line * row), row); }
    }
    Ok(())
}

struct Picture {
    size: (u32, u32),
    planes: [Plane; 2],
    staging: Staging,
}

/// The texture the overlay's picture is drawn from.
struct Sheet {
    plane: Plane,
    staging: Staging,
}

pub struct Presenter {
    xlib: xlib::Xlib,
    display: *mut xlib::Display,
    window: xlib::Window,
    _entry: ash::Entry,
    instance: ash::Instance,
    surfaces: khr::surface::Instance,
    surface: vk::SurfaceKHR,
    gpu: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    swapchains: khr::swapchain::Device,
    hdr_metadata: Option<ext::hdr_metadata::Device>,
    pool: vk::CommandPool,
    commands: vk::CommandBuffer,
    fence: vk::Fence,
    acquired: vk::Semaphore,
    sampler: vk::Sampler,
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    descriptors: vk::DescriptorPool,
    set: vk::DescriptorSet,
    overlay_set: vk::DescriptorSet,
    vertex: vk::ShaderModule,
    fragment: vk::ShaderModule,
    overlay_fragment: vk::ShaderModule,
    target: Option<Target>,
    picture: Option<Picture>,
    sheet: Option<Sheet>,
    /// The overlay picture the sheet holds.
    sheet_shows: Option<Arc<Layer>>,
    /// The host display whose description the swapchain carries.
    described: Option<HdrDisplay>,
}

// Everything here is used from the one thread that shows the frames.
unsafe impl Send for Presenter {}

impl Presenter {
    /// Prepares to show pictures in the X11 window `window`.
    pub fn new(window: u64) -> Result<Self> {
        let xlib = xlib::Xlib::open().map_err(|error| format!("libX11: {error}"))?;
        // A connection of its own: the worker's display belongs to another thread.
        let display = unsafe { (xlib.XOpenDisplay)(std::ptr::null()) };
        if display.is_null() { return Err("X11 display unavailable".into()); }
        unsafe {
            let entry = ash::Entry::load().map_err(|error| format!("Vulkan is unavailable: {error}"))?;
            let offered = entry.enumerate_instance_extension_properties(None).map_err(failed("list its extensions"))?;
            let has = |name: &CStr| offered.iter().any(|extension| extension.extension_name_as_c_str() == Ok(name));
            let mut extensions = vec![khr::surface::NAME.as_ptr(), khr::xlib_surface::NAME.as_ptr()];
            // Without it only the sRGB colour space may be asked for.
            if has(ext::swapchain_colorspace::NAME) { extensions.push(ext::swapchain_colorspace::NAME.as_ptr()); }
            let app = vk::ApplicationInfo::default().application_name(c"playwake-media").api_version(vk::API_VERSION_1_1);
            let instance = entry.create_instance(
                &vk::InstanceCreateInfo::default().application_info(&app).enabled_extension_names(&extensions), None)
                .map_err(failed("start"))?;
            let surfaces = khr::surface::Instance::new(&entry, &instance);
            let surface = khr::xlib_surface::Instance::new(&entry, &instance).create_xlib_surface(
                &vk::XlibSurfaceCreateInfoKHR::default().dpy(display.cast()).window(window as vk::Window), None)
                .map_err(failed("use the video window"))?;

            // A GPU that draws to this window; a software one only as the last choice.
            let mut choice = None;
            for gpu in instance.enumerate_physical_devices().map_err(failed("list the GPUs"))? {
                let software = instance.get_physical_device_properties(gpu).device_type == vk::PhysicalDeviceType::CPU;
                let families = instance.get_physical_device_queue_family_properties(gpu);
                let family = (0..families.len() as u32).find(|family| {
                    families[*family as usize].queue_flags.contains(vk::QueueFlags::GRAPHICS)
                        && surfaces.get_physical_device_surface_support(gpu, *family, surface).unwrap_or(false)
                });
                if let Some(family) = family {
                    if choice.is_none() || !software { choice = Some((gpu, family)); }
                    if !software { break; }
                }
            }
            let (gpu, family) = choice.ok_or("No GPU can draw to the video window")?;
            let device_offers = instance.enumerate_device_extension_properties(gpu).map_err(failed("list the GPU's extensions"))?;
            let with_metadata = device_offers.iter()
                .any(|extension| extension.extension_name_as_c_str() == Ok(ext::hdr_metadata::NAME));
            let mut device_extensions = vec![khr::swapchain::NAME.as_ptr()];
            if with_metadata { device_extensions.push(ext::hdr_metadata::NAME.as_ptr()); }
            let queues = [vk::DeviceQueueCreateInfo::default().queue_family_index(family).queue_priorities(&[1.0])];
            let device = instance.create_device(gpu,
                &vk::DeviceCreateInfo::default().queue_create_infos(&queues).enabled_extension_names(&device_extensions), None)
                .map_err(failed("open the GPU"))?;
            let queue = device.get_device_queue(family, 0);
            let swapchains = khr::swapchain::Device::new(&instance, &device);
            let hdr_metadata = with_metadata.then(|| ext::hdr_metadata::Device::new(&instance, &device));

            let pool = device.create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER), None).map_err(failed("make a command pool"))?;
            let commands = device.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool)
                .level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(1)).map_err(failed("make a command buffer"))?[0];
            let fence = device.create_fence(&vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED), None)
                .map_err(failed("make a fence"))?;
            let acquired = device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None).map_err(failed("make a semaphore"))?;
            let sampler = device.create_sampler(&vk::SamplerCreateInfo::default()
                .mag_filter(vk::Filter::LINEAR).min_filter(vk::Filter::LINEAR)
                .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE).address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE), None).map_err(failed("make a sampler"))?;
            let bindings = [0, 1].map(|binding| vk::DescriptorSetLayoutBinding::default().binding(binding)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER).descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT));
            let set_layout = device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings), None).map_err(failed("lay out the planes"))?;
            let set_layouts = [set_layout];
            let constants = [vk::PushConstantRange::default().stage_flags(vk::ShaderStageFlags::FRAGMENT).size(64)];
            let layout = device.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts)
                .push_constant_ranges(&constants), None).map_err(failed("lay out the pipeline"))?;
            let sizes = [vk::DescriptorPoolSize::default().ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER).descriptor_count(4)];
            let descriptors = device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default().max_sets(2).pool_sizes(&sizes), None).map_err(failed("make a descriptor pool"))?;
            // One set for the video's planes, one for the overlay, which uses the first binding only.
            let sets = device.allocate_descriptor_sets(&vk::DescriptorSetAllocateInfo::default().descriptor_pool(descriptors)
                .set_layouts(&[set_layout, set_layout])).map_err(failed("make the descriptor sets"))?;
            let (set, overlay_set) = (sets[0], sets[1]);
            let shader = |bytes: &[u8]| -> Result<vk::ShaderModule> {
                let code = ash::util::read_spv(&mut std::io::Cursor::new(bytes)).map_err(|error| format!("shader: {error}"))?;
                device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None).map_err(failed("load a shader"))
            };
            let vertex = shader(include_bytes!("../shaders/present.vert.spv"))?;
            let fragment = shader(include_bytes!("../shaders/present.frag.spv"))?;
            let overlay_fragment = shader(include_bytes!("../shaders/overlay.frag.spv"))?;
            Ok(Self { xlib, display, window: window as xlib::Window, _entry: entry, instance, surfaces, surface, gpu, device, queue, swapchains,
                hdr_metadata, pool, commands, fence, acquired, sampler, set_layout, layout, descriptors, set, overlay_set,
                vertex, fragment, overlay_fragment, target: None, picture: None, sheet: None, sheet_shows: None, described: None })
        }
    }

    fn memory(&self, needs: vk::MemoryRequirements, flags: vk::MemoryPropertyFlags) -> Result<vk::DeviceMemory> {
        let known = unsafe { self.instance.get_physical_device_memory_properties(self.gpu) };
        let kind = (0..known.memory_type_count).find(|kind| needs.memory_type_bits & (1 << kind) != 0
            && known.memory_types[*kind as usize].property_flags.contains(flags)).ok_or("The GPU has no memory of the needed kind")?;
        unsafe { self.device.allocate_memory(
            &vk::MemoryAllocateInfo::default().allocation_size(needs.size).memory_type_index(kind), None) }
            .map_err(failed("get memory"))
    }

    unsafe fn drop_target(&mut self, keep_swapchain: bool) -> vk::SwapchainKHR {
        let Some(target) = self.target.take() else { return vk::SwapchainKHR::null() };
        unsafe {
            let _ = self.device.device_wait_idle();
            for framebuffer in target.framebuffers { self.device.destroy_framebuffer(framebuffer, None); }
            for drawn in target.drawn { self.device.destroy_semaphore(drawn, None); }
            for view in target.views { self.device.destroy_image_view(view, None); }
            self.device.destroy_pipeline(target.pipeline, None);
            self.device.destroy_pipeline(target.overlay_pipeline, None);
            self.device.destroy_render_pass(target.pass, None);
            if keep_swapchain { return target.swapchain; }
            self.swapchains.destroy_swapchain(target.swapchain, None);
        }
        vk::SwapchainKHR::null()
    }

    /// Makes the swapchain for the window as it is now, in the colour space
    /// the picture needs.
    unsafe fn retarget(&mut self, pq: bool) -> Result<()> {
        unsafe {
            let old = self.drop_target(true);
            let made = self.make_target(pq, old);
            if old != vk::SwapchainKHR::null() { self.swapchains.destroy_swapchain(old, None); }
            self.target = Some(made?);
        }
        self.described = None;
        Ok(())
    }

    unsafe fn make_target(&mut self, pq: bool, old: vk::SwapchainKHR) -> Result<Target> {
        unsafe {
            let limits = self.surfaces.get_physical_device_surface_capabilities(self.gpu, self.surface)
                .map_err(failed("read the window"))?;
            let formats = self.surfaces.get_physical_device_surface_formats(self.gpu, self.surface)
                .map_err(failed("read the window's formats"))?;
            let format = surface_format(&formats, pq)
                .ok_or(if pq { "This display does not take HDR from Playwake" } else { "The video window has no usable format" })?;
            let modes = self.surfaces.get_physical_device_surface_present_modes(self.gpu, self.surface)
                .map_err(failed("read the window's present modes"))?;
            // Show the newest frame at once; the compositor keeps it tear-free.
            let mode = [vk::PresentModeKHR::MAILBOX, vk::PresentModeKHR::IMMEDIATE].into_iter()
                .find(|mode| modes.contains(mode)).unwrap_or(vk::PresentModeKHR::FIFO);
            let extent = if limits.current_extent.width != u32::MAX { limits.current_extent } else {
                let (mut root, mut x, mut y, mut width, mut height, mut border, mut depth) = (0, 0, 0, 0, 0, 0, 0);
                (self.xlib.XGetGeometry)(self.display, self.window, &mut root, &mut x, &mut y,
                    &mut width, &mut height, &mut border, &mut depth);
                vk::Extent2D { width, height }
            };
            if extent.width == 0 || extent.height == 0 { return Err("The video window has no size".into()); }
            let mut count = limits.min_image_count.max(3);
            if limits.max_image_count != 0 { count = count.min(limits.max_image_count); }
            let swapchain = self.swapchains.create_swapchain(&vk::SwapchainCreateInfoKHR::default()
                .surface(self.surface).min_image_count(count).image_format(format.format).image_color_space(format.color_space)
                .image_extent(extent).image_array_layers(1).image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
                .image_sharing_mode(vk::SharingMode::EXCLUSIVE).pre_transform(limits.current_transform)
                .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE).present_mode(mode).clipped(true).old_swapchain(old), None)
                .map_err(failed("make the swapchain"))?;
            eprintln!("pw-present swapchain {}x{} {:?} {:?} {:?}", extent.width, extent.height, format.format,
                format.color_space, mode);

            let attachments = [vk::AttachmentDescription::default().format(format.format).samples(vk::SampleCountFlags::TYPE_1)
                .load_op(vk::AttachmentLoadOp::CLEAR).store_op(vk::AttachmentStoreOp::STORE)
                .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE).stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
                .initial_layout(vk::ImageLayout::UNDEFINED).final_layout(vk::ImageLayout::PRESENT_SRC_KHR)];
            let colour = [vk::AttachmentReference::default().attachment(0).layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
            let subpasses = [vk::SubpassDescription::default().pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
                .color_attachments(&colour)];
            // The image changes layout only once it is acquired, which the frame waits for at this stage.
            let after_acquire = [vk::SubpassDependency::default().src_subpass(vk::SUBPASS_EXTERNAL).dst_subpass(0)
                .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)];
            let pass = self.device.create_render_pass(&vk::RenderPassCreateInfo::default().attachments(&attachments)
                .subpasses(&subpasses).dependencies(&after_acquire), None)
                .map_err(failed("make the render pass"))?;

            let stages = |fragment| [
                vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::VERTEX).module(self.vertex).name(c"main"),
                vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::FRAGMENT).module(fragment).name(c"main"),
            ];
            let (video_stages, overlay_stages) = (stages(self.fragment), stages(self.overlay_fragment));
            let input = vk::PipelineVertexInputStateCreateInfo::default();
            let assembly = vk::PipelineInputAssemblyStateCreateInfo::default().topology(vk::PrimitiveTopology::TRIANGLE_LIST);
            let viewport = vk::PipelineViewportStateCreateInfo::default().viewport_count(1).scissor_count(1);
            let raster = vk::PipelineRasterizationStateCreateInfo::default().polygon_mode(vk::PolygonMode::FILL)
                .cull_mode(vk::CullModeFlags::NONE).line_width(1.0);
            let samples = vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(vk::SampleCountFlags::TYPE_1);
            let blends = [vk::PipelineColorBlendAttachmentState::default().color_write_mask(vk::ColorComponentFlags::RGBA)];
            let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blends);
            let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
            let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
            let drawing = |stages| vk::GraphicsPipelineCreateInfo::default().stages(stages).vertex_input_state(&input)
                .input_assembly_state(&assembly).viewport_state(&viewport).rasterization_state(&raster)
                .multisample_state(&samples).color_blend_state(&blend).dynamic_state(&dynamic)
                .layout(self.layout).render_pass(pass);
            let pipelines = self.device.create_graphics_pipelines(vk::PipelineCache::null(),
                &[drawing(&video_stages), drawing(&overlay_stages)], None)
                .map_err(|(_, error)| failed("make the pipelines")(error))?;
            let (pipeline, overlay_pipeline) = (pipelines[0], pipelines[1]);

            let mut views = Vec::new();
            let mut framebuffers = Vec::new();
            let mut drawn = Vec::new();
            for image in self.swapchains.get_swapchain_images(swapchain).map_err(failed("read the swapchain"))? {
                let view = self.device.create_image_view(&vk::ImageViewCreateInfo::default().image(image)
                    .view_type(vk::ImageViewType::TYPE_2D).format(format.format).subresource_range(COLOUR), None)
                    .map_err(failed("view the swapchain"))?;
                let attached = [view];
                framebuffers.push(self.device.create_framebuffer(&vk::FramebufferCreateInfo::default().render_pass(pass)
                    .attachments(&attached).width(extent.width).height(extent.height).layers(1), None)
                    .map_err(failed("make a framebuffer"))?);
                views.push(view);
                drawn.push(self.device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
                    .map_err(failed("make a semaphore"))?);
            }
            Ok(Target { pq, swapchain, extent, pass, pipeline, overlay_pipeline, views, framebuffers, drawn })
        }
    }

    unsafe fn plane(&self, format: vk::Format, extent: vk::Extent2D, pixel: usize) -> Result<Plane> {
        unsafe {
            let image = self.device.create_image(&vk::ImageCreateInfo::default().image_type(vk::ImageType::TYPE_2D)
                .format(format).extent(vk::Extent3D { width: extent.width, height: extent.height, depth: 1 })
                .mip_levels(1).array_layers(1).samples(vk::SampleCountFlags::TYPE_1).tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST), None)
                .map_err(failed("make a texture"))?;
            let memory = self.memory(self.device.get_image_memory_requirements(image), vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
            self.device.bind_image_memory(image, memory, 0).map_err(failed("back a texture"))?;
            let view = self.device.create_image_view(&vk::ImageViewCreateInfo::default().image(image)
                .view_type(vk::ImageViewType::TYPE_2D).format(format).subresource_range(COLOUR), None)
                .map_err(failed("view a texture"))?;
            Ok(Plane { image, memory, view, extent, pixel })
        }
    }

    unsafe fn staging(&self, bytes: u64) -> Result<Staging> {
        unsafe {
            let buffer = self.device.create_buffer(&vk::BufferCreateInfo::default().size(bytes)
                .usage(vk::BufferUsageFlags::TRANSFER_SRC), None).map_err(failed("make a frame buffer"))?;
            let memory = self.memory(self.device.get_buffer_memory_requirements(buffer),
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT)?;
            self.device.bind_buffer_memory(buffer, memory, 0).map_err(failed("back a frame buffer"))?;
            let mapped = self.device.map_memory(memory, 0, bytes, vk::MemoryMapFlags::empty())
                .map_err(failed("map a frame buffer"))?.cast::<u8>();
            Ok(Staging { buffer, memory, mapped })
        }
    }

    unsafe fn drop_textures(&mut self, planes: impl IntoIterator<Item = Plane>, staging: Staging) {
        unsafe {
            let _ = self.device.device_wait_idle();
            for plane in planes {
                self.device.destroy_image_view(plane.view, None);
                self.device.destroy_image(plane.image, None);
                self.device.free_memory(plane.memory, None);
            }
            self.device.destroy_buffer(staging.buffer, None);
            self.device.free_memory(staging.memory, None);
        }
    }

    /// Points a descriptor set's bindings at textures, in order.
    unsafe fn bind(&self, set: vk::DescriptorSet, planes: &[&Plane]) {
        let images: Vec<_> = planes.iter().map(|plane| [vk::DescriptorImageInfo::default().sampler(self.sampler)
            .image_view(plane.view).image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)]).collect();
        let writes: Vec<_> = images.iter().enumerate().map(|(binding, image)| vk::WriteDescriptorSet::default()
            .dst_set(set).dst_binding(binding as u32).descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(image)).collect();
        unsafe { self.device.update_descriptor_sets(&writes, &[]); }
    }

    /// Makes the two plane textures of a P010 picture and the buffer that
    /// carries a frame to them.
    unsafe fn make_picture(&mut self, size: (u32, u32)) -> Result<Picture> {
        unsafe {
            let half = vk::Extent2D { width: size.0.div_ceil(2), height: size.1.div_ceil(2) };
            let full = vk::Extent2D { width: size.0, height: size.1 };
            let planes = [self.plane(vk::Format::R16_UNORM, full, 2)?, self.plane(vk::Format::R16G16_UNORM, half, 4)?];
            let staging = self.staging(planes.iter().map(Plane::bytes).sum())?;
            self.bind(self.set, &[&planes[0], &planes[1]]);
            Ok(Picture { size, planes, staging })
        }
    }

    /// Makes the texture for an overlay picture of this size.
    unsafe fn make_sheet(&mut self, extent: vk::Extent2D) -> Result<Sheet> {
        unsafe {
            let plane = self.plane(vk::Format::B8G8R8A8_UNORM, extent, 4)?;
            let staging = self.staging(plane.bytes())?;
            self.bind(self.overlay_set, &[&plane]);
            Ok(Sheet { plane, staging })
        }
    }

    /// Tells the compositor which display the host mastered the picture for.
    unsafe fn describe(&mut self, display: HdrDisplay) {
        let (Some(metadata), Some(target)) = (&self.hdr_metadata, &self.target) else { return };
        if !target.pq || self.described == Some(display) { return; }
        let point = |[x, y]: [u16; 2]| vk::XYColorEXT { x: f32::from(x) / 50000.0, y: f32::from(y) / 50000.0 };
        let described = vk::HdrMetadataEXT::default()
            .display_primary_red(point(display.primaries[0])).display_primary_green(point(display.primaries[1]))
            .display_primary_blue(point(display.primaries[2])).white_point(point(display.white))
            .max_luminance(f32::from(display.max_luminance)).min_luminance(f32::from(display.min_luminance) / 10000.0)
            .max_content_light_level(f32::from(display.max_content_light))
            .max_frame_average_light_level(f32::from(display.max_frame_average_light));
        unsafe { metadata.set_hdr_metadata(&[target.swapchain], &[described]); }
        self.described = Some(display);
    }

    /// Shows one P010 frame, with the overlay's picture above it if there is
    /// one. `display` is the host's HDR display, once known.
    pub fn show(&mut self, frame: &gst_video::VideoFrameRef<&gstreamer::BufferRef>, display: Option<HdrDisplay>,
        layer: Option<Arc<Layer>>) -> Result<()> {
        let info = frame.info();
        if info.format() != gst_video::VideoFormat::P01010le { return Err(format!("unexpected picture format {:?}", info.format())); }
        let colorimetry = info.colorimetry();
        let pq = colorimetry.transfer() == gst_video::VideoTransferFunction::Smpte2084;
        let weights = match colorimetry.matrix() {
            gst_video::VideoColorMatrix::Bt2020 => BT2020,
            gst_video::VideoColorMatrix::Bt601 => BT601,
            _ => BT709,
        };
        let matrix = colour_matrix(weights, colorimetry.range() == gst_video::VideoColorRange::Range0_255);
        let size = (info.width(), info.height());
        unsafe {
            self.device.wait_for_fences(&[self.fence], true, 1_000_000_000).map_err(failed("finish the last frame"))?;
            if self.picture.as_ref().is_none_or(|picture| picture.size != size) {
                if let Some(old) = self.picture.take() { self.drop_textures(old.planes, old.staging); }
                self.picture = Some(self.make_picture(size)?);
            }
            // The overlay's picture goes to its texture when it changes.
            let layer = layer.filter(|layer| layer.width > 0 && layer.height > 0);
            let fresh = layer.as_ref().filter(|layer| !self.sheet_shows.as_ref().is_some_and(|shown| Arc::ptr_eq(shown, layer))).cloned();
            if let Some(layer) = &fresh {
                let extent = vk::Extent2D { width: layer.width, height: layer.height };
                if self.sheet.as_ref().is_none_or(|sheet| sheet.plane.extent != extent) {
                    if let Some(old) = self.sheet.take() { self.drop_textures([old.plane], old.staging); }
                    self.sheet = Some(self.make_sheet(extent)?);
                }
                let sheet = self.sheet.as_ref().ok_or("no overlay texture")?;
                let row = layer.width as usize * 4;
                copy_rows(&layer.pixels, row, row, layer.height as usize, sheet.staging.mapped)?;
                self.sheet_shows = Some(layer.clone());
            }
            if self.target.as_ref().is_none_or(|target| target.pq != pq) { self.retarget(pq)?; }
            if let Some(display) = display { self.describe(display); }

            // The window changes size when Steam's keyboard opens; then the swapchain is remade.
            let mut index = None;
            for _ in 0..3 {
                let swapchain = self.target.as_ref().ok_or("no swapchain")?.swapchain;
                match self.swapchains.acquire_next_image(swapchain, 1_000_000_000, self.acquired, vk::Fence::null()) {
                    Ok((image, _)) => { index = Some(image); break }
                    Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.retarget(pq)?,
                    Err(error) => return Err(failed("take a swapchain image")(error)),
                }
            }
            let index = index.ok_or("The video window keeps changing")?;
            let picture = self.picture.as_ref().ok_or("no picture")?;
            let target = self.target.as_ref().ok_or("no swapchain")?;

            let mut offset = 0usize;
            let mut regions = Vec::new();
            for (number, plane) in picture.planes.iter().enumerate() {
                let data = frame.plane_data(number as u32).map_err(|error| format!("frame plane: {error}"))?;
                let row = plane.extent.width as usize * plane.pixel;
                copy_rows(data, frame.plane_stride()[number] as usize, row, plane.extent.height as usize,
                    picture.staging.mapped.add(offset))?;
                regions.push(offset as u64);
                offset += plane.bytes() as usize;
            }

            self.device.reset_fences(&[self.fence]).map_err(failed("reset the fence"))?;
            self.device.begin_command_buffer(self.commands,
                &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))
                .map_err(failed("begin the frame"))?;
            // What this frame copies: the video's planes, and the overlay's picture if it changed.
            let mut copies: Vec<(&Plane, vk::Buffer, u64)> = picture.planes.iter().zip(&regions)
                .map(|(plane, start)| (plane, picture.staging.buffer, *start)).collect();
            if let (Some(_), Some(sheet)) = (&fresh, &self.sheet) { copies.push((&sheet.plane, sheet.staging.buffer, 0)); }
            let moves = |old, new, from, to| copies.iter().map(|(plane, ..)| vk::ImageMemoryBarrier::default()
                .old_layout(old).new_layout(new).src_access_mask(from).dst_access_mask(to)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED).dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(plane.image).subresource_range(COLOUR)).collect::<Vec<_>>();
            self.device.cmd_pipeline_barrier(self.commands, vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(), &[], &[],
                &moves(vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::AccessFlags::empty(),
                    vk::AccessFlags::TRANSFER_WRITE));
            for (plane, buffer, start) in &copies {
                self.device.cmd_copy_buffer_to_image(self.commands, *buffer, plane.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[vk::BufferImageCopy::default().buffer_offset(*start)
                        .image_subresource(vk::ImageSubresourceLayers::default().aspect_mask(vk::ImageAspectFlags::COLOR).layer_count(1))
                        .image_extent(vk::Extent3D { width: plane.extent.width, height: plane.extent.height, depth: 1 })]);
            }
            self.device.cmd_pipeline_barrier(self.commands, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(), &[], &[],
                &moves(vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::AccessFlags::TRANSFER_WRITE, vk::AccessFlags::SHADER_READ));

            let clear = [vk::ClearValue { color: vk::ClearColorValue { float32: [0.0, 0.0, 0.0, 1.0] } }];
            let whole = vk::Rect2D { offset: vk::Offset2D::default(), extent: target.extent };
            self.device.cmd_begin_render_pass(self.commands, &vk::RenderPassBeginInfo::default().render_pass(target.pass)
                .framebuffer(target.framebuffers[index as usize]).render_area(whole).clear_values(&clear), vk::SubpassContents::INLINE);
            self.device.cmd_bind_pipeline(self.commands, vk::PipelineBindPoint::GRAPHICS, target.pipeline);
            let (x, y, width, height) = fit(size, (target.extent.width, target.extent.height));
            self.device.cmd_set_viewport(self.commands, 0,
                &[vk::Viewport { x, y, width, height, min_depth: 0.0, max_depth: 1.0 }]);
            self.device.cmd_set_scissor(self.commands, 0, &[whole]);
            self.device.cmd_bind_descriptor_sets(self.commands, vk::PipelineBindPoint::GRAPHICS, self.layout, 0, &[self.set], &[]);
            self.device.cmd_push_constants(self.commands, self.layout, vk::ShaderStageFlags::FRAGMENT, 0,
                std::slice::from_raw_parts(matrix.as_ptr().cast::<u8>(), 64));
            self.device.cmd_draw(self.commands, 3, 1, 0, 0);
            if let (Some(layer), Some(_)) = (&layer, &self.sheet) {
                let pq = [f32::from(u8::from(target.pq))];
                self.device.cmd_bind_pipeline(self.commands, vk::PipelineBindPoint::GRAPHICS, target.overlay_pipeline);
                self.device.cmd_set_viewport(self.commands, 0, &[vk::Viewport { x: layer.x as f32, y: layer.y as f32,
                    width: layer.width as f32, height: layer.height as f32, min_depth: 0.0, max_depth: 1.0 }]);
                self.device.cmd_bind_descriptor_sets(self.commands, vk::PipelineBindPoint::GRAPHICS, self.layout, 0,
                    &[self.overlay_set], &[]);
                self.device.cmd_push_constants(self.commands, self.layout, vk::ShaderStageFlags::FRAGMENT, 0,
                    std::slice::from_raw_parts(pq.as_ptr().cast::<u8>(), 4));
                self.device.cmd_draw(self.commands, 3, 1, 0, 0);
            }
            self.device.cmd_end_render_pass(self.commands);
            self.device.end_command_buffer(self.commands).map_err(failed("end the frame"))?;

            let (waits, stages, buffers, signals) =
                ([self.acquired], [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT], [self.commands], [target.drawn[index as usize]]);
            self.device.queue_submit(self.queue, &[vk::SubmitInfo::default().wait_semaphores(&waits)
                .wait_dst_stage_mask(&stages).command_buffers(&buffers).signal_semaphores(&signals)], self.fence)
                .map_err(failed("draw the frame"))?;
            let (swapchains, indices) = ([target.swapchain], [index]);
            match self.swapchains.queue_present(self.queue, &vk::PresentInfoKHR::default().wait_semaphores(&signals)
                .swapchains(&swapchains).image_indices(&indices)) {
                Ok(false) => {}
                // Remade before the next frame.
                Ok(true) | Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => { self.drop_target(false); }
                Err(error) => return Err(failed("show the frame")(error)),
            }
        }
        Ok(())
    }
}

const COLOUR: vk::ImageSubresourceRange = vk::ImageSubresourceRange {
    aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: 1,
};

impl Drop for Presenter {
    fn drop(&mut self) {
        unsafe {
            self.drop_target(false);
            if let Some(old) = self.picture.take() { self.drop_textures(old.planes, old.staging); }
            if let Some(old) = self.sheet.take() { self.drop_textures([old.plane], old.staging); }
            self.device.destroy_shader_module(self.vertex, None);
            self.device.destroy_shader_module(self.fragment, None);
            self.device.destroy_shader_module(self.overlay_fragment, None);
            self.device.destroy_descriptor_pool(self.descriptors, None);
            self.device.destroy_pipeline_layout(self.layout, None);
            self.device.destroy_descriptor_set_layout(self.set_layout, None);
            self.device.destroy_sampler(self.sampler, None);
            self.device.destroy_semaphore(self.acquired, None);
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.pool, None);
            self.device.destroy_device(None);
            self.surfaces.destroy_surface(self.surface, None);
            self.instance.destroy_instance(None);
            (self.xlib.XCloseDisplay)(self.display);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the shader computes for the 10-bit codes of one pixel.
    fn rgb(matrix: [[f32; 4]; 4], codes: [u32; 3]) -> [f32; 3] {
        let sampled = codes.map(|code| (code * 64) as f32 / 65535.0);
        [0, 1, 2].map(|row| matrix[0][row] * sampled[0] + matrix[1][row] * sampled[1] + matrix[2][row] * sampled[2] + matrix[3][row])
    }

    fn close(left: [f32; 3], right: [f32; 3]) -> bool {
        left.iter().zip(right).all(|(left, right)| (left - right).abs() < 0.004)
    }

    #[test]
    fn the_matrix_takes_video_range_codes_to_rgb() {
        let matrix = colour_matrix(BT2020, false);
        assert!(close(rgb(matrix, [64, 512, 512]), [0.0, 0.0, 0.0]));
        assert!(close(rgb(matrix, [940, 512, 512]), [1.0, 1.0, 1.0]));
        // Pure Rec. 2020 red: luma 0.2627, Cb -0.1396, Cr 0.5.
        assert!(close(rgb(matrix, [294, 387, 960]), [1.0, 0.0, 0.0]));
        // Rec. 709 puts the same red at other codes.
        assert!(close(rgb(colour_matrix(BT709, false), [250, 409, 960]), [1.0, 0.0, 0.0]));
        assert!(close(rgb(colour_matrix(BT709, true), [1023, 512, 512]), [1.0, 1.0, 1.0]));
    }

    #[test]
    fn rows_are_copied_without_the_padding_between_them() {
        // Two rows of three bytes, four apart: a padded download from the decoder.
        let padded = [1, 2, 3, 0, 4, 5, 6, 0];
        let mut packed = [9u8; 6];
        unsafe { copy_rows(&padded, 4, 3, 2, packed.as_mut_ptr()) }.unwrap();
        assert_eq!(packed, [1, 2, 3, 4, 5, 6]);
        // A third row is not there.
        assert!(unsafe { copy_rows(&padded, 4, 3, 3, [0u8; 9].as_mut_ptr()) }.is_err());
    }

    #[test]
    fn the_picture_keeps_its_shape_in_the_window() {
        assert_eq!(fit((1280, 800), (1280, 800)), (0.0, 0.0, 1280.0, 800.0));
        // A 16:9 picture in the Deck's 16:10 window: bars above and below.
        assert_eq!(fit((1920, 1080), (1280, 800)), (0.0, 40.0, 1280.0, 720.0));
        // The window above Steam's keyboard is half as large in the same shape.
        assert_eq!(fit((1280, 800), (640, 400)), (0.0, 0.0, 640.0, 400.0));
        assert_eq!(fit((1280, 800), (1000, 400)), (180.0, 0.0, 640.0, 400.0));
    }

    #[test]
    fn hdr_needs_the_hdr10_colour_space_and_sdr_falls_back_to_eight_bits() {
        let offer = |format, color_space| vk::SurfaceFormatKHR { format, color_space };
        let sdr = offer(vk::Format::B8G8R8A8_UNORM, vk::ColorSpaceKHR::SRGB_NONLINEAR);
        let deep = offer(vk::Format::A2B10G10R10_UNORM_PACK32, vk::ColorSpaceKHR::SRGB_NONLINEAR);
        let hdr = offer(vk::Format::A2R10G10B10_UNORM_PACK32, vk::ColorSpaceKHR::HDR10_ST2084_EXT);
        let chosen = |offered: &[vk::SurfaceFormatKHR], pq| surface_format(offered, pq).map(|format| (format.format, format.color_space));
        assert_eq!(chosen(&[sdr, deep, hdr], true), Some((hdr.format, hdr.color_space)));
        assert_eq!(chosen(&[sdr, deep, hdr], false), Some((deep.format, deep.color_space)));
        assert_eq!(chosen(&[sdr], false), Some((sdr.format, sdr.color_space)));
        // An SDR display: no HDR, and never HDR pixels in an SDR colour space.
        assert_eq!(chosen(&[sdr, deep], true), None);
    }
}
