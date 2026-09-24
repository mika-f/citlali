//! Request parameters and the resize/encode pipeline.

use serde::Deserialize;

use crate::vips::{Image, Size, VipsError};

pub const MAX_DIMENSION: u32 = 8192;
/// ~0.9 GiB peak for an AVIF input, the most memory-hungry decoder (~9 MiB per megapixel).
pub const DEFAULT_MAX_INPUT_PIXELS: u64 = 100_000_000;
const BLUR_SIGMA: f64 = 20.0;
/// The blur backdrop is computed at 1/`BLUR_DOWNSCALE` size, then scaled back up.
const BLUR_DOWNSCALE: i32 = 8;
/// AVIF encoder speed, libaom-style: 0 slowest/smallest .. 9 fastest. On a Neoverse-N1 core, 9
/// costs ~40% bytes over 6 for ~80% less CPU, and still beats WebP on size.
pub const DEFAULT_AVIF_SPEED: u8 = 9;
pub const MAX_AVIF_SPEED: u8 = 9;
/// Stands in for "no limit" on an axis; below `VIPS_MAX_COORD` on every libvips version.
const UNBOUNDED: i32 = 10_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[serde(alias = "jpg")]
    Jpeg,
    Png,
    Webp,
    Avif,
}

impl Format {
    /// Identifies the input by magic bytes, so only these four decoders are ever reachable.
    pub fn sniff(b: &[u8]) -> Option<Self> {
        match b {
            [0xFF, 0xD8, 0xFF, ..] => Some(Self::Jpeg),
            [0x89, b'P', b'N', b'G', ..] => Some(Self::Png),
            [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some(Self::Webp),
            [s0, s1, s2, s3, b'f', b't', b'y', b'p', ..] => {
                // ISO-BMFF `ftyp` box: major brand, minor version, then compatible brands.
                let end = usize::try_from(u32::from_be_bytes([*s0, *s1, *s2, *s3])).ok()?;
                let brands = b.get(8..end.min(b.len()))?;
                brands.chunks_exact(4).any(|c| c == b"avif" || c == b"avis").then_some(Self::Avif)
            }
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::Png => "png",
            Self::Webp => "webp",
            Self::Avif => "avif",
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Webp => "image/webp",
            Self::Avif => "image/avif",
        }
    }

    /// libvips save suffix. Metadata is always stripped; thumbnailing already converted to sRGB.
    ///
    /// Default qualities differ per format because each encoder's Q scale does: they are
    /// calibrated to the same SSIMULACRA2 score (~73, mozjpeg Q80) on bench/images with the
    /// Docker image's codecs.
    fn save_suffix(self, quality: Option<u8>, avif_speed: u8) -> String {
        match self {
            // Table 3 (ImageMagick's) beats the libjpeg default on both size and SSIMULACRA2.
            Self::Jpeg => format!(".jpg[Q={},optimize_coding,quant_table=3,keep=none]", quality.unwrap_or(78)),
            // effort 2 (default 4): ~36% less CPU per request for ~3% more bytes.
            Self::Webp => format!(".webp[Q={},effort=2,keep=none]", quality.unwrap_or(83)),
            // libvips calls it effort and runs it the other way round (0 fastest .. 9 slowest).
            Self::Avif => {
                let q = quality.unwrap_or_else(|| default_avif_quality(avif_speed));
                format!(".avif[Q={q},effort={},keep=none]", MAX_AVIF_SPEED - avif_speed)
            }
            Self::Png => match quality {
                None => ".png[keep=none]".to_owned(),
                // An explicit quality makes PNG lossy: quantise to a palette.
                Some(q) => format!(".png[palette,Q={q},keep=none]"),
            },
        }
    }
}

/// Server-wide settings, fixed at startup.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// `0..=MAX_AVIF_SPEED`.
    pub avif_speed: u8,
    /// Larger inputs are rejected before any pixels are decoded.
    pub max_input_pixels: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self { avif_speed: DEFAULT_AVIF_SPEED, max_input_pixels: DEFAULT_MAX_INPUT_PIXELS }
    }
}

