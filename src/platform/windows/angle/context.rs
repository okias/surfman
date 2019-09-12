//! Wrapper for EGL contexts managed by ANGLE using Direct3D 11 as a backend on Windows.

use crate::egl::types::{EGLAttrib, EGLConfig, EGLContext, EGLDeviceEXT, EGLDisplay};
use crate::egl::types::{EGLenum, EGLint};
use crate::{ContextAttributeFlags, ContextAttributes, Error, GLApi, GLFlavor, GLInfo};
use crate::{GLVersion, egl};
use super::adapter::Adapter;
use super::device::{Device, EGL_D3D11_DEVICE_ANGLE, EGL_EXTENSION_FUNCTIONS};
use super::device::{EGL_NO_DEVICE_EXT, OwnedEGLDisplay};
use super::error::ToWindowingApiError;
use super::surface::{ColorSurface, Surface, SurfaceTexture};

use gl;
use gl::types::GLuint;
use std::ffi::CString;
use std::mem;
use std::os::raw::c_void;
use std::ptr;
use std::str::FromStr;
use std::sync::Mutex;
use std::thread;
use winapi::um::d3d11::ID3D11Device;
use winapi::um::d3dcommon::D3D_DRIVER_TYPE_UNKNOWN;
use wio::com::ComPtr;

const EGL_DEVICE_EXT: EGLenum = 0x322c;

lazy_static! {
    static ref CREATE_CONTEXT_MUTEX: Mutex<bool> = Mutex::new(false);
}

pub struct Context {
    pub(crate) native_context: Box<dyn NativeContext>,
    gl_info: GLInfo,
    color_surface: ColorSurface,
}

pub(crate) trait NativeContext {
    fn egl_context(&self) -> EGLContext;
    fn is_destroyed(&self) -> bool;
    unsafe fn destroy(&mut self, device: &Device);
}

impl Drop for Context {
    #[inline]
    fn drop(&mut self) {
        if !self.native_context.is_destroyed() && !thread::panicking() {
            panic!("Contexts must be destroyed explicitly with `destroy_context`!")
        }
    }
}

#[derive(Clone)]
pub struct ContextDescriptor {
    egl_config_id: EGLint,
    egl_context_client_version: EGLint,
}

impl Device {
    pub fn create_context_descriptor(&self, attributes: &ContextAttributes)
                                     -> Result<ContextDescriptor, Error> {
        let renderable_type = match attributes.flavor.api {
            GLApi::GL => egl::OPENGL_BIT,
            GLApi::GLES => egl::OPENGL_ES2_BIT,
        };

        let flags = attributes.flags;
        let alpha_size   = if flags.contains(ContextAttributeFlags::ALPHA)   { 8  } else { 0 };
        let depth_size   = if flags.contains(ContextAttributeFlags::DEPTH)   { 24 } else { 0 };
        let stencil_size = if flags.contains(ContextAttributeFlags::STENCIL) { 8  } else { 0 };

        unsafe {
            // Create config attributes.
            let config_attributes = [
                egl::SURFACE_TYPE as EGLint,         egl::PBUFFER_BIT as EGLint,
                egl::RENDERABLE_TYPE as EGLint,      renderable_type as EGLint,
                egl::BIND_TO_TEXTURE_RGBA as EGLint, 1 as EGLint,
                egl::RED_SIZE as EGLint,             8,
                egl::GREEN_SIZE as EGLint,           8,
                egl::BLUE_SIZE as EGLint,            8,
                egl::ALPHA_SIZE as EGLint,           alpha_size,
                egl::DEPTH_SIZE as EGLint,           depth_size,
                egl::STENCIL_SIZE as EGLint,         stencil_size,
                egl::NONE as EGLint,                 0,
                0,                                   0,
            ];

            // Pick a config.
            let (mut config, mut config_count) = (ptr::null(), 0);
            let result = egl::ChooseConfig(self.native_display.egl_display(),
                                           config_attributes.as_ptr(),
                                           &mut config,
                                           1,
                                           &mut config_count);
            if result == egl::FALSE {
                let err = egl::GetError().to_windowing_api_error();
                return Err(Error::PixelFormatSelectionFailed(err));
            }
            if config_count == 0 || config.is_null() {
                return Err(Error::NoPixelFormatFound);
            }

            // Get the config ID.
            let mut egl_config_id = 0;
            let result = egl::GetConfigAttrib(display, config, attr, &mut egl_config_id);
            debug_assert_ne!(result, egl::FALSE);

            Ok(ContextDescriptor { egl_config_id })
        }
    }

