use crate::utils::ClipPlanes;
use euclid::Point2D;
use euclid::Rect;
use euclid::RigidTransform3D;
use euclid::Rotation3D;
use euclid::Size2D;
use euclid::Transform3D;
use euclid::Vector3D;
use gleam::gl::{self, GLenum, GLuint, Gl};
use log::warn;
use openxr::d3d::{Requirements, SessionCreateInfo, D3D11};
use openxr::sys::platform::ID3D11Device;
use openxr::{
    self, ActionSet, ActiveActionSet, ApplicationInfo, CompositionLayerFlags,
    CompositionLayerProjection, Entry, EnvironmentBlendMode, ExtensionSet, Extent2Di, FormFactor,
    Fovf, FrameState, FrameStream, FrameWaiter, Graphics, Instance, Posef, Quaternionf,
    ReferenceSpaceType, Session, Space, Swapchain, SwapchainCreateFlags, SwapchainCreateInfo,
    SwapchainUsageFlags, Vector3f, ViewConfigurationType, ViewConfigurationView,
};
use std::rc::Rc;
use std::{mem, ptr};
use surfman::Surface;
use webxr_api;
use webxr_api::Device;
use webxr_api::Discovery;
use webxr_api::Error;
use webxr_api::Event;
use webxr_api::EventBuffer;
use webxr_api::Floor;
use webxr_api::Frame;
use webxr_api::FrameUpdateEvent;
use webxr_api::Handedness;
use webxr_api::InputId;
use webxr_api::InputSource;
use webxr_api::Native;
use webxr_api::Quitter;
use webxr_api::Sender;
use webxr_api::Session as WebXrSession;
use webxr_api::SessionBuilder;
use webxr_api::SessionMode;
use webxr_api::TargetRayMode;
use webxr_api::View;
use webxr_api::Viewer;
use webxr_api::Views;
use winapi::shared::dxgi;
use winapi::shared::dxgiformat;
use winapi::shared::dxgitype;
use winapi::shared::winerror::{DXGI_ERROR_NOT_FOUND, S_OK};
use winapi::um::d3d11;
use winapi::um::d3dcommon::*;
use winapi::um::handleapi::INVALID_HANDLE_VALUE;
use winapi::Interface;
use wio::com::ComPtr;

mod input;
use input::OpenXRInput;

const HEIGHT: f32 = 1.0;

pub struct OpenXrDiscovery {
    gl: Rc<dyn Gl>,
}

impl OpenXrDiscovery {
    pub fn new(gl: Rc<dyn Gl>) -> Self {
        Self { gl }
    }
}

fn create_instance() -> Result<Instance, String> {
    let entry = Entry::load().map_err(|e| format!("{:?}", e))?;
    let app_info = ApplicationInfo {
        application_name: "webvr",
        application_version: 1,

        ..Default::default()
    };

    let exts = ExtensionSet {
        khr_d3d11_enable: true,
        ..Default::default()
    };

    entry
        .create_instance(&app_info, &exts)
        .map_err(|e| format!("{:?}", e))
}

pub fn pick_format(formats: &[dxgiformat::DXGI_FORMAT]) -> (dxgiformat::DXGI_FORMAT, GLenum) {
    warn!("Available formats: {:?}", formats);
    for format in formats {
        match *format {
            dxgiformat::DXGI_FORMAT_B8G8R8A8_UNORM => return (*format, gl::BGRA),
            //dxgiformat::DXGI_FORMAT_R8G8B8A8_UNORM => return (*format, gl::RGBA),
            f => {
                warn!("Backend requested unsupported format {:?}", f);
            }
        }
    }

    panic!("No formats supported amongst {:?}", formats);
}

impl Discovery for OpenXrDiscovery {
    fn request_session(
        &mut self,
        mode: SessionMode,
        xr: SessionBuilder,
    ) -> Result<WebXrSession, Error> {
        let instance = create_instance().map_err(|e| Error::BackendSpecific(e))?;
        if self.supports_session(mode) {
            let gl = self.gl.clone();
            xr.run_on_main_thread(move || OpenXrDevice::new(gl, instance))
        } else {
            Err(Error::NoMatchingDevice)
        }
    }

    fn supports_session(&self, mode: SessionMode) -> bool {
        mode == SessionMode::ImmersiveAR || mode == SessionMode::ImmersiveVR
    }
}

