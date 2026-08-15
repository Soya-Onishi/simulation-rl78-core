//! Host-side byte capture attached as a board Wire sink (not a guest peripheral).

use std::sync::{Arc, Mutex};

/// Buffer filled by board UART TX wiring, not by a guest peripheral.
#[derive(Default)]
pub struct ByteCapture {
    bytes: Vec<u8>,
}

impl ByteCapture {
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn push(&mut self, byte: u8) {
        self.bytes.push(byte);
    }

    pub fn clear(&mut self) {
        self.bytes.clear();
    }

    /// Wire sink: append the driven byte.
    pub fn on_input(this: &Arc<Mutex<Self>>, values: &[u8], changed: usize) {
        if let Some(&b) = values.get(changed) {
            this.lock().expect("tx").push(b);
        }
    }
}