/// libaom loses quality per Q as it speeds up, so the default Q rises with speed to stay at the
/// shared SSIMULACRA2 target (measured at speeds 6, 7 and 9).
fn default_avif_quality(speed: u8) -> u8 {
    match speed {
        0..=6 => 62,
        7 => 66,
        _ => 69,
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Fit {
    /// Shrink to fit inside the box, keeping aspect ratio. Never enlarges.
    #[default]
    ScaleDown,
    /// Resize (up or down) to fill the box exactly, centre-cropping the overflow.
    Cover,
    /// Like `cover`, but never enlarges: an axis smaller than the box is kept as is.
    Crop,
    /// Fit inside the box without enlarging, on a blurred copy of the image filling the box.
    Blur,
}

#[derive(Debug, Default, Deserialize)]
pub struct Params {
    pub width: Option<u32>,
    pub height: Option<u32>,
    #[serde(default)]
    pub fit: Fit,
    pub quality: Option<u8>,
    /// Defaults to the input format.
    pub format: Option<Format>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unsupported input format; expected JPEG, PNG, WebP or AVIF")]
    UnsupportedFormat,
    #[error("{0}")]
    InvalidParam(&'static str),
    #[error("input exceeds {0} pixels")]
    TooLarge(u64),
    #[error("failed to process image: {0}")]
    Vips(#[from] VipsError),
}

pub struct Output {
    pub format: Format,
    pub bytes: Vec<u8>,
}

pub fn transform(input: &[u8], params: &Params, config: Config) -> Result<Output, Error> {
    let source = Format::sniff(input).ok_or(Error::UnsupportedFormat)?;
    if params.quality.is_some_and(|q| !(1..=100).contains(&q)) {
        return Err(Error::InvalidParam("quality must be 1..=100"));
    }
    let axis = |v: Option<u32>| match v {
        None => Ok(UNBOUNDED),
        Some(v @ 1..=MAX_DIMENSION) => Ok(v as i32),
        Some(_) => Err(Error::InvalidParam("width and height must be 1..=8192")),
    };
    let (w, h) = (axis(params.width)?, axis(params.height)?);

    // Guard against decompression bombs before any pixels are decoded.
    let header = Image::header(input)?;
    if u64::from(header.width().unsigned_abs()) * u64::from(header.height().unsigned_abs()) > config.max_input_pixels {
        return Err(Error::TooLarge(config.max_input_pixels));
    }
    drop(header);

    let image = match (params.fit, params.width.is_some() && params.height.is_some()) {
        (Fit::Cover, true) => Image::thumbnail(input, w, h, Size::Both, true)?,
        (Fit::Crop, true) => Image::thumbnail(input, w, h, Size::Down, true)?,
        (Fit::Blur, true) => blur(input, w, h)?,
        // Without a full box there is nothing to fill, so every fit degrades to scale-down.
        _ => Image::thumbnail(input, w, h, Size::Down, false)?,
    };

    let format = params.format.unwrap_or(source);
    let bytes = image.save(&format.save_suffix(params.quality, config.avif_speed))?;
    Ok(Output { format, bytes })
}

fn blur(input: &[u8], w: i32, h: i32) -> Result<Image<'_>, VipsError> {
    // Decode once into RAM so both layers can read it: a second decode costs as much as the
    // rest of the request for PNG, which has no shrink-on-load.
    // ponytail: holds up to w*h pixels per request (256 MiB at 8192x8192 RGBA); fine at typical sizes.
    let fg = Image::thumbnail(input, w, h, Size::Down, false)?.copy_memory()?;
    // Blurring a small backdrop and scaling it up looks the same and costs ~1/64 as much.
    let bg = fg
        .thumbnail_image((w / BLUR_DOWNSCALE).max(1), (h / BLUR_DOWNSCALE).max(1), Size::Both, true)?
        .gaussblur(BLUR_SIGMA / f64::from(BLUR_DOWNSCALE))?
        .thumbnail_image(w, h, Size::Both, true)?;
    bg.insert(&fg, (w - fg.width()) / 2, (h - fg.height()) / 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vips;

    fn png(width: i32, height: i32) -> Vec<u8> {
        vips::init().unwrap();
        Image::black(width, height).unwrap().save(".png").unwrap()
    }

    fn params(fit: Fit, width: Option<u32>, height: Option<u32>) -> Params {
        Params { width, height, fit, ..Params::default() }
    }

    fn fastest_avif() -> Config {
        Config { avif_speed: MAX_AVIF_SPEED, ..Config::default() }
    }

    fn output_size(input: &[u8], params: &Params) -> (i32, i32) {
        let out = transform(input, params, Config::default()).unwrap();
        let image = Image::header(&out.bytes).unwrap();
        (image.width(), image.height())
    }

    #[test]
    fn scale_down_keeps_aspect_ratio_when_only_width_given() {
        assert_eq!(output_size(&png(400, 200), &params(Fit::ScaleDown, Some(100), None)), (100, 50));
    }

    #[test]
    fn scale_down_never_enlarges() {
        assert_eq!(output_size(&png(400, 200), &params(Fit::ScaleDown, Some(800), Some(800))), (400, 200));
    }

    #[test]
    fn cover_fills_box_exactly() {
        assert_eq!(output_size(&png(400, 200), &params(Fit::Cover, Some(100), Some(100))), (100, 100));
    }

    #[test]
    fn cover_enlarges_small_input() {
        assert_eq!(output_size(&png(40, 20), &params(Fit::Cover, Some(100), Some(100))), (100, 100));
    }

    #[test]
    fn crop_keeps_axis_smaller_than_box() {
        assert_eq!(output_size(&png(400, 200), &params(Fit::Crop, Some(300), Some(300))), (300, 200));
    }

    #[test]
    fn blur_fills_box_exactly() {
        assert_eq!(output_size(&png(400, 200), &params(Fit::Blur, Some(100), Some(100))), (100, 100));
    }

    #[test]
    fn cover_without_height_degrades_to_scale_down() {
        assert_eq!(output_size(&png(400, 200), &params(Fit::Cover, Some(800), None)), (400, 200));
    }

    #[test]
    fn every_input_format_converts_to_every_output_format() {
        let base = png(64, 48);
        for from in [Format::Jpeg, Format::Png, Format::Webp, Format::Avif] {
            let input =
                transform(&base, &Params { format: Some(from), ..Params::default() }, fastest_avif()).unwrap().bytes;
            for to in [Format::Jpeg, Format::Png, Format::Webp, Format::Avif] {
                let params = Params { format: Some(to), quality: Some(50), ..Params::default() };
                let out = transform(&input, &params, fastest_avif());
                assert_eq!(Format::sniff(&out.unwrap().bytes), Some(to), "{from:?} -> {to:?}");
            }
        }
    }

    #[test]
    fn avif_fastest_speed_maps_to_lowest_vips_effort() {
        assert!(Format::Avif.save_suffix(None, MAX_AVIF_SPEED).contains("effort=0"));
    }

    #[test]
    fn avif_default_quality_rises_with_speed() {
        assert!(Format::Avif.save_suffix(None, MAX_AVIF_SPEED).contains("Q=69"));
    }

    #[test]
    fn sniff_rejects_gif() {
        assert_eq!(Format::sniff(b"GIF89a\x01\x00\x01\x00"), None);
    }

    #[test]
    fn sniff_rejects_heic() {
        assert_eq!(Format::sniff(b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00mif1heic"), None);
    }

    #[test]
    fn transform_rejects_input_above_pixel_limit() {
        let config = Config { max_input_pixels: 100, ..Config::default() };
        let err = transform(&png(20, 20), &Params::default(), config);
        assert!(matches!(err, Err(Error::TooLarge(100))));
    }

    #[test]
    fn transform_rejects_quality_zero() {
        let err = transform(&png(8, 8), &Params { quality: Some(0), ..Params::default() }, Config::default());
        assert!(matches!(err, Err(Error::InvalidParam(_))));
    }

    #[test]
    fn transform_rejects_width_above_limit() {
        let err = transform(&png(8, 8), &params(Fit::ScaleDown, Some(MAX_DIMENSION + 1), None), Config::default());
        assert!(matches!(err, Err(Error::InvalidParam(_))));
    }
}