struct OpenXrDevice {
    instance: Instance,
    gl: Rc<dyn Gl>,
    read_fbo: GLuint,
    write_fbo: GLuint,
    events: EventBuffer,
    session: Session<D3D11>,
    frame_waiter: FrameWaiter,
    frame_stream: FrameStream<D3D11>,
    frame_state: FrameState,
    space: Space,
    clip_planes: ClipPlanes,
    openxr_views: Vec<openxr::View>,
    view_configurations: Vec<openxr::ViewConfigurationView>,
    format: dxgiformat::DXGI_FORMAT,
    gl_format: GLenum,
    left_extent: Extent2Di,
    right_extent: Extent2Di,
    left_swapchain: Swapchain<D3D11>,
    left_image: u32,
    right_swapchain: Swapchain<D3D11>,
    right_image: u32,
    device_context: ComPtr<d3d11::ID3D11DeviceContext>,
    device: ComPtr<d3d11::ID3D11Device>,
    surfman: Option<(surfman::Device, surfman::Context)>,

    // input
    action_set: ActionSet,
    right_hand: OpenXRInput,
    left_hand: OpenXRInput,
}

impl OpenXrDevice {
    fn new(gl: Rc<dyn Gl>, instance: Instance) -> Result<OpenXrDevice, Error> {
        let read_fbo = gl.gen_framebuffers(1)[0];
        debug_assert_eq!(gl.get_error(), gl::NO_ERROR);

        let write_fbo = gl.gen_framebuffers(1)[0];
        debug_assert_eq!(gl.get_error(), gl::NO_ERROR);

        let system = instance
            .system(FormFactor::HEAD_MOUNTED_DISPLAY)
            .map_err(|e| Error::BackendSpecific(format!("{:?}", e)))?;

        let requirements = D3D11::requirements(&instance, system)
            .map_err(|e| Error::BackendSpecific(format!("{:?}", e)))?;
        let adapter = get_matching_adapter(&requirements).map_err(|e| Error::BackendSpecific(e))?;
        let feature_levels = select_feature_levels(&requirements);
        let (device, device_context) = init_device_for_adapter(adapter, &feature_levels)
            .map_err(|e| Error::BackendSpecific(format!("{:?}", e)))?;

        let (session, mut frame_waiter, frame_stream) = unsafe {
            instance
                .create_session::<D3D11>(
                    system,
                    &SessionCreateInfo {
                        device: device.as_raw(),
                    },
                )
                .map_err(|e| Error::BackendSpecific(format!("{:?}", e)))?
        };

        // XXXPaul initialisation should happen on SessionStateChanged(Ready)?

        session
            .begin(ViewConfigurationType::PRIMARY_STEREO)
            .map_err(|e| Error::BackendSpecific(format!("{:?}", e)))?;

        let ref_space_type = ReferenceSpaceType::LOCAL;
        let pose = Posef {
            orientation: Quaternionf {
                x: 0.,
                y: 0.,
                z: 0.,
                w: 1.,
            },
            position: Vector3f {
                x: 0.,
                y: 0.,
                z: 0.,
            },
        };
        let space = session
            .create_reference_space(ref_space_type, pose)
            .map_err(|e| Error::BackendSpecific(format!("{:?}", e)))?;

        let view_configurations = instance
            .enumerate_view_configuration_views(system, ViewConfigurationType::PRIMARY_STEREO)
            .map_err(|e| Error::BackendSpecific(format!("{:?}", e)))?;

        let left_view_configuration = view_configurations[0];
        let right_view_configuration = view_configurations[1];
        let left_extent = Extent2Di {
            width: left_view_configuration.recommended_image_rect_width as i32,
            height: left_view_configuration.recommended_image_rect_height as i32,
        };
        let right_extent = Extent2Di {
            width: right_view_configuration.recommended_image_rect_width as i32,
            height: right_view_configuration.recommended_image_rect_height as i32,
        };

        // Obtain view info
        let frame_state = frame_waiter.wait().expect("error waiting for frame");
        let (_view_flags, views) = session
            .locate_views(
                ViewConfigurationType::PRIMARY_STEREO,
                frame_state.predicted_display_time,
                &space,
            )
            .expect("error locating views");

        // Create swapchains

        // XXXManishearth should we be doing this, or letting Servo set the format?
        let formats = session
            .enumerate_swapchain_formats()
            .map_err(|e| Error::BackendSpecific(format!("{:?}", e)))?;
        let (format, gl_format) = pick_format(&formats);
        let swapchain_create_info = SwapchainCreateInfo {
            create_flags: SwapchainCreateFlags::EMPTY,
            usage_flags: SwapchainUsageFlags::COLOR_ATTACHMENT | SwapchainUsageFlags::SAMPLED,
            format,
            sample_count: 1,
            // XXXManishearth what if the recommended widths are different?
            width: left_view_configuration.recommended_image_rect_width,
            height: left_view_configuration.recommended_image_rect_height,
            face_count: 1,
            array_size: 1,
            mip_count: 1,
        };

        let left_swapchain = session
            .create_swapchain(&swapchain_create_info)
            .map_err(|e| Error::BackendSpecific(format!("{:?}", e)))?;
        let right_swapchain = session
            .create_swapchain(&swapchain_create_info)
            .map_err(|e| Error::BackendSpecific(format!("{:?}", e)))?;

        /*let (texture, resource) = create_texture(
            &left_view_configuration,
            &right_view_configuration,
            &device,
            format,
        );*/

        let surfman = unsafe { surfman::Device::from_current_context().ok() };

        // input

        let action_set = instance.create_action_set("hands", "Hands", 0).unwrap();
        let right_hand = OpenXRInput::new(InputId(0), Handedness::Right, &action_set);
        let left_hand = OpenXRInput::new(InputId(1), Handedness::Left, &action_set);
        let mut bindings = right_hand.get_bindings(&instance);
        bindings.extend(left_hand.get_bindings(&instance).into_iter());
        let path_controller = instance
            .string_to_path("/interaction_profiles/khr/simple_controller")
            .unwrap();
        instance
            .suggest_interaction_profile_bindings(path_controller, &bindings)
            .unwrap();
        session.attach_action_sets(&[&action_set]).unwrap();

        Ok(OpenXrDevice {
            instance,
            events: Default::default(),
            gl,
            read_fbo,
            write_fbo,
            session,
            frame_stream,
            frame_waiter,
            frame_state,
            space,
            clip_planes: Default::default(),
            left_extent,
            right_extent,
            left_image: 0,
            right_image: 0,
            openxr_views: views,
            view_configurations,
            format,
            gl_format,
            left_swapchain,
            right_swapchain,
            /*texture,
            resource,*/
            device_context,
            device,
            surfman,

            action_set,
            right_hand,
            left_hand,
        })
    }