    /// Opens the device and context corresponding to the current EGL context.
    ///
    /// The native context is not retained, as there is no way to do this in the EGL API. It is the
    /// caller's responsibility to keep it alive for the duration of this context. Be careful when
    /// using this method; it's essentially a last resort.
    ///
    /// This method is designed to allow `surfman` to deal with contexts created outside the
    /// library; for example, by Glutin. It's legal to use this method to wrap a context rendering
    /// to any target: either a window or a pbuffer. The target is opaque to `surfman`; the library
    /// will not modify or try to detect the render target. This means that any of the methods that
    /// query or replace the surface—e.g. `replace_context_color_surface`—will fail if called with
    /// a context object created via this method.
    pub unsafe fn from_current_context() -> Result<(Device, Context), Error> {
        let mut previous_context_created = CREATE_CONTEXT_MUTEX.lock().unwrap();

        // Grab the current EGL display and EGL context.
        let egl_display = egl::GetCurrentDisplay();
        debug_assert_ne!(egl_display, egl::NO_DISPLAY);
        let egl_context = egl::GetCurrentContext();
        debug_assert_ne!(egl_context, egl::NO_CONTEXT);
        let native_context = Box::new(UnsafeEGLContextRef { egl_context });

        println!("Device::from_current_context() = {:x}", egl_context as usize);

        // Fetch the EGL device.
        let mut egl_device = EGL_NO_DEVICE_EXT;
        let result = (EGL_EXTENSION_FUNCTIONS.QueryDisplayAttribEXT)(
            egl_display,
            EGL_DEVICE_EXT as EGLint,
            &mut egl_device as *mut EGLDeviceEXT as *mut EGLAttrib);
        assert_ne!(result, egl::FALSE);
        debug_assert_ne!(egl_device, EGL_NO_DEVICE_EXT);

        // Fetch the D3D11 device.
        let mut d3d11_device: *mut ID3D11Device = ptr::null_mut();
        let result = (EGL_EXTENSION_FUNCTIONS.QueryDeviceAttribEXT)(
            egl_device,
            EGL_D3D11_DEVICE_ANGLE,
            &mut d3d11_device as *mut *mut ID3D11Device as *mut EGLAttrib);
        assert_ne!(result, egl::FALSE);
        assert!(!d3d11_device.is_null());
        let d3d11_device = ComPtr::from_raw(d3d11_device);

        // Create the device wrapper.
        // FIXME(pcwalton): Using `D3D_DRIVER_TYPE_UNKNOWN` is unfortunate. Perhaps we should
        // detect the "Microsoft Basic" string and switch to `D3D_DRIVER_TYPE_WARP` as appropriate.
        let device = Device {
            native_display: Box::new(OwnedEGLDisplay { egl_display }),
            egl_device,
            surface_bindings: vec![],
            d3d11_device,
            d3d_driver_type: D3D_DRIVER_TYPE_UNKNOWN,
        };

        // Create the config.
        let mut context = Context {
            native_context,
            gl_info: GLInfo::new(),
            color_surface: ColorSurface::External,
        };

        if !*previous_context_created {
            gl::load_with(|symbol| {
                device.get_proc_address(&mut context, symbol).unwrap_or(ptr::null())
            });
            *previous_context_created = true;
        }

        let context_descriptor = device.context_descriptor(&context);
        let context_attributes = device.context_descriptor_attributes(&context_descriptor);
        context.gl_info.populate(&context_attributes);

        Ok((device, context))
    }

