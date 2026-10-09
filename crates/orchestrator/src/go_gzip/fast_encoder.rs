use super::token::Token;

const TABLE_BITS: u32 = 14;
const TABLE_SIZE: usize = 1 << TABLE_BITS;
const TABLE_MASK: u32 = (1 << TABLE_BITS) - 1;
const TABLE_SHIFT: u32 = 32 - TABLE_BITS;

pub const MAX_STORE_BLOCK_SIZE: i32 = 65535;
const MAX_MATCH_OFFSET: i32 = 1 << 15;
const MAX_MATCH_LENGTH: usize = 258;
const BASE_MATCH_LENGTH: i32 = 3;
const BASE_MATCH_OFFSET: i32 = 1;
const BUFFER_RESET: i32 = i32::MAX - MAX_STORE_BLOCK_SIZE * 2;
const INPUT_MARGIN: i32 = 16 - 1;
const MIN_NON_LITERAL_BLOCK_SIZE: usize = 1 + 1 + INPUT_MARGIN as usize;

#[derive(Clone, Copy, Default)]
struct TableEntry {
    val: u32,
    offset: i32,
}

struct Cursor {
    position: i32,
    value: u32,
    next_hash: u32,
}

struct CopyRun {
    next_emit: i32,
    exhausted: bool,
}

pub struct FastEncoder {
    table: Vec<TableEntry>,
    prev: Vec<u8>,
    cur: i32,
}

impl FastEncoder {
    pub fn new() -> Self {
        Self {
            table: vec![TableEntry::default(); TABLE_SIZE],
            prev: Vec::with_capacity(MAX_STORE_BLOCK_SIZE as usize),
            cur: MAX_STORE_BLOCK_SIZE,
        }
    }

    pub fn encode(&mut self, src: &[u8]) -> Vec<Token> {
        if self.cur >= BUFFER_RESET {
            self.shift_offsets();
        }
        if src.len() < MIN_NON_LITERAL_BLOCK_SIZE {
            self.cur += MAX_STORE_BLOCK_SIZE;
            self.prev.clear();
            return literal_tokens(src);
        }
        let mut tokens = Vec::new();
        let next_emit = self.encode_matches(src, &mut tokens);
        tokens.extend(literal_tokens(&src[next_emit as usize..]));
        self.cur += src.len() as i32;
        self.prev.clear();
        self.prev.extend_from_slice(src);
        tokens
    }

    pub fn reset(&mut self) {
        self.prev.clear();
        self.cur += MAX_MATCH_OFFSET;
        if self.cur >= BUFFER_RESET {
            self.shift_offsets();
        }
    }

    fn encode_matches(&mut self, src: &[u8], tokens: &mut Vec<Token>) -> i32 {
        let s_limit = src.len() as i32 - INPUT_MARGIN;
        let value = load32(src, 0);
        let mut cursor = Cursor {
            position: 0,
            value,
            next_hash: hash(value),
        };
        let mut next_emit = 0;
        while let Some(candidate) = self.seek_candidate(src, s_limit, &mut cursor) {
            tokens.extend(literal_tokens(&src[next_emit as usize..cursor.position as usize]));
            let run = self.emit_copies(src, s_limit, &mut cursor, candidate, tokens);
            next_emit = run.next_emit;
            if run.exhausted {
                break;
            }
        }
        next_emit
    }

    fn seek_candidate(&mut self, src: &[u8], s_limit: i32, cursor: &mut Cursor) -> Option<TableEntry> {
        let mut skip = 32;
        let mut next_s = cursor.position;
        loop {
            cursor.position = next_s;
            let bytes_between_hash_lookups = skip >> 5;
            next_s = cursor.position + bytes_between_hash_lookups;
            skip += bytes_between_hash_lookups;
            if next_s > s_limit {
                return None;
            }
            let slot = (cursor.next_hash & TABLE_MASK) as usize;
            let candidate = self.table[slot];
            let now = load32(src, next_s);
            self.table[slot] = TableEntry {
                offset: cursor.position + self.cur,
                val: cursor.value,
            };
            cursor.next_hash = hash(now);
            if self.is_match(cursor.position, candidate, cursor.value) {
                return Some(candidate);
            }
            cursor.value = now;
        }
    }

