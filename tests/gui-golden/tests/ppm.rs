//! Tests for the PPM reader.
//!
//! The reader is the one piece of the golden test that runs without QEMU, so
//! it is the piece that can be tested everywhere — and it must be, because a
//! decoder that mis-parses the header would make every golden comparison
//! silently meaningless.

use vela_test_gui_golden::ppm::{self, Image, PpmError};

/// Build a PPM file with the given header and pixel data.
fn ppm_bytes(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
    let mut out = format!("P6\n{width} {height}\n255\n").into_bytes();
    out.extend_from_slice(pixels);
    out
}

#[test]
fn a_qemu_screendump_decodes() {
    // This is the exact shape QEMU 10.2 writes: `P6\n1280 800\n255\n`
    // followed by exactly `1280 * 800 * 3` bytes.
    let width = 4u32;
    let height = 3u32;
    let pixels: Vec<u8> = (0..(width * height * 3)).map(|i| (i % 256) as u8).collect();
    let bytes = ppm_bytes(width, height, &pixels);

    let image = ppm::decode(&bytes).expect("a QEMU-shaped PPM must decode");
    assert_eq!(image.width, 4);
    assert_eq!(image.height, 3);
    assert_eq!(image.pixels, pixels);
}

#[test]
fn pixels_are_rgb_top_row_first() {
    // A 2x1 image: red, then green.
    let bytes = ppm_bytes(2, 1, &[255, 0, 0, 0, 255, 0]);
    let image = ppm::decode(&bytes).unwrap();
    assert_eq!(image.pixel(0, 0), Some([255, 0, 0]));
    assert_eq!(image.pixel(1, 0), Some([0, 255, 0]));
}

#[test]
fn out_of_range_pixels_are_none() {
    let bytes = ppm_bytes(2, 2, &[0; 12]);
    let image = ppm::decode(&bytes).unwrap();
    assert_eq!(image.pixel(2, 0), None);
    assert_eq!(image.pixel(0, 2), None);
    assert_eq!(image.pixel(1, 1), Some([0, 0, 0]));
}

#[test]
fn a_non_ppm_is_rejected() {
    assert_eq!(
        ppm::decode(b"\x89PNG\r\n\x1a\n"),
        Err(PpmError::NotBinaryPpm)
    );
}

#[test]
fn an_empty_file_is_rejected() {
    assert_eq!(ppm::decode(b""), Err(PpmError::Truncated));
    assert_eq!(ppm::decode(b"P"), Err(PpmError::Truncated));
}

#[test]
fn a_truncated_header_is_rejected() {
    assert_eq!(ppm::decode(b"P6\n1280"), Err(PpmError::Truncated));
}

#[test]
fn a_depth_other_than_255_is_rejected() {
    let bytes = b"P6\n2 2\n65535\n".to_vec();
    assert_eq!(ppm::decode(&bytes), Err(PpmError::UnsupportedDepth(65535)));
}

#[test]
fn short_pixel_data_is_rejected() {
    let bytes = b"P6\n4 4\n255\nshort".to_vec();
    match ppm::decode(&bytes) {
        Err(PpmError::ShortPixels { expected, got }) => {
            assert_eq!(expected, 4 * 4 * 3);
            assert_eq!(got, 5);
        }
        other => panic!("expected a short-pixel error, got {other:?}"),
    }
}

#[test]
fn extra_trailing_bytes_are_ignored() {
    // QEMU may write more than one frame's worth if the file is reused; the
    // decoder must stop at the end of the first image.
    let mut bytes = ppm_bytes(2, 1, &[1, 2, 3, 4, 5, 6]);
    bytes.extend_from_slice(b"trailing garbage");
    let image = ppm::decode(&bytes).unwrap();
    assert_eq!(image.pixels.len(), 6);
}

#[test]
fn comments_in_the_header_are_skipped() {
    // The PPM format permits `#` comments between tokens.
    let mut bytes = b"P6\n# a comment\n2 1\n255\n".to_vec();
    bytes.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
    let image = ppm::decode(&bytes).unwrap();
    assert_eq!(image.width, 2);
    assert_eq!(image.pixels.len(), 6);
}

#[test]
fn encoding_round_trips() {
    let pixels: Vec<u8> = (0..18).collect();
    let image = Image {
        width: 2,
        height: 3,
        pixels: pixels.clone(),
    };
    let bytes = ppm::encode(&image);
    assert!(bytes.starts_with(b"P6\n2 3\n255\n"));
    let decoded = ppm::decode(&bytes).unwrap();
    assert_eq!(decoded, image);
    assert_eq!(decoded.pixels, pixels);
}

#[test]
fn identical_images_differ_in_no_pixels() {
    let image = Image {
        width: 2,
        height: 2,
        pixels: vec![0; 12],
    };
    assert_eq!(image.differing_pixels(&image), 0);
    assert_eq!(image.difference_ratio(&image), 0.0);
}

#[test]
fn a_single_changed_pixel_is_counted() {
    let mut a = Image {
        width: 2,
        height: 2,
        pixels: vec![0; 12],
    };
    let mut b = a.clone();
    a.pixels[0] = 255;
    b.pixels[0] = 255;
    assert_eq!(a.differing_pixels(&b), 0);

    let mut c = a.clone();
    c.pixels[3] = 255;
    assert_eq!(a.differing_pixels(&c), 1);
}

#[test]
fn images_of_different_sizes_are_all_different() {
    let a = Image {
        width: 2,
        height: 2,
        pixels: vec![0; 12],
    };
    let b = Image {
        width: 4,
        height: 4,
        pixels: vec![0; 48],
    };
    // The 4 pixels of the smaller image overlap, so only the 12 pixels that
    // the larger image has and the smaller one does not are counted.
    assert_eq!(a.differing_pixels(&b), 12);
}
