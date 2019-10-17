// surfman/src/gl_utils.rs
//
//! Various OpenGL utilities used by the different backends.

use crate::gl::types::GLuint;
use crate::gl::{self, Gl};

pub(crate) fn destroy_framebuffer(gl: &Gl, framebuffer_object: GLuint) {
    unsafe {
        // Unbind the framebuffer if currently bound.
        let (mut current_draw_framebuffer, mut current_read_framebuffer) = (0, 0);
        gl.GetIntegerv(gl::DRAW_FRAMEBUFFER_BINDING, &mut current_draw_framebuffer);
        gl.GetIntegerv(gl::READ_FRAMEBUFFER_BINDING, &mut current_read_framebuffer);
        if current_draw_framebuffer as GLuint == framebuffer_object {
            gl.BindFramebuffer(gl::DRAW_FRAMEBUFFER, 0);
        }
        if current_read_framebuffer as GLuint == framebuffer_object {
            gl.BindFramebuffer(gl::READ_FRAMEBUFFER, 0);
        }

        gl.DeleteFramebuffers(1, &framebuffer_object);
    }
}