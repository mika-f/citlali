//! Hand-written bindings for the handful of libvips calls Citlali needs.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::marker::PhantomData;
use std::ptr::{self, NonNull};
use std::sync::OnceLock;

#[repr(C)]
struct VipsImage {
    _private: [u8; 0],
}

// Variadic libvips calls take `name, value, ..., NULL` option lists.
unsafe extern "C" {
    fn vips_init(argv0: *const c_char) -> c_int;
    fn vips_cache_set_max(max: c_int);
    fn vips_block_untrusted_set(state: c_int);
    fn vips_error_buffer() -> *const c_char;
    fn vips_error_clear();
    fn vips_tracked_get_mem() -> usize;
    fn vips_image_new_from_buffer(buf: *const c_void, len: usize, option_string: *const c_char, ...) -> *mut VipsImage;
    fn vips_image_get_width(image: *const VipsImage) -> c_int;
    fn vips_image_get_height(image: *const VipsImage) -> c_int;
    fn vips_image_get_orientation_swap(image: *mut VipsImage) -> c_int;
    fn vips_thumbnail_buffer(buf: *mut c_void, len: usize, out: *mut *mut VipsImage, width: c_int, ...) -> c_int;
    fn vips_thumbnail_image(input: *mut VipsImage, out: *mut *mut VipsImage, width: c_int, ...) -> c_int;
    fn vips_image_copy_memory(image: *mut VipsImage) -> *mut VipsImage;
    fn vips_gaussblur(input: *mut VipsImage, out: *mut *mut VipsImage, sigma: f64, ...) -> c_int;
    fn vips_insert(
        main: *mut VipsImage,
        sub: *mut VipsImage,
        out: *mut *mut VipsImage,
        x: c_int,
        y: c_int,
        ...
    ) -> c_int;
    fn vips_colourspace(input: *mut VipsImage, out: *mut *mut VipsImage, space: c_int, ...) -> c_int;
    fn vips_extract_band(input: *mut VipsImage, out: *mut *mut VipsImage, band: c_int, ...) -> c_int;
    fn vips_hist_equal(input: *mut VipsImage, out: *mut *mut VipsImage, ...) -> c_int;
    fn vips_extract_area(
        input: *mut VipsImage,
        out: *mut *mut VipsImage,
        left: c_int,
        top: c_int,
        width: c_int,
        height: c_int,
        ...
    ) -> c_int;
    fn vips_image_write_to_memory(input: *mut VipsImage, size: *mut usize) -> *mut c_void;
    fn vips_image_write_to_buffer(
        input: *mut VipsImage,
        suffix: *const c_char,
        buf: *mut *mut c_void,
        size: *mut usize,
        ...
    ) -> c_int;
    #[cfg(test)]
    fn vips_invert(input: *mut VipsImage, out: *mut *mut VipsImage, ...) -> c_int;
    #[cfg(test)]
    fn vips_black(out: *mut *mut VipsImage, width: c_int, height: c_int, ...) -> c_int;
    fn g_object_unref(object: *mut c_void);
    fn g_free(mem: *mut c_void);
}

const NULL: *const c_char = ptr::null();

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct VipsError(String);

fn last_error() -> VipsError {
    // ponytail: libvips' error buffer is process-global, so concurrent failures may interleave
    // messages. Fine for diagnostics; switch to per-operation GError plumbing if it ever matters.
    // SAFETY: vips_error_buffer returns a valid NUL-terminated string owned by libvips.
    unsafe {
        let msg = CStr::from_ptr(vips_error_buffer()).to_string_lossy().trim().to_owned();
        vips_error_clear();
        VipsError(msg)
    }
}

/// Initialises libvips once per process. Safe to call repeatedly.
pub fn init() -> Result<(), VipsError> {
    static OK: OnceLock<bool> = OnceLock::new();
    // SAFETY: plain libvips setup calls, run exactly once.
    let ok = *OK.get_or_init(|| unsafe {
        if vips_init(c"citlali".as_ptr()) != 0 {
            return false;
        }
        // Citlali never caches; the libvips operation cache would only hold memory.
        vips_cache_set_max(0);
        // Refuse loaders libvips itself flags as unsafe for untrusted input.
        vips_block_untrusted_set(1);
        true
    });
    ok.then_some(()).ok_or_else(|| VipsError("vips_init failed".into()))
}

