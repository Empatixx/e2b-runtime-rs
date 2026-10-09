#[derive(Clone, Copy, Default)]
pub struct HuffmanCode {
    pub code: u16,
    pub len: u16,
}

#[derive(Default)]
pub struct BitWriter {
    bytes: Vec<u8>,
    pending: u64,
    pending_bits: u32,
}

impl BitWriter {
    pub fn write_bits(&mut self, value: u32, count: u32) {
        self.pending |= u64::from(value) << self.pending_bits;
        self.pending_bits += count;
        self.drain_whole_bytes();
    }

    pub fn write_code(&mut self, code: HuffmanCode) {
        self.write_bits(u32::from(code.code), u32::from(code.len));
    }

    pub fn align(&mut self) {
        match self.pending_bits % 8 {
            0 => {}
            used => self.write_bits(0, 8 - used),
        }
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.align();
        self.bytes.extend_from_slice(bytes);
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.align();
        self.bytes
    }

    fn drain_whole_bytes(&mut self) {
        while self.pending_bits >= 8 {
            self.bytes.push(self.pending as u8);
            self.pending >>= 8;
            self.pending_bits -= 8;
        }
    }
}
