use std::{
    io::{Cursor, Seek, SeekFrom, Write},
    sync::{Arc, Mutex},
};

use image::RgbImage;
use oxideav_core::{CodecId, CodecParameters, Muxer, Packet, StreamInfo, TimeBase, WriteSeek};
use oxideav_mkv::mux::MkvMuxer;
use rav1e::prelude::{Config, Context, EncoderConfig, EncoderStatus, Rational};

use crate::{cdp::CapturedScreencastFrame, leases::BrowserToolError};

pub fn encode_silent_webm(
    frames: &[CapturedScreencastFrame],
    fps: u32,
    quality: u8,
) -> Result<Vec<u8>, BrowserToolError> {
    let first = frames
        .first()
        .ok_or_else(|| BrowserToolError::artifact_error("screencast captured no frames"))?;
    let first = decode_frame(&first.bytes)?;
    let width = (first.width() as usize) & !1;
    let height = (first.height() as usize) & !1;
    if width < 16 || height < 16 {
        return Err(BrowserToolError::artifact_error(
            "screencast frame is too small for AV1 encoding",
        ));
    }
    let mut encoder = EncoderConfig::with_speed_preset(10);
    encoder.width = width;
    encoder.height = height;
    encoder.time_base = Rational::new(1, u64::from(fps));
    encoder.low_latency = true;
    encoder.min_key_frame_interval = 0;
    encoder.max_key_frame_interval = u64::from(fps) * 2;
    encoder.quantizer = (255usize.saturating_sub(usize::from(quality) * 2)).clamp(20, 200);
    let config = Config::new().with_encoder_config(encoder).with_threads(2);
    let mut context: Context<u8> = config
        .new_context()
        .map_err(|error| BrowserToolError::artifact_error(error.to_string()))?;
    for captured in frames {
        let decoded = decode_frame(&captured.bytes)?;
        let rgb = if decoded.width() as usize != width || decoded.height() as usize != height {
            image::imageops::resize(
                &decoded,
                width as u32,
                height as u32,
                image::imageops::FilterType::Triangle,
            )
        } else {
            decoded
        };
        let (y, u, v) = rgb_to_i420(&rgb, width, height);
        let mut frame = context.new_frame();
        frame.planes[0].copy_from_raw_u8(&y, width, 1);
        frame.planes[1].copy_from_raw_u8(&u, width / 2, 1);
        frame.planes[2].copy_from_raw_u8(&v, width / 2, 1);
        context
            .send_frame(frame)
            .map_err(|error| BrowserToolError::artifact_error(error.to_string()))?;
    }
    context.flush();

    let writer = SharedWriter::default();
    let bytes = writer.bytes.clone();
    let mut parameters = CodecParameters::video(CodecId::new("av1"));
    parameters.width = Some(width as u32);
    parameters.height = Some(height as u32);
    let time_base = TimeBase::new(1, i64::from(fps));
    let stream = StreamInfo {
        index: 0,
        time_base,
        duration: Some(frames.len() as i64),
        start_time: Some(0),
        params: parameters,
    };
    let output: Box<dyn WriteSeek> = Box::new(writer);
    let mut muxer = MkvMuxer::new_webm(output, &[stream])
        .map_err(|error| BrowserToolError::artifact_error(error.to_string()))?;
    muxer
        .write_header()
        .map_err(|error| BrowserToolError::artifact_error(error.to_string()))?;
    loop {
        match context.receive_packet() {
            Ok(encoded) => {
                let mut packet = Packet::new(0, time_base, encoded.data);
                packet.pts = Some(encoded.input_frameno as i64);
                packet.dts = packet.pts;
                packet.duration = Some(1);
                packet.flags.keyframe = encoded.frame_type.all_intra();
                muxer
                    .write_packet(&packet)
                    .map_err(|error| BrowserToolError::artifact_error(error.to_string()))?;
            }
            Err(EncoderStatus::Encoded) => {}
            Err(EncoderStatus::LimitReached) => break,
            Err(EncoderStatus::NeedMoreData) => continue,
            Err(error) => return Err(BrowserToolError::artifact_error(error.to_string())),
        }
    }
    muxer
        .write_trailer()
        .map_err(|error| BrowserToolError::artifact_error(error.to_string()))?;
    drop(muxer);
    Ok(bytes.lock().unwrap().get_ref().clone())
}

fn decode_frame(bytes: &[u8]) -> Result<RgbImage, BrowserToolError> {
    image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg)
        .map(|image| image.to_rgb8())
        .map_err(|error| {
            BrowserToolError::artifact_error(format!("invalid screencast frame: {error}"))
        })
}

fn rgb_to_i420(image: &RgbImage, width: usize, height: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut y = vec![0; width * height];
    let mut u = vec![0; width * height / 4];
    let mut v = vec![0; width * height / 4];
    for row in 0..height {
        for column in 0..width {
            let pixel = image.get_pixel(column as u32, row as u32).0;
            y[row * width + column] = clamp(
                (77 * i32::from(pixel[0]) + 150 * i32::from(pixel[1]) + 29 * i32::from(pixel[2]))
                    >> 8,
            );
        }
    }
    for row in (0..height).step_by(2) {
        for column in (0..width).step_by(2) {
            let mut sum_u = 0;
            let mut sum_v = 0;
            for dy in 0..2 {
                for dx in 0..2 {
                    let pixel = image.get_pixel((column + dx) as u32, (row + dy) as u32).0;
                    let red = i32::from(pixel[0]);
                    let green = i32::from(pixel[1]);
                    let blue = i32::from(pixel[2]);
                    sum_u += ((-43 * red - 85 * green + 128 * blue) >> 8) + 128;
                    sum_v += ((128 * red - 107 * green - 21 * blue) >> 8) + 128;
                }
            }
            let index = (row / 2) * (width / 2) + column / 2;
            u[index] = clamp(sum_u / 4);
            v[index] = clamp(sum_v / 4);
        }
    }
    (y, u, v)
}

fn clamp(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

#[derive(Clone, Default)]
struct SharedWriter {
    bytes: Arc<Mutex<Cursor<Vec<u8>>>>,
}

impl Write for SharedWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes.lock().unwrap().write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.bytes.lock().unwrap().flush()
    }
}

impl Seek for SharedWriter {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.bytes.lock().unwrap().seek(position)
    }
}