/// Bytes currently allocated by libvips pixel buffers.
pub fn tracked_mem() -> usize {
    // SAFETY: reads a libvips counter.
    unsafe { vips_tracked_get_mem() }
}

#[derive(Debug, Clone, Copy)]
#[repr(i32)]
pub enum Size {
    Both = 0,
    Down = 2,
}

/// A lazily evaluated libvips image. `'a` is the input buffer it may still read from.
pub struct Image<'a> {
    ptr: NonNull<VipsImage>,
    _buf: PhantomData<&'a [u8]>,
}

impl Drop for Image<'_> {
    fn drop(&mut self) {
        // SAFETY: we own one reference to a live GObject.
        unsafe { g_object_unref(self.ptr.as_ptr().cast()) }
    }
}

fn wrap<'a>(status: c_int, out: *mut VipsImage) -> Result<Image<'a>, VipsError> {
    match NonNull::new(out) {
        Some(ptr) if status == 0 => Ok(Image { ptr, _buf: PhantomData }),
        _ => Err(last_error()),
    }
}

impl<'a> Image<'a> {
    /// Opens `buf` reading only the header; pixels are decoded on demand.
    pub fn header(buf: &'a [u8]) -> Result<Self, VipsError> {
        // SAFETY: buf outlives the image via 'a.
        let out = unsafe { vips_image_new_from_buffer(buf.as_ptr().cast(), buf.len(), c"".as_ptr(), NULL) };
        wrap(0, out)
    }

    /// Decodes `buf` straight to fit a `width`x`height` box, using shrink-on-load and
    /// honouring EXIF orientation. `crop` fills the box and centre-crops the overflow.
    pub fn thumbnail(buf: &'a [u8], width: i32, height: i32, size: Size, crop: bool) -> Result<Self, VipsError> {
        let mut out = ptr::null_mut();
        // SAFETY: libvips only reads from buf, which outlives the image via 'a.
        let status = unsafe {
            vips_thumbnail_buffer(
                buf.as_ptr().cast_mut().cast(),
                buf.len(),
                &mut out,
                width,
                c"height".as_ptr(),
                height,
                c"size".as_ptr(),
                size as c_int,
                c"crop".as_ptr(),
                c_int::from(crop), // VIPS_INTERESTING_NONE / _CENTRE
                NULL,
            )
        };
        wrap(status, out)
    }

    /// Same as [`Image::thumbnail`] but from an already opened image.
    pub fn thumbnail_image(&self, width: i32, height: i32, size: Size, crop: bool) -> Result<Self, VipsError> {
        let mut out = ptr::null_mut();
        // SAFETY: self is a live image.
        let status = unsafe {
            vips_thumbnail_image(
                self.ptr.as_ptr(),
                &mut out,
                width,
                c"height".as_ptr(),
                height,
                c"size".as_ptr(),
                size as c_int,
                c"crop".as_ptr(),
                c_int::from(crop),
                NULL,
            )
        };
        wrap(status, out)
    }

    /// Renders the pipeline into RAM so it can be read more than once and in any order.
    pub fn copy_memory(&self) -> Result<Self, VipsError> {
        // SAFETY: self is a live image; the result is a new reference.
        wrap(0, unsafe { vips_image_copy_memory(self.ptr.as_ptr()) })
    }

    pub fn gaussblur(&self, sigma: f64) -> Result<Self, VipsError> {
        let mut out = ptr::null_mut();
        // SAFETY: self is a live image.
        let status = unsafe { vips_gaussblur(self.ptr.as_ptr(), &mut out, sigma, NULL) };
        wrap(status, out)
    }

    /// Pastes `sub` onto `self` with its top-left corner at (`x`, `y`).
    pub fn insert(&self, sub: &Image<'a>, x: i32, y: i32) -> Result<Self, VipsError> {
        let mut out = ptr::null_mut();
        // SAFETY: both images are live.
        let status = unsafe { vips_insert(self.ptr.as_ptr(), sub.ptr.as_ptr(), &mut out, x, y, NULL) };
        wrap(status, out)
    }

