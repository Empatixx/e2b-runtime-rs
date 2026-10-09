use super::bit_writer::BitWriter;
use super::huffman::HuffmanEncoder;
use super::token::Token;

const MAX_NUM_LIT: usize = 286;
const OFFSET_CODE_COUNT: usize = 30;
const CODEGEN_CODE_COUNT: usize = 19;
const END_BLOCK_MARKER: usize = 256;
const LENGTH_CODES_START: usize = 257;
const BAD_CODE: u8 = 255;
const MAX_STORE_BLOCK_SIZE: usize = 65535;
const CODEGEN_ORDER: [usize; CODEGEN_CODE_COUNT] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

struct DynamicHeader {
    num_literals: usize,
    num_offsets: usize,
    num_codegens: usize,
    size: usize,
}

pub struct BlockWriter {
    bits: BitWriter,
    literal_freq: Vec<i32>,
    offset_freq: Vec<i32>,
    codegen: Vec<u8>,
    codegen_freq: [i32; CODEGEN_CODE_COUNT],
    literal_encoding: HuffmanEncoder,
    offset_encoding: HuffmanEncoder,
    codegen_encoding: HuffmanEncoder,
    huff_offset: HuffmanEncoder,
}

impl BlockWriter {
    pub fn new() -> Self {
        let mut huff_offset = HuffmanEncoder::new(OFFSET_CODE_COUNT);
        let mut single_offset = vec![0; OFFSET_CODE_COUNT];
        single_offset[0] = 1;
        huff_offset.generate(&single_offset, 15);
        Self {
            bits: BitWriter::default(),
            literal_freq: vec![0; MAX_NUM_LIT],
            offset_freq: vec![0; OFFSET_CODE_COUNT],
            codegen: vec![0; MAX_NUM_LIT + OFFSET_CODE_COUNT + 1],
            codegen_freq: [0; CODEGEN_CODE_COUNT],
            literal_encoding: HuffmanEncoder::new(MAX_NUM_LIT),
            offset_encoding: HuffmanEncoder::new(OFFSET_CODE_COUNT),
            codegen_encoding: HuffmanEncoder::new(CODEGEN_CODE_COUNT),
            huff_offset,
        }
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.write_stored_header(0, true);
        self.bits.finish()
    }

    fn write_stored_header(&mut self, length: usize, is_eof: bool) {
        self.bits.write_bits(u32::from(is_eof), 3);
        self.bits.align();
        self.bits.write_bits(length as u32, 16);
        self.bits.write_bits(u32::from(!(length as u16)), 16);
    }

    pub fn write_block_dynamic(&mut self, tokens: &[Token], is_eof: bool, input: &[u8]) {
        let (num_literals, num_offsets) = self.index_tokens(tokens);
        let header = self.prepare_dynamic(num_literals, num_offsets, false);
        if stores_better(input, header.size) {
            self.write_stored_block(input, is_eof);
            return;
        }
        self.write_dynamic_header(&header, is_eof);
        self.write_tokens(tokens);
    }

    pub fn write_block_huff(&mut self, is_eof: bool, input: &[u8]) {
        self.literal_freq.fill(0);
        input.iter().for_each(|&byte| self.literal_freq[usize::from(byte)] += 1);
        self.literal_freq[END_BLOCK_MARKER] = 1;
        self.offset_freq[0] = 1;
        self.literal_encoding.generate(&self.literal_freq, 15);
        let header = self.prepare_dynamic(END_BLOCK_MARKER + 1, 1, true);
        if stores_better(input, header.size) {
            self.write_stored_block(input, is_eof);
            return;
        }
        self.write_dynamic_header(&header, is_eof);
        let codes = &self.literal_encoding.codes;
        input
            .iter()
            .for_each(|&byte| self.bits.write_code(codes[usize::from(byte)]));
        self.bits.write_code(codes[END_BLOCK_MARKER]);
    }

