//! A minimal reader for the PPM images QEMU's `screendump` writes.
//!
//! # Why PPM and not PNG
//!
//! QEMU's `screendump` can write two formats, and the choice matters for a
//! golden test:
//!
//! * **PNG** is compressed with zlib. Reading it back needs an inflate
//!   implementation, which means either a dependency or a few hundred lines
//!   of someone's careful bit-twiddling.
//! * **PPM** (`P6`) is a text header followed by the raw pixels. Reading it
//!   back needs a header parser and a `memcpy`.
//!
//! A golden test compares pixels; it does not care about the container. So
//! this module reads the uncompressed format, and the golden reference is
//! stored as a PPM.
//!
//! # The format, precisely
//!
//! QEMU writes the **binary** variant (`P6`), whose header is:
//!
//! ```text
//! P6\n<width> <height>\n<maxval>\n<binary pixel data>
//! ```
//!
//! Verified against QEMU 10.2 on this machine: the file begins with the
//! literal bytes `P6\n1280 800\n255\n`, and the pixel data that follows is
//! exactly `width * height * 3` bytes. The whitespace between the header
//! fields is a single newline in QEMU's output, but the reader accepts any
//! run of whitespace, because the format allows it.
//!
//! Pixels are stored **top row first** and each pixel is **red, green,
//! blue** in that order — *not* the `Bgr` order the framebuffer itself uses.
//! QEMU converts on the way out, so a PPM read back is always RGB.

/// A decoded PPM image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The RGB pixels, three bytes each, top row first.
    pub pixels: Vec<u8>,
}

impl Image {
    /// The pixel at `(x, y)`, as an RGB triple.
    ///
    /// Returns `None` when the coordinates are outside the image.
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 3]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let index = ((y as usize) * (self.width as usize) + (x as usize)) * 3;
        Some([
            self.pixels[index],
            self.pixels[index + 1],
            self.pixels[index + 2],
        ])
    }

    /// The number of pixels that differ from `other` at the same position.
    ///
    /// Two images of different sizes have every pixel of the smaller one
    /// counted as different, because the comparison is over the overlapping
    /// region and the sizes are reported separately.
    pub fn differing_pixels(&self, other: &Image) -> usize {
        let common_w = self.width.min(other.width);
        let common_h = self.height.min(other.height);
        let mut differing = 0usize;
        for y in 0..common_h {
            for x in 0..common_w {
                if self.pixel(x, y) != other.pixel(x, y) {
                    differing += 1;
                }
            }
        }
        // The parts that do not overlap are differences too.
        let self_area = (self.width as usize) * (self.height as usize);
        let other_area = (other.width as usize) * (other.height as usize);
        let common_area = (common_w as usize) * (common_h as usize);
        differing += self_area.saturating_sub(common_area);
        differing += other_area.saturating_sub(common_area);
        differing
    }

    /// The fraction of pixels that differ, in the range `0.0..=1.0`.
    pub fn difference_ratio(&self, other: &Image) -> f64 {
        let area = (self.width as usize)
            .saturating_mul(self.height as usize)
            .max((other.width as usize).saturating_mul(other.height as usize));
        if area == 0 {
            return 0.0;
        }
        self.differing_pixels(other) as f64 / area as f64
    }
}

/// Why a byte slice could not be read as a PPM image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PpmError {
    /// The file was too short to hold a header.
    Truncated,
    /// The magic was not `P6`.
    NotBinaryPpm,
    /// The header did not parse as `P6 <width> <height> <maxval>`.
    BadHeader(String),
    /// The maximum value was not 255, which is the only depth this reader
    /// supports.
    UnsupportedDepth(u32),
    /// The pixel data was shorter than the header promised.
    ShortPixels {
        /// How many bytes the header said there were.
        expected: usize,
        /// How many bytes were actually there.
        got: usize,
    },
}

impl std::fmt::Display for PpmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => write!(f, "the file is too short to hold a PPM header"),
            Self::NotBinaryPpm => write!(
                f,
                "the file is not a binary PPM; QEMU's `screendump -f ppm` writes `P6`"
            ),
            Self::BadHeader(detail) => write!(f, "the PPM header is malformed: {detail}"),
            Self::UnsupportedDepth(d) => write!(
                f,
                "the PPM's maximum value is {d}; only 255 (eight bits per channel) is supported"
            ),
            Self::ShortPixels { expected, got } => write!(
                f,
                "the PPM header promises {expected} pixel bytes but the file has {got}"
            ),
        }
    }
}

/// Read a binary PPM from `bytes`.
pub fn decode(bytes: &[u8]) -> Result<Image, PpmError> {
    // The magic.
    if bytes.len() < 2 {
        return Err(PpmError::Truncated);
    }
    if &bytes[..2] != b"P6" {
        return Err(PpmError::NotBinaryPpm);
    }

    // The header is whitespace-separated tokens after the magic. The pixel
    // data starts at the first byte after the whitespace that follows the
    // maximum value — and that byte may itself be whitespace, which is the
    // classic PPM trap. A comment (`# ...`) may appear between tokens.
    let mut cursor = 2usize;
    let mut tokens: Vec<String> = Vec::new();
    while tokens.len() < 3 {
        // Skip whitespace and comments.
        loop {
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor < bytes.len() && bytes[cursor] == b'#' {
                while cursor < bytes.len() && bytes[cursor] != b'\n' {
                    cursor += 1;
                }
                continue;
            }
            break;
        }
        if cursor >= bytes.len() {
            return Err(PpmError::Truncated);
        }
        let start = cursor;
        while cursor < bytes.len() && !bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        tokens.push(String::from_utf8_lossy(&bytes[start..cursor]).into_owned());
    }

    let width: u32 = tokens[0]
        .parse()
        .map_err(|_| PpmError::BadHeader(format!("width {:?}", tokens[0])))?;
    let height: u32 = tokens[1]
        .parse()
        .map_err(|_| PpmError::BadHeader(format!("height {:?}", tokens[1])))?;
    let maxval: u32 = tokens[2]
        .parse()
        .map_err(|_| PpmError::BadHeader(format!("maximum value {:?}", tokens[2])))?;

    if maxval != 255 {
        return Err(PpmError::UnsupportedDepth(maxval));
    }
    if width == 0 || height == 0 {
        return Err(PpmError::BadHeader(format!(
            "the image is {width}x{height}; both must be non-zero"
        )));
    }

    // Exactly one whitespace byte separates the header from the pixel data.
    if cursor >= bytes.len() || !bytes[cursor].is_ascii_whitespace() {
        return Err(PpmError::BadHeader(
            "the maximum value is not followed by whitespace".into(),
        ));
    }
    cursor += 1;

    let expected = (width as usize) * (height as usize) * 3;
    let got = bytes.len() - cursor;
    if got < expected {
        return Err(PpmError::ShortPixels { expected, got });
    }
    Ok(Image {
        width,
        height,
        pixels: bytes[cursor..cursor + expected].to_vec(),
    })
}

/// Encode an image back to the binary PPM format.
///
/// The encoder exists so a golden reference can be written in exactly the
/// format QEMU produces, byte for byte.
pub fn encode(image: &Image) -> Vec<u8> {
    let mut out = Vec::with_capacity(image.pixels.len() + 32);
    out.extend_from_slice(format!("P6\n{} {}\n255\n", image.width, image.height).as_bytes());
    out.extend_from_slice(&image.pixels);
    out
}
