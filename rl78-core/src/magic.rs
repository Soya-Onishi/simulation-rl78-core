//! Magic probe: a normal [`sim_kernel::MemoryMapped`] logger device.

use std::io::{self, Write};

use sim_kernel::{BusError, MemoryMapped};

use crate::map::MAGIC_PROBE_SIZE;

/// Destination for probe bytes (stdout in the CLI; tests inject their own sink).
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
