//! Host-side byte capture attached as a board Wire sink (not a guest peripheral).

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
}
