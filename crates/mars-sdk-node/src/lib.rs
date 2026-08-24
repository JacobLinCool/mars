use mars_sdk::{LiveWriter, VirtualMic};
use mars_types::EnsuredVirtualInput;
use napi::bindgen_prelude::Float32Array;
use napi_derive::napi;

#[derive(Debug)]
#[napi]
pub struct NativeLiveWriter {
    writer: Option<LiveWriter>,
}

#[napi]
impl NativeLiveWriter {
    #[napi(constructor)]
    pub fn new(ensured_json: String) -> napi::Result<Self> {
        let ensured: EnsuredVirtualInput =
            serde_json::from_str(&ensured_json).map_err(|error| {
                napi::Error::from_reason(format!("invalid virtual input handle: {error}"))
            })?;
        let writer = VirtualMic::from_info(ensured)
            .open_live_writer()
            .map_err(map_sdk_error)?;
        Ok(Self {
            writer: Some(writer),
        })
    }

    #[napi]
    pub fn write_f32_interleaved_live(&mut self, frames: Float32Array) -> napi::Result<u32> {
        let written = self
            .writer_mut()?
            .write_f32_interleaved_live(&frames)
            .map_err(map_sdk_error)?;
        u32::try_from(written)
            .map_err(|_| napi::Error::from_reason("written frame count exceeds u32"))
    }

    #[napi]
    pub fn clear_unread(&mut self) -> napi::Result<u32> {
        let dropped = self.writer_mut()?.clear_unread();
        u32::try_from(dropped)
            .map_err(|_| napi::Error::from_reason("dropped frame count exceeds u32"))
    }

    #[napi]
    pub fn flush_silence(&mut self) -> napi::Result<()> {
        self.writer_mut()?.flush_silence().map_err(map_sdk_error)
    }

    #[napi]
    pub fn close(&mut self) {
        self.writer.take();
    }
}

impl NativeLiveWriter {
    fn writer_mut(&mut self) -> napi::Result<&mut LiveWriter> {
        self.writer
            .as_mut()
            .ok_or_else(|| napi::Error::from_reason("live writer is closed"))
    }
}

fn map_sdk_error(error: mars_sdk::MarsClientError) -> napi::Error {
    napi::Error::from_reason(error.to_string())
}
