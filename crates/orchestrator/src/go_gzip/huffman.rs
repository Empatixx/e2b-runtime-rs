use super::bit_writer::HuffmanCode;

const MAX_BITS_LIMIT: usize = 16;

#[derive(Clone, Copy)]
struct LiteralNode {
    literal: u16,
    freq: i32,
}

#[derive(Clone, Copy, Default)]
struct LevelInfo {
    last_freq: i32,
    next_char_freq: i32,
    next_pair_freq: i32,
    needed: i32,
}

pub struct HuffmanEncoder {
    pub codes: Vec<HuffmanCode>,
}

impl HuffmanEncoder {
    pub fn new(size: usize) -> Self {
        Self {
            codes: vec![HuffmanCode::default(); size],
        }
    }

    pub fn bit_length(&self, freq: &[i32]) -> usize {
        freq.iter()
            .zip(&self.codes)
            .filter(|(freq, _)| **freq != 0)
            .map(|(freq, code)| *freq as usize * usize::from(code.len))
            .sum()
    }

    pub fn generate(&mut self, freq: &[i32], max_bits: i32) {
        let mut list = self.used_literals(freq);
        if list.len() <= 2 {
            self.assign_single_bit_codes(&list);
            return;
        }
        list.sort_by_key(|node| (node.freq, node.literal));
        let bit_count = bit_counts(&list, max_bits);
        self.assign_encoding_and_size(&bit_count, &mut list);
    }

    fn used_literals(&mut self, freq: &[i32]) -> Vec<LiteralNode> {
        freq.iter()
            .enumerate()
            .filter_map(|(literal, &freq)| match freq {
                0 => {
                    self.codes[literal].len = 0;
                    None
                }
                _ => Some(LiteralNode {
                    literal: literal as u16,
                    freq,
                }),
            })
            .collect()
    }

    fn assign_single_bit_codes(&mut self, list: &[LiteralNode]) {
        list.iter().enumerate().for_each(|(index, node)| {
            self.codes[usize::from(node.literal)] = HuffmanCode {
                code: index as u16,
                len: 1,
            };
        });
    }

    fn assign_encoding_and_size(&mut self, bit_count: &[i32], mut list: &mut [LiteralNode]) {
        let mut code = 0u16;
        for (length, &count) in bit_count.iter().enumerate() {
            code <<= 1;
            if length == 0 || count == 0 {
                continue;
            }
            let (rest, chunk) = list.split_at_mut(list.len() - count as usize);
            chunk.sort_by_key(|node| node.literal);
            for node in chunk.iter() {
                self.codes[usize::from(node.literal)] = HuffmanCode {
                    code: reverse_bits(code, length as u32),
                    len: length as u16,
                };
                code = code.wrapping_add(1);
            }
            list = rest;
        }
    }
}

pub fn reverse_bits(number: u16, bit_length: u32) -> u16 {
    (number << (16 - bit_length)).reverse_bits()
}

fn bit_counts(list: &[LiteralNode], max_bits: i32) -> Vec<i32> {
    let n = list.len() as i32;
    let mut list = list.to_vec();
    list.push(LiteralNode {
        literal: u16::MAX,
        freq: i32::MAX,
    });
    let max_bits = max_bits.min(n - 1) as usize;
    let mut levels = [LevelInfo::default(); MAX_BITS_LIMIT];
    let mut leaf_counts = [[0i32; MAX_BITS_LIMIT]; MAX_BITS_LIMIT];
    for level in 1..=max_bits {
        levels[level] = LevelInfo {
            last_freq: list[1].freq,
            next_char_freq: list[2].freq,
            next_pair_freq: if level == 1 {
                i32::MAX
            } else {
                list[0].freq + list[1].freq
            },
            needed: 0,
        };
        leaf_counts[level][level] = 2;
    }
    levels[max_bits].needed = 2 * n - 4;
    build_levels(&list, &mut levels, &mut leaf_counts, max_bits);
    let counts = &leaf_counts[max_bits];
    let mut bit_count = vec![0i32; max_bits + 1];
    for (bits, level) in (1..=max_bits).zip((1..=max_bits).rev()) {
        bit_count[bits] = counts[level] - counts[level - 1];
    }
    bit_count
}

fn build_levels(
    list: &[LiteralNode],
    levels: &mut [LevelInfo; MAX_BITS_LIMIT],
    leaf_counts: &mut [[i32; MAX_BITS_LIMIT]; MAX_BITS_LIMIT],
    max_bits: usize,
) {
    let mut level = max_bits;
    loop {
        if levels[level].next_pair_freq == i32::MAX && levels[level].next_char_freq == i32::MAX {
            levels[level].needed = 0;
            levels[level + 1].next_pair_freq = i32::MAX;
            level += 1;
            continue;
        }
        let prev_freq = levels[level].last_freq;
        if levels[level].next_char_freq < levels[level].next_pair_freq {
            take_leaf(list, levels, leaf_counts, level);
        } else {
            take_pair(levels, leaf_counts, level);
        }
        levels[level].needed -= 1;
        if levels[level].needed == 0 {
            if level == max_bits {
                break;
            }
            levels[level + 1].next_pair_freq = prev_freq.wrapping_add(levels[level].last_freq);
            level += 1;
        } else {
            while levels[level - 1].needed > 0 {
                level -= 1;
            }
        }
    }
}

fn take_leaf(
    list: &[LiteralNode],
    levels: &mut [LevelInfo; MAX_BITS_LIMIT],
    leaf_counts: &mut [[i32; MAX_BITS_LIMIT]; MAX_BITS_LIMIT],
    level: usize,
) {
    let taken = leaf_counts[level][level] + 1;
    levels[level].last_freq = levels[level].next_char_freq;
    leaf_counts[level][level] = taken;
    levels[level].next_char_freq = list[taken as usize].freq;
}

fn take_pair(
    levels: &mut [LevelInfo; MAX_BITS_LIMIT],
    leaf_counts: &mut [[i32; MAX_BITS_LIMIT]; MAX_BITS_LIMIT],
    level: usize,
) {
    levels[level].last_freq = levels[level].next_pair_freq;
    let (lower, upper) = leaf_counts.split_at_mut(level);
    upper[0][..level].copy_from_slice(&lower[level - 1][..level]);
    levels[level - 1].needed = 2;
}
