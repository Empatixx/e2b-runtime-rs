use std::io::{self, Write};

use flate2::Crc;

use best_speed::BestSpeedDeflate;

mod best_speed;
mod bit_writer;
mod block_writer;
mod fast_encoder;
mod huffman;
mod token;

const HEADER: [u8; 10] = [0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x04, 0xff];

pub struct GoGzipWriter {
    deflate: BestSpeedDeflate,
    crc: Crc,
}

impl GoGzipWriter {
    pub fn best_speed() -> Self {
        Self {
            deflate: BestSpeedDeflate::new(),
            crc: Crc::new(),
        }
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.crc.update(bytes);
        self.deflate.write(bytes);
    }

    pub fn finish(self) -> Vec<u8> {
        let mut out = HEADER.to_vec();
        out.extend(self.deflate.finish());
        out.extend(self.crc.sum().to_le_bytes());
        out.extend(self.crc.amount().to_le_bytes());
        out
    }
}

impl Write for GoGzipWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.write_bytes(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn gzip_best_speed(bytes: &[u8]) -> Vec<u8> {
    let mut writer = GoGzipWriter::best_speed();
    writer.write_bytes(bytes);
    writer.finish()
}

#[cfg(test)]
mod tests {
    use flate2::read::GzDecoder;
    use sha2::{Digest, Sha256};
    use std::io::Read;

    use super::*;

    const WORDS: [&str; 16] = [
        "the",
        "quick",
        "brown",
        "fox",
        "jumps",
        "over",
        "lazy",
        "dog",
        "envd",
        "sandbox",
        "template",
        "layer",
        "/usr/bin/",
        "\n",
        "    ",
        "0755",
    ];

    struct Rng {
        state: u64,
    }

    impl Rng {
        fn new(state: u64) -> Self {
            Self { state }
        }

        fn next(&mut self) -> u64 {
            self.state ^= self.state << 13;
            self.state ^= self.state >> 7;
            self.state ^= self.state << 17;
            self.state
        }

        fn below(&mut self, limit: u64) -> usize {
            (self.next() % limit) as usize
        }
    }

    fn text(rng: &mut Rng, length: usize) -> Vec<u8> {
        let mut out = Vec::new();
        while out.len() < length {
            out.extend_from_slice(WORDS[rng.below(WORDS.len() as u64)].as_bytes());
            out.push(b' ');
        }
        out.truncate(length);
        out
    }

    fn random(rng: &mut Rng, length: usize) -> Vec<u8> {
        (0..length).map(|_| (rng.next() >> 24) as u8).collect()
    }

    fn mixed(rng: &mut Rng, length: usize) -> Vec<u8> {
        let mut out = Vec::new();
        while out.len() < length {
            match rng.below(3) {
                0 => {
                    let chunk = rng.below(2000) + 1;
                    out.extend(random(rng, chunk));
                }
                1 if !out.is_empty() => {
                    let limit = out.len().min(40000) as u64;
                    let distance = rng.below(limit) + 1;
                    let copied = rng.below(300) + 1;
                    (0..copied).for_each(|_| out.push(out[out.len() - distance]));
                }
                1 => {}
                _ => {
                    let chunk = rng.below(500) + 1;
                    out.extend(text(rng, chunk));
                }
            }
        }
        out.truncate(length);
        out
    }

    fn fixtures() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("empty", Vec::new()),
            ("one", b"a".to_vec()),
            ("text16", text(&mut Rng::new(1), 16)),
            ("text17", text(&mut Rng::new(2), 17)),
            ("text100", text(&mut Rng::new(3), 100)),
            ("text127", text(&mut Rng::new(4), 127)),
            ("text128", text(&mut Rng::new(5), 128)),
            ("text1000", text(&mut Rng::new(6), 1000)),
            ("random50", random(&mut Rng::new(7), 50)),
            ("random1000", random(&mut Rng::new(8), 1000)),
            ("text65535", text(&mut Rng::new(9), 65535)),
            ("text65536", text(&mut Rng::new(10), 65536)),
            ("text70000", text(&mut Rng::new(11), 70000)),
            ("text250000", text(&mut Rng::new(12), 250000)),
            ("zeros300000", vec![0; 300000]),
            ("random100000", random(&mut Rng::new(13), 100000)),
            ("random131077", random(&mut Rng::new(14), 131077)),
            ("mixed200000", mixed(&mut Rng::new(15), 200000)),
            ("mixed1000000", mixed(&mut Rng::new(16), 1000000)),
        ]
    }

    const GO_OUTPUT: [(&str, usize, &str); 19] = [
        (
            "empty",
            23,
            "81da0491c5af5635831f6a3febb5d9bfd66987ba3ecc42e58dc3d80938c25705",
        ),
        (
            "one",
            29,
            "2b3843da9cf6852170bc3d756fad5e1602882bc8f5ccad0cd4672f3c0689b445",
        ),
        (
            "text16",
            44,
            "aa2c66f3f7841ebacd3477182c35e836e46bb5cec365028b0f1f2dedc0cc9fc9",
        ),
        (
            "text17",
            45,
            "4a8234cdadd228274d27434142741bc0da378534ae5837d0c0ca578e4fc8fffd",
        ),
        (
            "text100",
            98,
            "02a4143b563884a234b5f50a4789bca7470ba67fd71a12e537989f918642f2e2",
        ),
        (
            "text127",
            122,
            "da0babd601e542433d7bf981808724687341472af8639ac0dfa5ff66acaad7b4",
        ),
        (
            "text128",
            109,
            "d9bda58155976d4c721c85e5554e90c80829cce5c1eec45621c3a73b70f7edae",
        ),
        (
            "text1000",
            334,
            "9f8ed4de709acf67e4713101cafa5b74454229c73b2964820a119b55079b59a7",
        ),
        (
            "random50",
            78,
            "1d3030a5dd869999d5ca0da0a756a34fb756fcf4470811f2f1f47eca35485a96",
        ),
        (
            "random1000",
            1028,
            "8cfda5b893b4250cd67758f89aea012ba455f614607ccbf5f897d7bf91e4cabe",
        ),
        (
            "text65535",
            14891,
            "6af4530b77e46dc87347049a39046baa7b70cf6d6322f7eeb03f0b79fc92f842",
        ),
        (
            "text65536",
            14823,
            "9a6a9cd523fb742257747eda940f96651da89ff151539f5ca17635633249e362",
        ),
        (
            "text70000",
            15895,
            "24f52e89a53c4bb7b4c630eb189398dfd8000b78a9e807a581504ed76fa3e75b",
        ),
        (
            "text250000",
            56364,
            "8f1fac74ce49d4ce61860fdf4d7293b5065aeb78d90fee49e1b72aec113ea1cd",
        ),
        (
            "zeros300000",
            392,
            "cad76f445ee3afce1b5936001ed4f7e5f655ced0635d99a9703fe1082f100c6a",
        ),
        (
            "random100000",
            100033,
            "bb2a31a02a8c550c468f5f64f5b8eaa7e92e3de067fece0ba126fbbd70aca62f",
        ),
        (
            "random131077",
            131115,
            "587dda576be5a4842edee7ba1b2dd4ac9676c390f8b3661e885a49a2c7d586f3",
        ),
        (
            "mixed200000",
            171152,
            "d1b8959ad7b5632f4f5cad3203ce526cfb6cb2db61d52ecd5e26943219b815e0",
        ),
        (
            "mixed1000000",
            882173,
            "0d1459344c23ffb8b68466d981408e8daaa9bd15d04efb0046641d34fc3864dc",
        ),
    ];

    #[test]
    fn output_is_byte_identical_to_go_compress_gzip_best_speed() {
        for (fixture, (name, go_length, go_sha256)) in fixtures().iter().zip(GO_OUTPUT) {
            assert_eq!(fixture.0, name);
            let gzipped = gzip_best_speed(&fixture.1);
            assert_eq!(gzipped.len(), go_length, "{name} length");
            assert_eq!(format!("{:x}", Sha256::digest(&gzipped)), go_sha256, "{name} sha256");
        }
    }

    #[test]
    fn output_round_trips_through_a_gzip_reader() {
        for (name, bytes) in fixtures() {
            let mut decoded = Vec::new();
            GzDecoder::new(gzip_best_speed(&bytes).as_slice())
                .read_to_end(&mut decoded)
                .unwrap();
            assert_eq!(decoded, bytes, "{name}");
        }
    }

    #[test]
    fn chunked_writes_produce_the_same_bytes_as_one_write() {
        let bytes = mixed(&mut Rng::new(17), 300000);
        let mut writer = GoGzipWriter::best_speed();
        bytes.chunks(7919).for_each(|chunk| writer.write_all(chunk).unwrap());
        assert_eq!(writer.finish(), gzip_best_speed(&bytes));
    }
}
