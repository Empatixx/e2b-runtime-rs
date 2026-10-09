const LENGTH_SHIFT: u32 = 22;
const OFFSET_MASK: u32 = (1 << LENGTH_SHIFT) - 1;
const MATCH_TYPE: u32 = 1 << 30;

const LENGTH_BASE: [u32; 29] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 255,
];

const OFFSET_BASE: [u32; 30] = [
    0x000000, 0x000001, 0x000002, 0x000003, 0x000004, 0x000006, 0x000008, 0x00000c, 0x000010, 0x000018, 0x000020,
    0x000030, 0x000040, 0x000060, 0x000080, 0x0000c0, 0x000100, 0x000180, 0x000200, 0x000300, 0x000400, 0x000600,
    0x000800, 0x000c00, 0x001000, 0x001800, 0x002000, 0x003000, 0x004000, 0x006000,
];

const LENGTH_EXTRA_BITS: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

const OFFSET_EXTRA_BITS: [u32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13,
];

#[derive(Clone, Copy)]
pub struct Token {
    bits: u32,
}

#[derive(Clone, Copy)]
pub struct LengthSymbol {
    pub code: usize,
    pub extra_bits: u32,
    pub extra_value: u32,
}

#[derive(Clone, Copy)]
pub struct OffsetSymbol {
    pub code: usize,
    pub extra_bits: u32,
    pub extra_value: u32,
}

impl Token {
    pub fn literal(value: u32) -> Self {
        Self { bits: value }
    }

    pub fn matched(length_minus_base: u32, offset_minus_base: u32) -> Self {
        Self {
            bits: MATCH_TYPE + (length_minus_base << LENGTH_SHIFT) + offset_minus_base,
        }
    }

    pub fn is_match(self) -> bool {
        self.bits >= MATCH_TYPE
    }

    pub fn literal_value(self) -> usize {
        self.bits as usize
    }

    pub fn length_symbol(self) -> LengthSymbol {
        let length = (self.bits - MATCH_TYPE) >> LENGTH_SHIFT;
        let code = LENGTH_BASE.partition_point(|&base| base <= length) - 1;
        LengthSymbol {
            code,
            extra_bits: LENGTH_EXTRA_BITS[code],
            extra_value: length - LENGTH_BASE[code],
        }
    }

    pub fn offset_symbol(self) -> OffsetSymbol {
        let offset = self.bits & OFFSET_MASK;
        let code = OFFSET_BASE.partition_point(|&base| base <= offset) - 1;
        OffsetSymbol {
            code,
            extra_bits: OFFSET_EXTRA_BITS[code],
            extra_value: offset - OFFSET_BASE[code],
        }
    }
}
