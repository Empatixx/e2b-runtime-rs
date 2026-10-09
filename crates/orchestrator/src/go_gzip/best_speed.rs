use super::block_writer::BlockWriter;
use super::fast_encoder::{FastEncoder, MAX_STORE_BLOCK_SIZE};

const STORED_BLOCK_LIMIT: usize = 16;
const HUFFMAN_ONLY_LIMIT: usize = 128;

pub struct BestSpeedDeflate {
    window: Vec<u8>,
    encoder: FastEncoder,
    blocks: BlockWriter,
}

impl BestSpeedDeflate {
    pub fn new() -> Self {
        Self {
            window: Vec::with_capacity(MAX_STORE_BLOCK_SIZE as usize),
            encoder: FastEncoder::new(),
            blocks: BlockWriter::new(),
        }
    }

    pub fn write(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            if self.window.len() == MAX_STORE_BLOCK_SIZE as usize {
                self.encode_window();
            }
            let (chunk, rest) = data.split_at(data.len().min(MAX_STORE_BLOCK_SIZE as usize - self.window.len()));
            self.window.extend_from_slice(chunk);
            data = rest;
        }
    }

    pub fn finish(mut self) -> Vec<u8> {
        match self.window.len() {
            0 => {}
            length if length <= STORED_BLOCK_LIMIT => {
                self.flush_small(|blocks, window| blocks.write_stored_block(window, false))
            }
            length if length < HUFFMAN_ONLY_LIMIT => {
                self.flush_small(|blocks, window| blocks.write_block_huff(false, window))
            }
            _ => self.encode_window(),
        }
        self.blocks.finish()
    }

    fn flush_small(&mut self, write: impl FnOnce(&mut BlockWriter, &[u8])) {
        write(&mut self.blocks, &self.window);
        self.window.clear();
        self.encoder.reset();
    }

    fn encode_window(&mut self) {
        let tokens = self.encoder.encode(&self.window);
        let length = self.window.len();
        match tokens.len() > length - (length >> 4) {
            true => self.blocks.write_block_huff(false, &self.window),
            false => self.blocks.write_block_dynamic(&tokens, false, &self.window),
        }
        self.window.clear();
    }
}