    pub fn write_stored_block(&mut self, input: &[u8], is_eof: bool) {
        self.write_stored_header(input.len(), is_eof);
        self.bits.write_bytes(input);
    }

    fn prepare_dynamic(&mut self, num_literals: usize, num_offsets: usize, huff_only: bool) -> DynamicHeader {
        let offset_encoding = if huff_only {
            &self.huff_offset
        } else {
            &self.offset_encoding
        };
        generate_codegen(
            &mut self.codegen,
            &mut self.codegen_freq,
            num_literals,
            num_offsets,
            &self.literal_encoding,
            offset_encoding,
        );
        self.codegen_encoding.generate(&self.codegen_freq, 7);
        let num_codegens = self.num_codegens();
        let size = self.header_size(num_codegens)
            + self.literal_encoding.bit_length(&self.literal_freq)
            + offset_encoding.bit_length(&self.offset_freq);
        DynamicHeader {
            num_literals,
            num_offsets,
            num_codegens,
            size,
        }
    }

    fn num_codegens(&self) -> usize {
        (4..=CODEGEN_CODE_COUNT)
            .rev()
            .find(|&count| self.codegen_freq[CODEGEN_ORDER[count - 1]] != 0)
            .unwrap_or(4)
    }

    fn header_size(&self, num_codegens: usize) -> usize {
        3 + 5
            + 5
            + 4
            + 3 * num_codegens
            + self.codegen_encoding.bit_length(&self.codegen_freq)
            + self.codegen_freq[16] as usize * 2
            + self.codegen_freq[17] as usize * 3
            + self.codegen_freq[18] as usize * 7
    }

    fn write_dynamic_header(&mut self, header: &DynamicHeader, is_eof: bool) {
        self.bits.write_bits(if is_eof { 5 } else { 4 }, 3);
        self.bits.write_bits((header.num_literals - 257) as u32, 5);
        self.bits.write_bits((header.num_offsets - 1) as u32, 5);
        self.bits.write_bits((header.num_codegens - 4) as u32, 4);
        CODEGEN_ORDER[..header.num_codegens].iter().for_each(|&order| {
            self.bits
                .write_bits(u32::from(self.codegen_encoding.codes[order].len), 3)
        });
        self.write_codegen_sequence();
    }

    fn write_codegen_sequence(&mut self) {
        let mut index = 0;
        while self.codegen[index] != BAD_CODE {
            let code_word = usize::from(self.codegen[index]);
            index += 1;
            self.bits.write_code(self.codegen_encoding.codes[code_word]);
            let extra_bits = match code_word {
                16 => 2,
                17 => 3,
                18 => 7,
                _ => 0,
            };
            if extra_bits > 0 {
                self.bits.write_bits(u32::from(self.codegen[index]), extra_bits);
                index += 1;
            }
        }
    }

    fn index_tokens(&mut self, tokens: &[Token]) -> (usize, usize) {
        self.literal_freq.fill(0);
        self.offset_freq.fill(0);
        tokens.iter().for_each(|&token| self.count_token(token));
        self.literal_freq[END_BLOCK_MARKER] += 1;
        let num_literals = self
            .literal_freq
            .iter()
            .rposition(|&freq| freq != 0)
            .map_or(0, |last| last + 1);
        let num_offsets = self
            .offset_freq
            .iter()
            .rposition(|&freq| freq != 0)
            .map_or(0, |last| last + 1);
        let num_offsets = match num_offsets {
            0 => {
                self.offset_freq[0] = 1;
                1
            }
            counted => counted,
        };
        self.literal_encoding.generate(&self.literal_freq, 15);
        self.offset_encoding.generate(&self.offset_freq, 15);
        (num_literals, num_offsets)
    }

    fn count_token(&mut self, token: Token) {
        match token.is_match() {
            false => self.literal_freq[token.literal_value()] += 1,
            true => {
                self.literal_freq[LENGTH_CODES_START + token.length_symbol().code] += 1;
                self.offset_freq[token.offset_symbol().code] += 1;
            }
        }
    }