    fn is_match(&self, position: i32, candidate: TableEntry, value: u32) -> bool {
        let offset = position - (candidate.offset - self.cur);
        offset <= MAX_MATCH_OFFSET && value == candidate.val
    }

    fn emit_copies(
        &mut self,
        src: &[u8],
        s_limit: i32,
        cursor: &mut Cursor,
        first_candidate: TableEntry,
        tokens: &mut Vec<Token>,
    ) -> CopyRun {
        let mut candidate = first_candidate;
        loop {
            cursor.position += 4;
            let t = candidate.offset - self.cur + 4;
            let length = self.match_len(cursor.position, t, src);
            tokens.push(Token::matched(
                (length + 4 - BASE_MATCH_LENGTH) as u32,
                (cursor.position - t - BASE_MATCH_OFFSET) as u32,
            ));
            cursor.position += length;
            let next_emit = cursor.position;
            if cursor.position >= s_limit {
                return CopyRun {
                    next_emit,
                    exhausted: true,
                };
            }
            let x = load64(src, cursor.position - 1);
            self.insert(x as u32, cursor.position - 1);
            let x = x >> 8;
            candidate = self.table[hash_slot(x as u32)];
            self.insert(x as u32, cursor.position);
            if !self.is_match(cursor.position, candidate, x as u32) {
                cursor.value = (x >> 8) as u32;
                cursor.next_hash = hash(cursor.value);
                cursor.position += 1;
                return CopyRun {
                    next_emit,
                    exhausted: false,
                };
            }
        }
    }

    fn insert(&mut self, value: u32, position: i32) {
        self.table[hash_slot(value)] = TableEntry {
            offset: self.cur + position,
            val: value,
        };
    }

    fn match_len(&self, s: i32, t: i32, src: &[u8]) -> i32 {
        let s = s as usize;
        let s1 = (s + MAX_MATCH_LENGTH - 4).min(src.len());
        match t >= 0 {
            true => common_prefix(&src[s..s1], &src[t as usize..]) as i32,
            false => self.match_len_into_previous(s, s1, t, src),
        }
    }

    fn match_len_into_previous(&self, s: usize, s1: usize, t: i32, src: &[u8]) -> i32 {
        let Some(tp) = usize::try_from(self.prev.len() as i32 + t).ok() else {
            return 0;
        };
        let wanted = &src[s..s1];
        let history = &self.prev[tp..];
        let history = &history[..history.len().min(wanted.len())];
        let matched = common_prefix(wanted, history);
        if matched < history.len() || s + matched == s1 {
            return matched as i32;
        }
        (matched + common_prefix(&src[s + matched..s1], src)) as i32
    }

    fn shift_offsets(&mut self) {
        if self.prev.is_empty() {
            self.table.fill(TableEntry::default());
            self.cur = MAX_MATCH_OFFSET + 1;
            return;
        }
        let cur = self.cur;
        self.table
            .iter_mut()
            .for_each(|entry| entry.offset = (entry.offset - cur + MAX_MATCH_OFFSET + 1).max(0));
        self.cur = MAX_MATCH_OFFSET + 1;
    }
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(left, right)| left == right).count()
}

fn literal_tokens(bytes: &[u8]) -> Vec<Token> {
    bytes.iter().map(|&byte| Token::literal(u32::from(byte))).collect()
}

fn load32(src: &[u8], at: i32) -> u32 {
    let at = at as usize;
    u32::from_le_bytes(std::array::from_fn(|index| src[at + index]))
}

fn load64(src: &[u8], at: i32) -> u64 {
    let at = at as usize;
    u64::from_le_bytes(std::array::from_fn(|index| src[at + index]))
}

fn hash(value: u32) -> u32 {
    value.wrapping_mul(0x1e35a7bd) >> TABLE_SHIFT
}

fn hash_slot(value: u32) -> usize {
    (hash(value) & TABLE_MASK) as usize
}
