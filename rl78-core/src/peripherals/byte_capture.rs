//! Host-side UART capture attached as a board Wire sink (not a guest peripheral).

use std::sync::{Arc, Mutex};

use sim_kernel::{Resettable, UartFrame};

/// Buffer filled by board UART TX wiring, not by a guest peripheral.
#[derive(Default)]
pub struct ByteCapture {
    frames: Vec<UartFrame>,
}

impl ByteCapture {
    #[must_use]
    pub fn frames(&self) -> &[UartFrame] {
        &self.frames
    }

    #[must_use]
    pub fn bytes(&self) -> Vec<u8> {
        self.frames.iter().map(|f| (f.data & 0xFF) as u8).collect()
    }

    pub fn push(&mut self, frame: UartFrame) {
        self.frames.push(frame);
    }

    pub fn clear(&mut self) {
        self.frames.clear();
    }

    /// Wire sink: append the driven UART frame.
    pub fn on_input(this: &Arc<Mutex<Self>>, values: &[UartFrame], changed: usize) {
        if let Some(&frame) = values.get(changed) {
            this.lock().expect("tx").push(frame);
        }
    }
}

impl Resettable for ByteCapture {
    fn reset(&mut self) {
        self.clear();
    }
}