    fn handle_openxr_events(&mut self) -> bool {
        use openxr::Event::*;
        loop {
            let mut buffer = openxr::EventDataBuffer::new();
            let event = self.instance.poll_event(&mut buffer).unwrap();
            match event {
                Some(SessionStateChanged(session_change)) => match session_change.state() {
                    openxr::SessionState::STOPPING => {
                        self.events.callback(Event::SessionEnd);
                        self.session.end().unwrap();
                        return false;
                    }
                    openxr::SessionState::EXITING | openxr::SessionState::LOSS_PENDING => {
                        break;
                    }
                    _ => {
                        // FIXME: Handle other states
                    }
                },
                Some(InstanceLossPending(_)) => {
                    break;
                }
                Some(_) => {
                    // FIXME: Handle other events
                }
                None => {
                    // No more events to process
                    break;
                }
            }
        }
        true
    }
}

impl Device for OpenXrDevice {
    fn floor_transform(&self) -> RigidTransform3D<f32, Native, Floor> {
        let translation = Vector3D::new(-HEIGHT, 0.0, 0.0);
        RigidTransform3D::from_translation(translation)
    }

    fn views(&self) -> Views {
        let left_view_configuration = &self.view_configurations[0];
        let right_view_configuration = &self.view_configurations[1];
        let views = &self.openxr_views;

        let lerped = lerp_transforms(&views[0].pose, &views[1].pose);
        let left_vp = Rect::new(
            Point2D::zero(),
            Size2D::new(
                left_view_configuration.recommended_image_rect_width as i32,
                left_view_configuration.recommended_image_rect_height as i32,
            ),
        );
        let right_vp = Rect::new(
            Point2D::new(
                left_view_configuration.recommended_image_rect_width as i32,
                0,
            ),
            Size2D::new(
                right_view_configuration.recommended_image_rect_width as i32,
                right_view_configuration.recommended_image_rect_height as i32,
            ),
        );
        let left_view = View {
            transform: transform(&views[0].pose).inverse().pre_transform(&lerped),
            projection: fov_to_projection_matrix(&views[0].fov, self.clip_planes),
            viewport: left_vp,
        };
        let right_view = View {
            transform: transform(&views[1].pose).inverse().pre_transform(&lerped),
            projection: fov_to_projection_matrix(&views[1].fov, self.clip_planes),
            viewport: right_vp,
        };

        Views::Stereo(left_view, right_view)
    }