    pub fn create_context(&self, color_surface: Surface) -> Result<Context, Error> {
        let mut previous_context_created = CREATE_CONTEXT_MUTEX.lock().unwrap();

        let egl_config = self.context_descriptor_to_egl_config(&color_surface.descriptor);
        let egl_context_client_version = color_surface.descriptor.flavor.version.major as EGLint;

        unsafe {
            // Include some extra zeroes to work around broken implementations.
            let egl_context_attributes = [
                egl::CONTEXT_CLIENT_VERSION as EGLint, egl_context_client_version,
                egl::NONE as EGLint, 0,
                0, 0,
            ];

            let mut egl_context = egl::CreateContext(self.native_display.egl_display(),
                                                     config,
                                                     egl::NO_CONTEXT,
                                                     egl_context_attributes.as_ptr());
            if egl_context == egl::NO_CONTEXT {
                let err = egl::GetError().to_windowing_api_error();
                return Err(Error::ContextCreationFailed(err));
            }

            let mut context = Context {
                native_context: Box::new(OwnedEGLContext { egl_context }),
                color_surface: ColorSurface::Managed(color_surface),
                gl_info: GLInfo::new(attributes),
            };

            if !*previous_context_created {
                gl::load_with(|symbol| {
                    self.get_proc_address(&mut context, symbol).unwrap_or(ptr::null())
                });
                *previous_context_created = true;
            }

            let context_descriptor = device.context_descriptor(&context);
            let context_attributes = device.context_descriptor_attributes(&context_descriptor);
            context.gl_info.populate(&context_attributes);

            self.make_context_current(&context)?;
            Ok(context)
        }
    }

    pub fn destroy_context(&self, context: &mut Context) -> Result<(), Error> {
        if context.native_context.is_destroyed() {
            return Ok(());
        }

        if let ColorSurface::Managed(color_surface) = mem::replace(&mut context.color_surface,
                                                                   ColorSurface::None) {
            self.destroy_surface(color_surface);
        }

        context.native_context.destroy(self);
        Ok(())
    }

    pub fn context_descriptor(&self, context: &Context) -> ContextDescriptor {
        unsafe {
            // Get the EGL config ID.
            let mut egl_config_id = 0;
            let result = egl::QueryContext(egl_display,
                                           egl_context,
                                           egl::CONFIG_ID as EGLint,
                                           &mut egl_config_id);
            assert_ne!(result, egl::FALSE);

            // Get the GL version.
            let mut egl_context_client_version = 0;
            let result = egl::QueryContext(egl_display,
                                           egl_context,
                                           egl::CONTEXT_CLIENT_VERSION as EGLint,
                                           &mut egl_context_client_version);
            assert_ne!(result, egl::FALSE);
            debug_assert!(egl_context_client_version > 0);
            println!("client version = {}", egl_context_client_version);

            ContextDescriptor { egl_config_id, egl_context_client_version }
        }
    }

