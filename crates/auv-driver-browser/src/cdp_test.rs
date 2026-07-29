use std::io::Cursor;

use auv_driver_common::DriverError;
use base64::Engine;
use image::{DynamicImage, GrayImage, ImageFormat};

use super::decode_capture_png;

#[test]
fn grayscale_capture_is_rejected_before_rgba_expansion() {
  let image = DynamicImage::ImageLuma8(GrayImage::new(5_000, 5_000));
  let mut bytes = Cursor::new(Vec::new());
  image.write_to(&mut bytes, ImageFormat::Png).unwrap();
  let encoded = base64::engine::general_purpose::STANDARD.encode(bytes.into_inner());

  let error = decode_capture_png(&encoded).unwrap_err();
  assert!(matches!(error, DriverError::Backend { .. }));
  assert!(error.to_string().contains("pixel decode limit"));
}