    fn write_tokens(&mut self, tokens: &[Token]) {
        tokens.iter().for_each(|&token| self.write_token(token));
        self.bits.write_code(self.literal_encoding.codes[END_BLOCK_MARKER]);
    }

    fn write_token(&mut self, token: Token) {
        if !token.is_match() {
            self.bits.write_code(self.literal_encoding.codes[token.literal_value()]);
            return;
        }
        let length = token.length_symbol();
        self.bits
            .write_code(self.literal_encoding.codes[length.code + LENGTH_CODES_START]);
        if length.extra_bits > 0 {
            self.bits.write_bits(length.extra_value, length.extra_bits);
        }
        let offset = token.offset_symbol();
        self.bits.write_code(self.offset_encoding.codes[offset.code]);
        if offset.extra_bits > 0 {
            self.bits.write_bits(offset.extra_value, offset.extra_bits);
        }
    }
}

fn stores_better(input: &[u8], dynamic_size: usize) -> bool {
    input.len() <= MAX_STORE_BLOCK_SIZE && (input.len() + 5) * 8 < dynamic_size + (dynamic_size >> 4)
}

fn generate_codegen(
    codegen: &mut [u8],
    codegen_freq: &mut [i32; CODEGEN_CODE_COUNT],
    num_literals: usize,
    num_offsets: usize,
    literal_encoding: &HuffmanEncoder,
    offset_encoding: &HuffmanEncoder,
) {
    codegen_freq.fill(0);
    literal_encoding.codes[..num_literals]
        .iter()
        .chain(&offset_encoding.codes[..num_offsets])
        .map(|code| code.len as u8)
        .enumerate()
        .for_each(|(index, len)| codegen[index] = len);
    codegen[num_literals + num_offsets] = BAD_CODE;
    let mut run = CodegenRun {
        codegen,
        freq: codegen_freq,
        out_index: 0,
    };
    run.encode_runs();
}

struct CodegenRun<'a> {
    codegen: &'a mut [u8],
    freq: &'a mut [i32; CODEGEN_CODE_COUNT],
    out_index: usize,
}

impl CodegenRun<'_> {
    fn encode_runs(&mut self) {
        let mut size = self.codegen[0];
        let mut count = 1;
        let mut in_index = 1;
        while size != BAD_CODE {
            let next_size = self.codegen[in_index];
            in_index += 1;
            if next_size == size {
                count += 1;
                continue;
            }
            self.emit_run(size, count);
            size = next_size;
            count = 1;
        }
        self.codegen[self.out_index] = BAD_CODE;
    }

    fn emit_run(&mut self, size: u8, count: usize) {
        let remaining = match size {
            0 => self.emit_zero_run(count),
            _ => self.emit_value_run(size, count),
        };
        (0..remaining).for_each(|_| self.push(size));
    }

    fn emit_value_run(&mut self, size: u8, count: usize) -> usize {
        self.push(size);
        let mut count = count - 1;
        while count >= 3 {
            let repeat = count.min(6);
            self.push_repeat(16, repeat - 3);
            count -= repeat;
        }
        count
    }

    fn emit_zero_run(&mut self, count: usize) -> usize {
        let mut count = count;
        while count >= 11 {
            let repeat = count.min(138);
            self.push_repeat(18, repeat - 11);
            count -= repeat;
        }
        if count >= 3 {
            self.push_repeat(17, count - 3);
            count = 0;
        }
        count
    }

    fn push(&mut self, value: u8) {
        self.codegen[self.out_index] = value;
        self.out_index += 1;
        self.freq[usize::from(value)] += 1;
    }

    fn push_repeat(&mut self, code: u8, extra: usize) {
        self.codegen[self.out_index] = code;
        self.codegen[self.out_index + 1] = extra as u8;
        self.out_index += 2;
        self.freq[usize::from(code)] += 1;
    }
}
