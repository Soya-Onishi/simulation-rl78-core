//! Magic probe: a normal [`sim_kernel::MemoryMapped`] logger device.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use sim_kernel::{BusError, MemoryMapped};

use crate::map::MAGIC_PROBE_SIZE;

/// Destination for probe bytes. Tests inject a buffer; the CLI uses stdout.
pub trait ProbeSink: Send {
    fn emit(&mut self, bytes: &[u8]);
}

/// Writes probe output to the host stdout.
#[derive(Clone, Debug, Default)]
pub struct StdoutSink;

impl ProbeSink for StdoutSink {
    fn emit(&mut self, bytes: &[u8]) {
        let mut out = io::stdout().lock();
        let _ = out.write_all(bytes);
        let _ = out.flush();
    }
}

/// Shared byte buffer for tests.
#[derive(Clone, Debug, Default)]
pub struct BufferSink {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl BufferSink {
    #[must_use]
    pub fn buffer(&self) -> Arc<Mutex<Vec<u8>>> {
        Arc::clone(&self.buf)
    }
}

impl ProbeSink for BufferSink {
    fn emit(&mut self, bytes: &[u8]) {
        self.buf
            .lock()
            .expect("probe buffer")
            .extend_from_slice(bytes);
    }
}

/// MMIO window: any store is forwarded to the sink; loads return 0.
pub struct MagicProbe<S> {
    sink: S,
}

impl<S: ProbeSink> MagicProbe<S> {
    #[must_use]
    pub fn new(sink: S) -> Self {
        Self { sink }
    }
}

impl<S: ProbeSink> MemoryMapped for MagicProbe<S> {
    fn len(&self) -> u64 {
        MAGIC_PROBE_SIZE as u64
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BusError> {
        if offset.saturating_add(buf.len() as u64) > MAGIC_PROBE_SIZE as u64 {
            return Err(BusError::OutOfRange {
                addr: offset,
                offset,
                len: buf.len(),
            });
        }
        buf.fill(0);
        Ok(())
    }

    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), BusError> {
        if offset.saturating_add(buf.len() as u64) > MAGIC_PROBE_SIZE as u64 {
            return Err(BusError::OutOfRange {
                addr: offset,
                offset,
                len: buf.len(),
            });
        }
        self.sink.emit(buf);
        Ok(())
    }
}
