/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use crate::DeviceAPI;
use crate::Error;
use crate::Event;
use crate::Floor;
use crate::Frame;
use crate::FrameUpdateEvent;
use crate::InputSource;
use crate::Native;
use crate::Receiver;
use crate::Sender;
use crate::SwapChainId;
use crate::Viewport;
use crate::Views;

use euclid::RigidTransform3D;
use euclid::Size2D;

use log::warn;

use std::thread;
use std::time::Duration;

use surfman_chains_api::SwapChainAPI;
use surfman_chains_api::SwapChainsAPI;

#[cfg(feature = "ipc")]
use serde::{Deserialize, Serialize};

// How long to wait for an rAF.
static TIMEOUT: Duration = Duration::from_millis(5);

trait Foo {
    fn to_ms(&self) -> f64;
}

impl Foo for u64 {
    fn to_ms(&self) -> f64 {
        *self as f64 / 1000000.
    }
}

/// https://www.w3.org/TR/webxr/#xrsessionmode-enum
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "ipc", derive(Serialize, Deserialize))]
pub enum SessionMode {
    Inline,
    ImmersiveVR,
    ImmersiveAR,
}

/// https://immersive-web.github.io/webxr-ar-module/#xrenvironmentblendmode-enum
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "ipc", derive(Serialize, Deserialize))]
pub enum EnvironmentBlendMode {
    Opaque,
    AlphaBlend,
    Additive,
}

// The messages that are sent from the content thread to the session thread.
#[cfg_attr(feature = "ipc", derive(Serialize, Deserialize))]
enum SessionMsg {
    SetSwapChain(Option<SwapChainId>),
    SetEventDest(Sender<Event>),
    UpdateClipPlanes(/* near */ f32, /* far */ f32),
    StartRenderLoop,
    RenderAnimationFrame(u64),
    Quit,
}

#[cfg_attr(feature = "ipc", derive(Serialize, Deserialize))]
#[derive(Clone)]
pub struct Quitter {
    sender: Sender<SessionMsg>,
}

impl Quitter {
    pub fn quit(&self) {
        let _ = self.sender.send(SessionMsg::Quit);
    }
}

/// An object that represents an XR session.
/// This is owned by the content thread.
/// https://www.w3.org/TR/webxr/#xrsession-interface
#[cfg_attr(feature = "ipc", derive(Serialize, Deserialize))]
pub struct Session {
    floor_transform: Option<RigidTransform3D<f32, Native, Floor>>,
    views: Views,
    resolution: Option<Size2D<i32, Viewport>>,
    sender: Sender<SessionMsg>,
    environment_blend_mode: EnvironmentBlendMode,
    initial_inputs: Vec<InputSource>,
}

impl Session {
    pub fn floor_transform(&self) -> Option<RigidTransform3D<f32, Native, Floor>> {
        self.floor_transform.clone()
    }

    pub fn initial_inputs(&self) -> &[InputSource] {
        &self.initial_inputs
    }

    pub fn views(&self) -> Views {
        self.views.clone()
    }

    pub fn environment_blend_mode(&self) -> EnvironmentBlendMode {
        self.environment_blend_mode
    }

    pub fn recommended_framebuffer_resolution(&self) -> Size2D<i32, Viewport> {
        self.resolution
            .expect("Inline XR sessions should not construct a framebuffer")
    }

    pub fn set_swap_chain(&mut self, swap_chain_id: Option<SwapChainId>) {
        let _ = self.sender.send(SessionMsg::SetSwapChain(swap_chain_id));
    }

    pub fn start_render_loop(&mut self) {
        let _ = self.sender.send(SessionMsg::StartRenderLoop);
    }

    pub fn update_clip_planes(&mut self, near: f32, far: f32) {
        let _ = self.sender.send(SessionMsg::UpdateClipPlanes(near, far));
    }

    pub fn set_event_dest(&mut self, dest: Sender<Event>) {
        let _ = self.sender.send(SessionMsg::SetEventDest(dest));
    }