    fn wait_for_animation_frame(&mut self) -> Option<Frame> {
        if !self.handle_openxr_events() {
            // Session is not running anymore.
            return None;
        }
        self.frame_state = self.frame_waiter.wait().expect("error waiting for frame");
        // XXXManishearth should we check frame_state.should_render?
        let (_view_flags, views) = self
            .session
            .locate_views(
                ViewConfigurationType::PRIMARY_STEREO,
                self.frame_state.predicted_display_time,
                &self.space,
            )
            .expect("error locating views");
        self.openxr_views = views;

        let transform = lerp_transforms(&self.openxr_views[0].pose, &self.openxr_views[1].pose);

        let events = if self.clip_planes.recently_updated() {
            vec![FrameUpdateEvent::UpdateViews(self.views())]
        } else {
            vec![]
        };

        let active_action_set = ActiveActionSet::new(&self.action_set);

        self.session.sync_actions(&[active_action_set]).unwrap();

        let (right_input_frame, right_select) =
            self.right_hand
                .frame(&self.session, &self.frame_state, &self.space);
        let (left_input_frame, left_select) =
            self.left_hand
                .frame(&self.session, &self.frame_state, &self.space);

        let frame = Frame {
            transform,
            inputs: vec![right_input_frame, left_input_frame],
            events,
        };

        if let Some(right_select) = right_select {
            self.events
                .callback(Event::Select(InputId(0), right_select, frame.clone()));
        }
        if let Some(left_select) = left_select {
            self.events
                .callback(Event::Select(InputId(1), left_select, frame.clone()));
        }

        // todo use pose in input
        Some(frame)
    }