    /// Greyscale, histogram-equalised, alpha dropped: what the face detector expects.
    pub fn equalised_grey(&self) -> Result<Self, VipsError> {
        const VIPS_INTERPRETATION_B_W: c_int = 1;
        let mut grey = ptr::null_mut();
        // SAFETY: self is a live image.
        let grey =
            wrap(unsafe { vips_colourspace(self.ptr.as_ptr(), &mut grey, VIPS_INTERPRETATION_B_W, NULL) }, grey)?;
        let mut band = ptr::null_mut();
        // SAFETY: grey is a live image.
        let band = wrap(unsafe { vips_extract_band(grey.ptr.as_ptr(), &mut band, 0, NULL) }, band)?;
        let mut out = ptr::null_mut();
        // SAFETY: band is a live image.
        let status = unsafe { vips_hist_equal(band.ptr.as_ptr(), &mut out, NULL) };
        wrap(status, out)
    }

    pub fn extract_area(&self, left: i32, top: i32, width: i32, height: i32) -> Result<Self, VipsError> {
        let mut out = ptr::null_mut();
        // SAFETY: self is a live image.
        let status = unsafe { vips_extract_area(self.ptr.as_ptr(), &mut out, left, top, width, height, NULL) };
        wrap(status, out)
    }

    /// Whether the EXIF orientation swaps width and height (thumbnailing applies it).
    pub fn orientation_swaps(&self) -> bool {
        // SAFETY: self is a live image.
        unsafe { vips_image_get_orientation_swap(self.ptr.as_ptr()) != 0 }
    }

    /// Runs the pipeline and returns the raw pixels, bands interleaved.
    pub fn pixels(&self) -> Result<Vec<u8>, VipsError> {
        let mut len = 0;
        // SAFETY: on success libvips hands us a g_malloc'd buffer of `len` bytes, freed below.
        unsafe {
            let buf = vips_image_write_to_memory(self.ptr.as_ptr(), &mut len);
            if buf.is_null() {
                return Err(last_error());
            }
            let bytes = std::slice::from_raw_parts(buf.cast::<u8>(), len).to_vec();
            g_free(buf);
            Ok(bytes)
        }
    }

    pub fn width(&self) -> i32 {
        // SAFETY: self is a live image.
        unsafe { vips_image_get_width(self.ptr.as_ptr()) }
    }

    pub fn height(&self) -> i32 {
        // SAFETY: self is a live image.
        unsafe { vips_image_get_height(self.ptr.as_ptr()) }
    }

    /// Runs the pipeline and encodes it; `suffix` picks the saver, e.g. `.webp[Q=80]`.
    pub fn save(&self, suffix: &str) -> Result<Vec<u8>, VipsError> {
        let suffix = CString::new(suffix).map_err(|e| VipsError(e.to_string()))?;
        let mut buf = ptr::null_mut();
        let mut len = 0;
        // SAFETY: on success libvips hands us a g_malloc'd buffer of `len` bytes, freed below.
        unsafe {
            if vips_image_write_to_buffer(self.ptr.as_ptr(), suffix.as_ptr(), &mut buf, &mut len, NULL) != 0 {
                return Err(last_error());
            }
            let bytes = std::slice::from_raw_parts(buf.cast::<u8>(), len).to_vec();
            g_free(buf);
            Ok(bytes)
        }
    }

    #[cfg(test)]
    pub fn black(width: i32, height: i32) -> Result<Image<'static>, VipsError> {
        let mut out = ptr::null_mut();
        // SAFETY: allocates a new image with no borrowed input.
        let status = unsafe { vips_black(&mut out, width, height, c"bands".as_ptr(), 3 as c_int, NULL) };
        wrap(status, out)
    }

    #[cfg(test)]
    pub fn invert(&self) -> Result<Self, VipsError> {
        let mut out = ptr::null_mut();
        // SAFETY: self is a live image.
        let status = unsafe { vips_invert(self.ptr.as_ptr(), &mut out, NULL) };
        wrap(status, out)
    }
}