    pub fn render_animation_frame(&mut self) {
        let _ = self.sender.send(SessionMsg::RenderAnimationFrame(time::precise_time_ns()));
    }

    pub fn end_session(&mut self) {
        let _ = self.sender.send(SessionMsg::Quit);
    }

    pub fn apply_event(&mut self, event: FrameUpdateEvent) {
        match event {
            FrameUpdateEvent::UpdateViews(views) => self.views = views,
            FrameUpdateEvent::UpdateFloorTransform(floor) => self.floor_transform = floor,
        }
    }
}

/// For devices that want to do their own thread management, the `SessionThread` type is exposed.
pub struct SessionThread<Device, SwapChains: SwapChainsAPI<SwapChainId>> {
    receiver: Receiver<SessionMsg>,
    sender: Sender<SessionMsg>,
    swap_chain: Option<SwapChains::SwapChain>,
    swap_chains: SwapChains,
    frame_count: u64,
    frame_sender: Sender<Frame>,
    running: bool,
    device: Device,
}

impl<Device, SwapChains> SessionThread<Device, SwapChains>
where
    Device: DeviceAPI<SwapChains::Surface>,
    SwapChains: SwapChainsAPI<SwapChainId>,
{
    pub fn new(
        mut device: Device,
        swap_chains: SwapChains,
        frame_sender: Sender<Frame>,
    ) -> Result<Self, Error> {
        let (sender, receiver) = crate::channel().or(Err(Error::CommunicationError))?;
        device.set_quitter(Quitter {
            sender: sender.clone(),
        });
        let frame_count = 0;
        let swap_chain = None;
        let running = true;
        Ok(SessionThread {
            sender,
            receiver,
            device,
            swap_chain,
            swap_chains,
            frame_count,
            frame_sender,
            running,
        })
    }

    pub fn new_session(&mut self) -> Session {
        let floor_transform = self.device.floor_transform();
        let views = self.device.views();
        let resolution = self.device.recommended_framebuffer_resolution();
        let sender = self.sender.clone();
        let initial_inputs = self.device.initial_inputs();
        let environment_blend_mode = self.device.environment_blend_mode();
        Session {
            floor_transform,
            views,
            resolution,
            sender,
            initial_inputs,
            environment_blend_mode,
        }
    }

    pub fn run(&mut self) {
        loop {
            if let Ok(msg) = self.receiver.recv() {
                if !self.handle_msg(msg) {
                    self.running = false;
                    break;
                }
            } else {
                break;
            }
        }
    }

    fn handle_msg(&mut self, msg: SessionMsg) -> bool {
        match msg {
            SessionMsg::SetSwapChain(swap_chain_id) => {
                self.swap_chain = swap_chain_id.and_then(|id| self.swap_chains.get(id));
            }
            SessionMsg::SetEventDest(dest) => {
                self.device.set_event_dest(dest);
            }
            SessionMsg::StartRenderLoop => {
                let frame = match self.device.wait_for_animation_frame() {
                    Some(frame) => frame,
                    None => {
                        warn!("Device stopped providing frames, exiting");
                        return false;
                    }
                };

                let _ = self.frame_sender.send(frame);
            }
            SessionMsg::UpdateClipPlanes(near, far) => self.device.update_clip_planes(near, far),
            SessionMsg::RenderAnimationFrame(sent_time) => {
                self.frame_count += 1;
                let mut render_start = None;
                if let Some(ref swap_chain) = self.swap_chain {
                    if let Some(surface) = swap_chain.take_surface() {
                        //println!("!!! raf render {}", Instant::now());
                        render_start = Some(time::precise_time_ns());
                        println!("!!! raf transmitted {}ms", (render_start.unwrap() - sent_time).to_ms());
                        let surface = self.device.render_animation_frame(surface);
                        swap_chain.recycle_surface(surface);
                    }
                }
                let wait_start = time::precise_time_ns();
                if let Some(render_start) = render_start {
                    println!("!!! raf render {}", (wait_start - render_start).to_ms());
                }
                //println!("!!! raf wait {}", wait_start);
                let mut frame = match self.device.wait_for_animation_frame() {
                    Some(frame) => frame,
                    None => {
                        warn!("Device stopped providing frames, exiting");
                        return false;
                    }
                };
                let wait_end = time::precise_time_ns();
                println!("!!! raf wait {}", (wait_end - wait_start).to_ms());
                //println!("!!! raf trigger {:?}", );
                frame.sent_time = wait_end;
                let _ = self.frame_sender.send(frame);
            }
            SessionMsg::Quit => {
                self.device.quit();
                return false;
            }
        }
        true
    }
}

