use webxr_api::Discovery;
use webxr_api::Error;
use webxr_api::Session;
use webxr_api::SessionBuilder;
use webxr_api::SessionMode;

use super::device::GoogleVRDevice;

#[cfg(target_os = "android")]
use crate::jni_utils::JNIScope;
#[cfg(target_os = "android")]
use android_injected_glue::ffi as ndk;
use gvr_sys as gvr;
use std::ptr;

#[cfg(target_os = "android")]
const SERVICE_CLASS_NAME: &'static str = "com/rust/webvr/GVRService";

pub struct GoogleVRDiscovery {
    ctx: *mut gvr::gvr_context,
    controller_ctx: *mut gvr::gvr_controller_context,
    #[cfg(target_os = "android")]
    pub java_object: ndk::jobject,
    #[cfg(target_os = "android")]
    pub java_class: ndk::jclass,
}

impl GoogleVRDiscovery {
    pub fn new() -> Result<Self, Error> {
        let mut this = Self::new_uninit();
        unsafe {
            this.create_context().map_err(Error::BackendSpecific)?;
        }
        if this.ctx.is_null() {
            return Err(Error::BackendSpecific(
                "GoogleVR SDK failed to initialize".into(),
            ));
        }
        unsafe {
            this.create_controller_context();
        }
        Ok(this)
    }
}

impl Discovery for GoogleVRDiscovery {
    fn request_session(&mut self, mode: SessionMode, xr: SessionBuilder) -> Result<Session, Error> {
        if self.supports_session(mode) {
            xr.spawn(move || GoogleVRDevice::new())
        } else {
            Err(Error::NoMatchingDevice)
        }
    }

    fn supports_session(&self, mode: SessionMode) -> bool {
        mode == SessionMode::ImmersiveVR
    }
}

impl GoogleVRDiscovery {
    #[cfg(target_os = "android")]
    pub fn new_uninit() -> Self {
        Self {
            ctx: ptr::null_mut(),
            controller_ctx: ptr::null_mut(),
            java_object: ptr::null_mut(),
            java_class: ptr::null_mut(),
        }
    }

    #[cfg(not(target_os = "android"))]
    pub fn new_uninit() -> Self {
        Self {
            ctx: ptr::null_mut(),
            controller_ctx: ptr::null_mut(),
        }
    }

    // On Android, the gvr_context must be be obtained from
    // the Java GvrLayout object via GvrLayout.getGvrApi().getNativeGvrContext()
    // Java code is implemented in GVRService. It handles the life cycle of the GvrLayout.
    // JNI code is used to comunicate with that Java code.
    #[cfg(target_os = "android")]
    unsafe fn create_context(&mut self) -> Result<(), String> {
        use std::mem;

        let jni_scope = JNIScope::attach()?;

        let jni = jni_scope.jni();
        let env = jni_scope.env;

        // Use NativeActivity's classloader to find our class
        self.java_class = jni_scope.find_class(SERVICE_CLASS_NAME)?;
        if self.java_class.is_null() {
            return Err("Didn't find GVRService class".into());
        };
        self.java_class = (jni.NewGlobalRef)(env, self.java_class);

        // Create GVRService instance and own it as a globalRef.
        let method = jni_scope.get_method(
            self.java_class,
            "create",
            "(Landroid/app/Activity;J)Ljava/lang/Object;",
            true,
        );
        let thiz: usize = mem::transmute(self as *mut GoogleVRDiscovery);
        self.java_object = (jni.CallStaticObjectMethod)(
            env,
            self.java_class,
            method,
            jni_scope.activity,
            thiz as ndk::jlong,
        );
        if self.java_object.is_null() {
            return Err("Failed to create GVRService instance".into());
        };
        self.java_object = (jni.NewGlobalRef)(env, self.java_object);

        // Finally we have everything required to get the gvr_context pointer from java :)
        let method = jni_scope.get_method(self.java_class, "getNativeContext", "()J", false);
        let pointer = (jni.CallLongMethod)(env, self.java_object, method);
        self.ctx = pointer as *mut gvr::gvr_context;
        if self.ctx.is_null() {
            return Err("Failed to getNativeGvrContext from java GvrLayout".into());
        }

        Ok(())
    }

    #[cfg(not(target_os = "android"))]
    unsafe fn create_context(&mut self) -> Result<(), String> {
        self.ctx = gvr::gvr_create();
        Ok(())
    }

    unsafe fn create_controller_context(&mut self) {
        let options = gvr::gvr_controller_get_default_options();
        self.controller_ctx = gvr::gvr_controller_create_and_init(options, self.ctx);
        gvr::gvr_controller_resume(self.controller_ctx);
    }

    pub fn on_pause(&self) {
        unimplemented!()
    }

    pub fn on_resume(&self) {
        unimplemented!()
    }
}

#[cfg(target_os = "android")]
#[no_mangle]
#[allow(non_snake_case)]
#[allow(dead_code)]
pub extern "C" fn Java_com_rust_webvr_GVRService_nativeOnPause(
    _: *mut ndk::JNIEnv,
    service: ndk::jlong,
) {
    use std::mem;
    unsafe {
        let service: *mut GoogleVRDiscovery = mem::transmute(service as usize);
        (*service).on_pause();
    }
}

#[cfg(target_os = "android")]
#[no_mangle]
#[allow(non_snake_case)]
#[allow(dead_code)]
pub extern "C" fn Java_com_rust_webvr_GVRService_nativeOnResume(
    _: *mut ndk::JNIEnv,
    service: ndk::jlong,
) {
    use std::mem;
    unsafe {
        let service: *mut GoogleVRDiscovery = mem::transmute(service as usize);
        (*service).on_resume();
    }
}
