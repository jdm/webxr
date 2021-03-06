/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use webxr_api::Device;
use webxr_api::Discovery;
use webxr_api::Error;
use webxr_api::Event;
use webxr_api::EventBuffer;
use webxr_api::Floor;
use webxr_api::Frame;
use webxr_api::Input;
use webxr_api::InputFrame;
use webxr_api::InputSource;
use webxr_api::MockDeviceInit;
use webxr_api::MockDeviceMsg;
use webxr_api::MockDiscovery;
use webxr_api::MockInputMsg;
use webxr_api::Native;
use webxr_api::Quitter;
use webxr_api::Receiver;
use webxr_api::Sender;
use webxr_api::Session;
use webxr_api::SessionBuilder;
use webxr_api::SessionMode;
use webxr_api::Viewer;
use webxr_api::Views;

use euclid::default::Size2D;
use euclid::RigidTransform3D;

use gleam::gl;
use gleam::gl::GLsync;
use gleam::gl::GLuint;
use gleam::gl::Gl;

use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;

pub struct HeadlessMockDiscovery {
    gl: Rc<dyn Gl>,
}

struct HeadlessDiscovery {
    gl: Rc<dyn Gl>,
    data: Arc<Mutex<HeadlessDeviceData>>,
    supports_immersive: bool,
}

struct InputInfo {
    source: InputSource,
    active: bool,
    pointer: RigidTransform3D<f32, Input, Native>,
}

struct HeadlessDevice {
    gl: Rc<dyn Gl>,
    data: Arc<Mutex<HeadlessDeviceData>>,
}

struct HeadlessDeviceData {
    floor_transform: RigidTransform3D<f32, Native, Floor>,
    viewer_origin: RigidTransform3D<f32, Viewer, Native>,
    views: Views,
    inputs: Vec<InputInfo>,
    events: EventBuffer,
    quitter: Option<Quitter>,
    disconnected: bool,
}

impl MockDiscovery for HeadlessMockDiscovery {
    fn simulate_device_connection(
        &mut self,
        init: MockDeviceInit,
        receiver: Receiver<MockDeviceMsg>,
    ) -> Result<Box<dyn Discovery>, Error> {
        let viewer_origin = init.viewer_origin.clone();
        let floor_transform = init.floor_origin.inverse();
        let views = init.views.clone();
        let data = HeadlessDeviceData {
            floor_transform,
            viewer_origin,
            views,
            inputs: vec![],
            events: Default::default(),
            quitter: None,
            disconnected: false,
        };
        let data = Arc::new(Mutex::new(data));
        let data_ = data.clone();

        thread::spawn(move || {
            run_loop(receiver, data_);
        });
        Ok(Box::new(HeadlessDiscovery {
            gl: self.gl.clone(),
            data,
            supports_immersive: init.supports_immersive,
        }))
    }
}

fn run_loop(receiver: Receiver<MockDeviceMsg>, data: Arc<Mutex<HeadlessDeviceData>>) {
    while let Ok(msg) = receiver.recv() {
        if !data.lock().expect("Mutex poisoned").handle_msg(msg) {
            break;
        }
    }
}

impl Discovery for HeadlessDiscovery {
    fn request_session(&mut self, mode: SessionMode, xr: SessionBuilder) -> Result<Session, Error> {
        if self.data.lock().unwrap().disconnected || !self.supports_session(mode) {
            return Err(Error::NoMatchingDevice);
        }
        let gl = self.gl.clone();
        let data = self.data.clone();
        xr.run_on_main_thread(move || Ok(HeadlessDevice { gl, data }))
    }

    fn supports_session(&self, mode: SessionMode) -> bool {
        mode == SessionMode::Inline || self.supports_immersive
    }
}

impl Device for HeadlessDevice {
    fn floor_transform(&self) -> RigidTransform3D<f32, Native, Floor> {
        self.data.lock().unwrap().floor_transform.clone()
    }

    fn views(&self) -> Views {
        self.data.lock().unwrap().views.clone()
    }

    fn wait_for_animation_frame(&mut self) -> Frame {
        let data = self.data.lock().unwrap();
        let transform = data.viewer_origin;
        let inputs = data
            .inputs
            .iter()
            .filter(|i| i.active)
            .map(|i| InputFrame {
                id: i.source.id,
                target_ray_origin: i.pointer,
            })
            .collect();
        Frame { transform, inputs }
    }

    fn render_animation_frame(&mut self, _: GLuint, _: Size2D<i32>, sync: Option<GLsync>) {
        if let Some(sync) = sync {
            self.gl.wait_sync(sync, 0, gl::TIMEOUT_IGNORED);
            debug_assert_eq!(self.gl.get_error(), gl::NO_ERROR);
        }
    }

    fn initial_inputs(&self) -> Vec<InputSource> {
        vec![]
    }

    fn set_event_dest(&mut self, dest: Sender<Event>) {
        self.data.lock().unwrap().events.upgrade(dest)
    }

    fn quit(&mut self) {
        self.data.lock().unwrap().events.callback(Event::SessionEnd);
    }

    fn set_quitter(&mut self, quitter: Quitter) {
        self.data.lock().unwrap().quitter = Some(quitter);
    }
}

impl HeadlessMockDiscovery {
    pub fn new(gl: Rc<dyn Gl>) -> HeadlessMockDiscovery {
        HeadlessMockDiscovery { gl }
    }
}

impl HeadlessDeviceData {
    fn handle_msg(&mut self, msg: MockDeviceMsg) -> bool {
        match msg {
            MockDeviceMsg::SetViewerOrigin(viewer_origin) => {
                self.viewer_origin = viewer_origin;
            }
            MockDeviceMsg::SetViews(views) => {
                self.views = views;
            }
            MockDeviceMsg::Focus => {
                // TODO
            }
            MockDeviceMsg::Blur => {
                // TODO
            }
            MockDeviceMsg::AddInputSource(init) => {
                self.inputs.push(InputInfo {
                    source: init.source,
                    pointer: init.pointer_origin,
                    active: true,
                });
                self.events.callback(Event::AddInput(init.source))
            }
            MockDeviceMsg::MessageInputSource(id, msg) => {
                if let Some(ref mut input) = self.inputs.iter_mut().find(|i| i.source.id == id) {
                    match msg {
                        MockInputMsg::SetHandedness(h) => input.source.handedness = h,
                        MockInputMsg::SetTargetRayMode(t) => input.source.target_ray_mode = t,
                        MockInputMsg::SetPointerOrigin(p) => input.pointer = p,
                        MockInputMsg::Disconnect => input.active = false,
                        MockInputMsg::Reconnect => input.active = true,
                    }
                }
            }
            MockDeviceMsg::Disconnect(s) => {
                self.disconnected = true;
                self.quitter.as_ref().map(|q| q.quit());
                // notify the client that we're done disconnecting
                let _ = s.send(());
                return false;
            }
        }
        true
    }
}