/// Devices that need to can run sessions on the main thread.
pub trait MainThreadSession: 'static {
    fn run_one_frame(&mut self);
    fn running(&self) -> bool;
}

impl<Device, SwapChains> MainThreadSession for SessionThread<Device, SwapChains>
where
    Device: DeviceAPI<SwapChains::Surface>,
    SwapChains: SwapChainsAPI<SwapChainId>,
{
    fn run_one_frame(&mut self) {
        let frame_count = self.frame_count;
        let start_run = time::precise_time_ns();
        while frame_count == self.frame_count && self.running {
            if let Ok(msg) = crate::recv_timeout(&self.receiver, TIMEOUT) {
            //if let Ok(msg) = self.receiver.try_recv() {
                self.running = self.handle_msg(msg);
            } else {
                break;
            }
        }
        let end_run = time::precise_time_ns();
        println!("!!! run_one_frame {}ms", (end_run - start_run).to_ms());
    }

    fn running(&self) -> bool {
        self.running
    }
}

/// A type for building XR sessions
pub struct SessionBuilder<'a, SwapChains: 'a> {
    swap_chains: &'a SwapChains,
    sessions: &'a mut Vec<Box<dyn MainThreadSession>>,
    frame_sender: Sender<Frame>,
}

impl<'a, SwapChains> SessionBuilder<'a, SwapChains>
where
    SwapChains: SwapChainsAPI<SwapChainId>,
{
    pub(crate) fn new(
        swap_chains: &'a SwapChains,
        sessions: &'a mut Vec<Box<dyn MainThreadSession>>,
        frame_sender: Sender<Frame>,
    ) -> Self {
        SessionBuilder {
            swap_chains,
            sessions,
            frame_sender,
        }
    }

    /// For devices which are happy to hand over thread management to webxr.
    pub fn spawn<Device, Factory>(self, factory: Factory) -> Result<Session, Error>
    where
        Factory: 'static + FnOnce() -> Result<Device, Error> + Send,
        Device: DeviceAPI<SwapChains::Surface>,
    {
        let (acks, ackr) = crate::channel().or(Err(Error::CommunicationError))?;
        let swap_chains = self.swap_chains.clone();
        let frame_sender = self.frame_sender.clone();
        thread::spawn(move || {
            match factory().and_then(|device| SessionThread::new(device, swap_chains, frame_sender))
            {
                Ok(mut thread) => {
                    let session = thread.new_session();
                    let _ = acks.send(Ok(session));
                    thread.run();
                }
                Err(err) => {
                    let _ = acks.send(Err(err));
                }
            }
        });
        ackr.recv().unwrap_or(Err(Error::CommunicationError))
    }

    /// For devices that need to run on the main thread.
    pub fn run_on_main_thread<Device, Factory>(self, factory: Factory) -> Result<Session, Error>
    where
        Factory: 'static + FnOnce() -> Result<Device, Error>,
        Device: DeviceAPI<SwapChains::Surface>,
    {
        let device = factory()?;
        let swap_chains = self.swap_chains.clone();
        let frame_sender = self.frame_sender.clone();
        let mut session_thread = SessionThread::new(device, swap_chains, frame_sender)?;
        let session = session_thread.new_session();
        self.sessions.push(Box::new(session_thread));
        Ok(session)
    }
}