    #[inline]
    pub fn context_gl_info<'c>(&self, context: &'c Context) -> &'c GLInfo {
        &context.gl_info
    }

    pub fn make_context_current(&self, context: &Context) -> Result<(), Error> {
        unsafe {
            let color_egl_surface = match context.color_surface {
                ColorSurface::Managed(ref color_surface) => self.lookup_surface(color_surface),
                ColorSurface::None | ColorSurface::External => egl::NO_SURFACE,
            };
            let result = egl::MakeCurrent(self.native_display.egl_display(),
                                          color_egl_surface,
                                          color_egl_surface,
                                          context.native_context.egl_context());
            if result == egl::FALSE {
                let err = egl::GetError().to_windowing_api_error();
                return Err(Error::MakeCurrentFailed(err));
            }
            Ok(())
        }
    }

    pub fn make_context_not_current(&self, _: &Context) -> Result<(), Error> {
        unsafe {
            let result = egl::MakeCurrent(self.native_display.egl_display(),
                                          egl::NO_SURFACE,
                                          egl::NO_SURFACE,
                                          egl::NO_CONTEXT);
            if result == egl::FALSE {
                let err = egl::GetError().to_windowing_api_error();
                return Err(Error::MakeCurrentFailed(err));
            }
            Ok(())
        }
    }

    pub fn get_proc_address(&self, _: &Context, symbol_name: &str)
                            -> Result<*const c_void, Error> {
        unsafe {
            let symbol_name: CString = CString::new(symbol_name).unwrap();
            let fun_ptr = egl::GetProcAddress(symbol_name.as_ptr());
            if fun_ptr.is_null() {
                return Err(Error::GLFunctionNotFound);
            }
            
            return Ok(fun_ptr as *const c_void);
        }
    }

    #[inline]
    pub fn context_color_surface<'c>(&self, context: &'c Context) -> Option<&'c Surface> {
        match context.color_surface {
            ColorSurface::None | ColorSurface::External => None,
            ColorSurface::Managed(ref surface) => Some(surface),
        }
    }

    pub fn replace_context_color_surface(&self, context: &mut Context, new_color_surface: Surface)
                                         -> Result<Option<Surface>, Error> {
        if let ColorSurface::External = context.color_surface {
            return Err(Error::ExternalRenderTarget)
        }

        let context_descriptor = self.context_descriptor(context);
        if new_color_surface.descriptor.egl_config_id != context_descriptor.egl_config_id ||
                new_color_surface.descriptor.egl_context_client_version !=
                context_descriptor.egl_context_client_version {
            return Err(Error::IncompatibleContextDescriptor);
        }

        let old_surface = match mem::replace(&mut context.color_surface,
                                             ColorSurface::Managed(new_color_surface)) {
            ColorSurface::None => None,
            ColorSurface::Managed(old_surface) => Some(old_surface),
        };

        self.make_context_current(context)?;

        Ok(old_surface)
    }

    #[inline]
    pub fn context_surface_framebuffer_object(&self, context: &Context) -> Result<GLuint, Error> {
        Ok(0)
    }

    pub fn context_descriptor_attributes(&self, context_descriptor: &ContextDescriptor)
                                         -> ContextAttributes {
        let egl_display = self.native_display.egl_display();
        let egl_config = self.context_descriptor_to_egl_config(context_descriptor);

        unsafe {
            let alpha_size = get_config_attr(egl_display, egl_config, egl::ALPHA_SIZE as EGLint);
            let depth_size = get_config_attr(egl_display, egl_config, egl::DEPTH_SIZE as EGLint);
            let stencil_size = get_config_attr(egl_display,
                                               egl_config,
                                               egl::STENCIL_SIZE as EGLint);

            // Convert to `surfman` context attribute flags.
            let mut attribute_flags = ContextAttributeFlags::empty();
            attribute_flags.set(ContextAttributeFlags::ALPHA, alpha_size != 0);
            attribute_flags.set(ContextAttributeFlags::DEPTH, depth_size != 0);
            attribute_flags.set(ContextAttributeFlags::STENCIL, stencil_size != 0);

            // Create appropriate context attributes.
            ContextAttributes { flags: attribute_flags, flavor: context_descriptor.flavor }
        }
    }

    fn context_descriptor_to_egl_config(&self, context_descriptor: &ContextDescriptor) -> EGLint {
        unsafe {
            let config_attributes = [
                egl::CONFIG_ID as EGLint,   context_descriptor.egl_config_id,
                egl::NONE as EGLint,        0,
                0,                          0,
            ];

            let (mut config, mut config_count) = (ptr::null(), 0);
            let result = egl::ChooseConfig(self.native_display.egl_display(),
                                           config_attributes.as_ptr(),
                                           &mut config,
                                           1,
                                           &mut config_count);
            assert_ne!(result, egl::FALSE);
            assert!(config_count > 0);
            config
        }
    }
}

struct OwnedEGLContext {
    egl_context: EGLContext,
}

impl NativeContext for OwnedEGLContext {
    #[inline]
    fn egl_context(&self) -> EGLContext {
        self.egl_context
    }

    #[inline]
    fn is_destroyed(&self) -> bool {
        self.egl_context == egl::NO_CONTEXT
    }

    unsafe fn destroy(&mut self, device: &Device) {
        assert!(!self.is_destroyed());
        egl::MakeCurrent(device.native_display.egl_display(),
                         egl::NO_SURFACE,
                         egl::NO_SURFACE,
                         egl::NO_CONTEXT);
        let result = egl::DestroyContext(device.native_display.egl_display(), self.egl_context);
        assert_ne!(result, egl::FALSE);
        self.egl_context = egl::NO_CONTEXT;
    }
}

struct UnsafeEGLContextRef {
    egl_context: EGLContext,
}

impl NativeContext for UnsafeEGLContextRef {
    #[inline]
    fn egl_context(&self) -> EGLContext {
        self.egl_context
    }

    #[inline]
    fn is_destroyed(&self) -> bool {
        self.egl_context == egl::NO_CONTEXT
    }

    unsafe fn destroy(&mut self, device: &Device) {
        assert!(!self.is_destroyed());
        self.egl_context = egl::NO_CONTEXT;
    }
}