    fn render_animation_frame(&mut self, surface: Surface) -> Surface {
        let (mut device, mut context) = self.surfman.take().unwrap();
        let size = surface.size();

        let surface_texture = device
            .create_surface_texture(&mut context, surface)
            .unwrap();
        let texture_id = surface_texture.gl_texture();
        
        let flipped_surface = device
            .create_surface(&context, &surfman::SurfaceType::Generic { size })
            .expect("no create surface??");
        let share_handle = flipped_surface.share_handle().expect("no share handle??");
        let flipped_surface_texture = device
            .create_surface_texture(&mut context, flipped_surface)
            .expect("no create surface texture??");
        let flipped_texture_id = flipped_surface_texture.gl_texture();

        let mut value = [0];
        unsafe {
            self.gl.get_integer_v(gl::FRAMEBUFFER_BINDING, &mut value);
        }
        let old_framebuffer = value[0] as gl::GLuint;

        self.gl.bind_framebuffer(gl::READ_FRAMEBUFFER, self.read_fbo);

        self.gl.framebuffer_texture_2d(
            gl::READ_FRAMEBUFFER,
            gl::COLOR_ATTACHMENT0,
            device.surface_gl_texture_target(),
            texture_id,
            0,
        );

        self.gl.bind_framebuffer(gl::DRAW_FRAMEBUFFER, self.write_fbo);

        self.gl.framebuffer_texture_2d(
            gl::DRAW_FRAMEBUFFER,
            gl::COLOR_ATTACHMENT0,
            device.surface_gl_texture_target(),
            flipped_texture_id,
            0,
        );

        // Blit the appropriate rectangle from the source texture to the d3d texture.
        self.gl.blit_framebuffer(
            0, 0, size.width, size.height,
            0, size.height, size.width, 0,
            gl::COLOR_BUFFER_BIT, gl::NEAREST,
        );
        assert_eq!(self.gl.get_error(), gl::NO_ERROR);

        self.gl.flush();

        // Restore old GL bindings.
        self.gl.bind_framebuffer(gl::FRAMEBUFFER, old_framebuffer);

        // XXXManishearth this code should perhaps be in wait_for_animation_frame,
        // but we then get errors that wait_image was called without a release_image()
        self.frame_stream
            .begin()
            .expect("failed to start frame stream");

        let mut tmpResource = ptr::null_mut();
        unsafe {
            let hr = self.device.OpenSharedResource(share_handle, &d3d11::ID3D11Resource::uuidof(), &mut tmpResource);
            assert_eq!(hr, S_OK);
            assert!(!tmpResource.is_null());
        }
        let tmpResource = unsafe { ComPtr::from_raw(tmpResource as *mut d3d11::ID3D11Resource) };
        let texture = tmpResource.cast::<d3d11::ID3D11Texture2D>().expect("not a d3d texture???");

        let mut desc: d3d11::D3D11_TEXTURE2D_DESC = unsafe { mem::zeroed() };
        unsafe { texture.GetDesc(&mut desc) };
        println!("have a d3d texture: {:?}", desc.Format);

        let texture = texture.up::<d3d11::ID3D11Resource>();

        self.left_image = self.left_swapchain.acquire_image().unwrap();
        self.left_swapchain
            .wait_image(openxr::Duration::INFINITE)
            .unwrap();
        self.right_image = self.right_swapchain.acquire_image().unwrap();
        self.right_swapchain
            .wait_image(openxr::Duration::INFINITE)
            .unwrap();

        let left_swapchain_images = self.left_swapchain.enumerate_images().unwrap();
        let left_image = left_swapchain_images[self.left_image as usize];
        let right_swapchain_images = self.right_swapchain.enumerate_images().unwrap();
        let right_image = right_swapchain_images[self.right_image as usize];

        let mut desc: d3d11::D3D11_TEXTURE2D_DESC = unsafe { mem::zeroed() };
        let left_image_tmp = unsafe { ComPtr::from_raw(left_image) };
        unsafe { left_image_tmp.GetDesc(&mut desc); }
        mem::forget(left_image_tmp);
        println!("openxr d3d texture: {:?}", desc.Format);


        let mut b = d3d11::D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: (size.width / 2) as u32,
            bottom: size.height as u32,
            back: 1,
        };
        unsafe {
            // from_raw adopts instead of retaining, so we need to manually addref
            // alternatively we can just forget after the CopySubresourceRegion call,
            // since these images are guaranteed to live at least as long as the frame
            let left_resource = ComPtr::from_raw(left_image).up::<d3d11::ID3D11Resource>();
            mem::forget(left_resource.clone());
            let right_resource = ComPtr::from_raw(right_image).up::<d3d11::ID3D11Resource>();
            mem::forget(right_resource.clone());
            self.device_context.CopySubresourceRegion(
                left_resource.as_raw(),
                0,
                0,
                0,
                0,
                texture.as_raw(),
                0,
                &b,
            );
            b = d3d11::D3D11_BOX {
                left: (size.width / 2) as u32,
                top: 0,
                front: 0,
                right: size.width as u32,
                bottom: size.height as u32,
                back: 1,
            };
            self.device_context.CopySubresourceRegion(
                right_resource.as_raw(),
                0,
                0,
                0,
                0,
                texture.as_raw(),
                0,
                &b,
            );
        }
        
        mem::forget(texture);

