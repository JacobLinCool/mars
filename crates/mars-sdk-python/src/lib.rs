use mars_sdk::{LiveWriter, VirtualMic};
use mars_types::EnsuredVirtualInput;
use pyo3::{buffer::PyBuffer, exceptions::PyRuntimeError, prelude::*};

#[derive(Debug)]
#[pyclass(name = "LiveWriter")]
pub struct PythonLiveWriter {
    writer: Option<LiveWriter>,
}

#[pymethods]
impl PythonLiveWriter {
    #[new]
    fn new(ensured_json: &str) -> PyResult<Self> {
        let ensured: EnsuredVirtualInput = serde_json::from_str(ensured_json).map_err(|error| {
            PyRuntimeError::new_err(format!("invalid virtual input handle: {error}"))
        })?;
        let writer = VirtualMic::from_info(ensured)
            .open_live_writer()
            .map_err(map_sdk_error)?;
        Ok(Self {
            writer: Some(writer),
        })
    }

    fn write_f32_interleaved_live(
        &mut self,
        py: Python<'_>,
        frames: PyBuffer<f32>,
    ) -> PyResult<usize> {
        let cells = frames.as_slice(py).ok_or_else(|| {
            PyRuntimeError::new_err("frames must be a C-contiguous Float32 buffer")
        })?;
        let samples = cells.iter().map(|sample| sample.get()).collect::<Vec<_>>();
        self.writer_mut()?
            .write_f32_interleaved_live(&samples)
            .map_err(map_sdk_error)
    }

    fn clear_unread(&mut self) -> PyResult<u64> {
        Ok(self.writer_mut()?.clear_unread())
    }

    fn flush_silence(&mut self) -> PyResult<()> {
        self.writer_mut()?.flush_silence().map_err(map_sdk_error)
    }

    fn close(&mut self) {
        self.writer.take();
    }
}

impl PythonLiveWriter {
    fn writer_mut(&mut self) -> PyResult<&mut LiveWriter> {
        self.writer
            .as_mut()
            .ok_or_else(|| PyRuntimeError::new_err("live writer is closed"))
    }
}

fn map_sdk_error(error: mars_sdk::MarsClientError) -> PyErr {
    PyRuntimeError::new_err(error.to_string())
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PythonLiveWriter>()
}