        self.left_swapchain.release_image().unwrap();
        self.right_swapchain.release_image().unwrap();
        self.frame_stream
            .end(
                self.frame_state.predicted_display_time,
                EnvironmentBlendMode::ADDITIVE,
                &[&CompositionLayerProjection::new()
                    .space(&self.space)
                    .layer_flags(CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA)
                    .views(&[
                        openxr::CompositionLayerProjectionView::new()
                            .pose(self.openxr_views[0].pose)
                            .fov(self.openxr_views[0].fov)
                            .sub_image(
                                // XXXManishearth is this correct?
                                openxr::SwapchainSubImage::new()
                                    .swapchain(&self.left_swapchain)
                                    .image_array_index(0)
                                    .image_rect(openxr::Rect2Di {
                                        offset: openxr::Offset2Di { x: 0, y: 0 },
                                        extent: self.left_extent,
                                    }),
                            ),
                        openxr::CompositionLayerProjectionView::new()
                            .pose(self.openxr_views[0].pose)
                            .fov(self.openxr_views[0].fov)
                            .sub_image(
                                openxr::SwapchainSubImage::new()
                                    .swapchain(&self.right_swapchain)
                                    .image_array_index(0)
                                    .image_rect(openxr::Rect2Di {
                                        offset: openxr::Offset2Di { x: 0, y: 0 },
                                        extent: self.right_extent,
                                    }),
                            ),
                    ])],
            )
            .unwrap();
        let flipped_surface = device.destroy_surface_texture(&mut context, flipped_surface_texture).unwrap();
        device.destroy_surface(&mut context, flipped_surface).unwrap();
        let surface = device
            .destroy_surface_texture(&mut context, surface_texture)
            .unwrap();
        self.surfman = Some((device, context));
        surface
    }

    fn initial_inputs(&self) -> Vec<InputSource> {
        vec![
            InputSource {
                handedness: Handedness::Right,
                id: InputId(0),
                target_ray_mode: TargetRayMode::TrackedPointer,
                supports_grip: true,
            },
            InputSource {
                handedness: Handedness::Left,
                id: InputId(1),
                target_ray_mode: TargetRayMode::TrackedPointer,
                supports_grip: true,
            },
        ]
    }

    fn set_event_dest(&mut self, dest: Sender<Event>) {
        self.events.upgrade(dest)
    }

    fn quit(&mut self) {
        self.session.request_exit().unwrap();
    }

    fn set_quitter(&mut self, _: Quitter) {
        // Glwindow currently doesn't have any way to end its own session
        // XXXManishearth add something for this that listens for the window
        // being closed
    }

    fn update_clip_planes(&mut self, near: f32, far: f32) {
        self.clip_planes.update(near, far);
    }

    fn environment_blend_mode(&self) -> webxr_api::EnvironmentBlendMode {
        webxr_api::EnvironmentBlendMode::Additive
    }
}

impl Drop for OpenXrDevice {
    fn drop(&mut self) {
        if let Some((ref device, ref mut context)) = self.surfman {
            let _ = device.destroy_context(context);
        }
    }
}

fn get_matching_adapter(
    requirements: &Requirements,
) -> Result<ComPtr<dxgi::IDXGIAdapter1>, String> {
    unsafe {
        let mut factory_ptr: *mut dxgi::IDXGIFactory1 = ptr::null_mut();
        let result = dxgi::CreateDXGIFactory1(
            &dxgi::IDXGIFactory1::uuidof(),
            &mut factory_ptr as *mut _ as *mut _,
        );
        assert_eq!(result, S_OK);
        let factory = ComPtr::from_raw(factory_ptr);

        let index = 0;
        loop {
            let mut adapter_ptr = ptr::null_mut();
            let result = factory.EnumAdapters1(index, &mut adapter_ptr);
            if result == DXGI_ERROR_NOT_FOUND {
                return Err("No matching adapter".to_owned());
            }
            assert_eq!(result, S_OK);
            let adapter = ComPtr::from_raw(adapter_ptr);
            let mut adapter_desc = mem::zeroed();
            let result = adapter.GetDesc1(&mut adapter_desc);
            assert_eq!(result, S_OK);
            let adapter_luid = &adapter_desc.AdapterLuid;
            if adapter_luid.LowPart == requirements.adapter_luid.LowPart
                && adapter_luid.HighPart == requirements.adapter_luid.HighPart
            {
                return Ok(adapter);
            }
        }
    }
}

fn select_feature_levels(requirements: &Requirements) -> Vec<D3D_FEATURE_LEVEL> {
    let levels = [
        D3D_FEATURE_LEVEL_12_1,
        D3D_FEATURE_LEVEL_12_0,
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
    ];
    levels
        .into_iter()
        .filter(|&&level| level >= requirements.min_feature_level)
        .map(|&level| level)
        .collect()
}

fn init_device_for_adapter(
    adapter: ComPtr<dxgi::IDXGIAdapter1>,
    feature_levels: &[D3D_FEATURE_LEVEL],
) -> Result<(ComPtr<ID3D11Device>, ComPtr<d3d11::ID3D11DeviceContext>), String> {
    let adapter = adapter.up::<dxgi::IDXGIAdapter>();
    unsafe {
        let mut device_ptr = ptr::null_mut();
        let mut device_context_ptr = ptr::null_mut();
        let hr = d3d11::D3D11CreateDevice(
            adapter.as_raw(),
            D3D_DRIVER_TYPE_UNKNOWN,
            ptr::null_mut(),
            // add d3d11::D3D11_CREATE_DEVICE_DEBUG below for debug output
            d3d11::D3D11_CREATE_DEVICE_BGRA_SUPPORT | d3d11::D3D11_CREATE_DEVICE_DEBUG,
            feature_levels.as_ptr(),
            feature_levels.len() as u32,
            d3d11::D3D11_SDK_VERSION,
            &mut device_ptr,
            ptr::null_mut(),
            &mut device_context_ptr,
        );
        assert_eq!(hr, S_OK);
        let device = ComPtr::from_raw(device_ptr);
        let device_context = ComPtr::from_raw(device_context_ptr);
        Ok((device, device_context))
    }
}

/*fn create_texture(
    left_view_configuration: &ViewConfigurationView,
    right_view_configuration: &ViewConfigurationView,
    device: &ComPtr<ID3D11Device>,
    format: dxgiformat::DXGI_FORMAT,
) -> (ComPtr<d3d11::ID3D11Texture2D>, ComPtr<dxgi::IDXGIResource>) {
    let width = left_view_configuration.recommended_image_rect_width
        + right_view_configuration.recommended_image_rect_width;
    let height = left_view_configuration.recommended_image_rect_height
        + right_view_configuration.recommended_image_rect_height;
    let texture_desc = d3d11::D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        Format: format,
        MipLevels: 1,
        ArraySize: 1,
        SampleDesc: dxgitype::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: d3d11::D3D11_USAGE_DEFAULT,
        BindFlags: d3d11::D3D11_BIND_RENDER_TARGET | d3d11::D3D11_BIND_SHADER_RESOURCE,
        CPUAccessFlags: 0,
        MiscFlags: d3d11::D3D11_RESOURCE_MISC_SHARED,
    };
    let mut d3dtex_ptr = ptr::null_mut();
    // XXXManishearth we should be able to handle other formats
    let mut data = vec![0u8; width as usize * height as usize * mem::size_of::<u32>()];
    for pixels in data.chunks_mut(mem::size_of::<u32>()) {
        pixels[0] = 255;
        pixels[3] = 255;
    }

    let init_data = d3d11::D3D11_SUBRESOURCE_DATA {
        pSysMem: data.as_ptr() as *const _,
        SysMemPitch: width * mem::size_of::<u32>() as u32,
        SysMemSlicePitch: width * height * mem::size_of::<u32>() as u32,
    };

    unsafe {
        let hr = device.CreateTexture2D(&texture_desc, &init_data, &mut d3dtex_ptr);
        assert_eq!(hr, S_OK);
        let d3dtex = ComPtr::from_raw(d3dtex_ptr);
        let dxgi_resource = d3dtex
            .cast::<dxgi::IDXGIResource>()
            .expect("not a dxgi resource");
        (d3dtex, dxgi_resource)
    }
}*/

fn transform<Eye>(pose: &Posef) -> RigidTransform3D<f32, Eye, Native> {
    let rotation = Rotation3D::quaternion(
        pose.orientation.x,
        pose.orientation.y,
        pose.orientation.z,
        pose.orientation.w,
    );

    let translation = Vector3D::new(pose.position.x, pose.position.y, pose.position.z);

    RigidTransform3D::new(rotation, translation)
}

// Approximate the viewer transform by linear interpolation of the two eyes
fn lerp_transforms(left: &Posef, right: &Posef) -> RigidTransform3D<f32, Viewer, Native> {
    let left = transform(left);
    let right = transform(right);

    let rotation = left.rotation.lerp(&right.rotation, 0.5);
    let translation = left.translation.lerp(right.translation, 0.5);

    RigidTransform3D::new(rotation, translation)
}

#[inline]
fn fov_to_projection_matrix<T, U>(fov: &Fovf, clip_planes: ClipPlanes) -> Transform3D<f32, T, U> {
    let near = clip_planes.near;
    let far = clip_planes.far;
    // XXXManishearth deal with infinite planes
    let left = fov.angle_left.tan() * near;
    let right = fov.angle_right.tan() * near;
    let top = fov.angle_up.tan() * near;
    let bottom = fov.angle_down.tan() * near;

    let w = right - left;
    let h = top - bottom;
    let d = far - near;

    Transform3D::column_major(
        2. * near / w,
        0.,
        (right + left) / w,
        0.,
        0.,
        2. * near / h,
        (top + bottom) / h,
        0.,
        0.,
        0.,
        -(far + near) / d,
        -2. * far * near / d,
        0.,
        0.,
        -1.,
        0.,
    )
}
