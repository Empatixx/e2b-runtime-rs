use std::io::Read;
use std::path::Path;

use super::error::{Kind, WarmError};

const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const CLASS_32: u8 = 1;
const CLASS_64: u8 = 2;
const DATA_LITTLE: u8 = 1;
const DATA_BIG: u8 = 2;
const SECTION_STRING_TABLE: u32 = 3;

struct Layout {
    class64: bool,
    little: bool,
}

struct Header {
    phoff: u64,
    shoff: u64,
    phentsize: u64,
    shentsize: u64,
    phnum: u64,
    shnum: u64,
    shstrndx: u64,
}

pub fn validate_image(path: &Path, size: u64) -> Result<(), WarmError> {
    let name = format!("{:?}", path.display());
    let mut file =
        std::fs::File::open(path).map_err(|e| WarmError::other(format!("open envd binary copy {name}: {}", e)))?;
    let mut image = vec![0u8; size as usize];
    file.read_exact(&mut image)
        .map_err(|e| WarmError::other(format!("read envd binary copy {name}: {}", read_reason(&e))))?;
    parse(&image).map_err(|problem| WarmError::wrapping(Kind::BadImage, &name, &WarmError::other(problem)))
}

fn read_reason(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::UnexpectedEof => "unexpected EOF".to_owned(),
        _ => error.to_string(),
    }
}

fn parse(image: &[u8]) -> Result<(), String> {
    let layout = identify(image)?;
    let header = header(image, &layout)?;
    header_checks(&header, &layout)?;
    programs(image, &header)?;
    sections(image, &header, &layout)
}

fn identify(image: &[u8]) -> Result<Layout, String> {
    let ident = image.get(..16).ok_or("unexpected EOF")?;
    if ident[..4] != ELF_MAGIC {
        return Err(format!("bad magic number {:?}", &ident[..4]));
    }
    let class64 = match ident[4] {
        CLASS_32 => false,
        CLASS_64 => true,
        other => return Err(format!("unknown ELF class {other}")),
    };
    let little = match ident[5] {
        DATA_LITTLE => true,
        DATA_BIG => false,
        other => return Err(format!("unknown ELF data encoding {other}")),
    };
    match ident[6] {
        1 => Ok(Layout { class64, little }),
        other => Err(format!("unknown ELF version {other}")),
    }
}

fn header(image: &[u8], layout: &Layout) -> Result<Header, String> {
    let field = |offset: usize, width: usize| -> Result<u64, String> {
        let bytes = image.get(offset..offset + width).ok_or("unexpected EOF")?;
        Ok(read_number(bytes, layout.little))
    };
    match layout.class64 {
        true => Ok(Header {
            phoff: field(32, 8)?,
            shoff: field(40, 8)?,
            phentsize: field(54, 2)?,
            phnum: field(56, 2)?,
            shentsize: field(58, 2)?,
            shnum: field(60, 2)?,
            shstrndx: field(62, 2)?,
        }),
        false => Ok(Header {
            phoff: field(28, 4)?,
            shoff: field(32, 4)?,
            phentsize: field(42, 2)?,
            phnum: field(44, 2)?,
            shentsize: field(46, 2)?,
            shnum: field(48, 2)?,
            shstrndx: field(50, 2)?,
        }),
    }
}

fn header_checks(header: &Header, layout: &Layout) -> Result<(), String> {
    let (want_program, want_section) = match layout.class64 {
        true => (56, 64),
        false => (32, 40),
    };
    if header.shoff == 0 && header.shnum != 0 {
        return Err("invalid ELF shnum for shoff=0".to_owned());
    }
    if header.shnum > 0 && header.shstrndx >= header.shnum {
        return Err("invalid ELF shstrndx".to_owned());
    }
    if header.phnum > 0 && header.phentsize < want_program {
        return Err("invalid ELF phentsize".to_owned());
    }
    if header.shnum > 0 && header.shentsize < want_section {
        return Err("invalid ELF shentsize".to_owned());
    }
    Ok(())
}

fn programs(image: &[u8], header: &Header) -> Result<(), String> {
    match header.phnum {
        0 => Ok(()),
        count => within(image, header.phoff, count * header.phentsize).map(|_| ()),
    }
}

fn sections(image: &[u8], header: &Header, layout: &Layout) -> Result<(), String> {
    if header.shnum == 0 {
        return Ok(());
    }
    let table = within(image, header.shoff, header.shnum * header.shentsize)?;
    if header.shstrndx == 0 {
        return Ok(());
    }
    let entry = &table[(header.shstrndx * header.shentsize) as usize..];
    let kind = read_number(&entry[4..8], layout.little);
    if kind != u64::from(SECTION_STRING_TABLE) {
        return Err("invalid ELF section name string table type".to_owned());
    }
    let (offset, length) = match layout.class64 {
        true => (
            read_number(&entry[24..32], layout.little),
            read_number(&entry[32..40], layout.little),
        ),
        false => (
            read_number(&entry[16..20], layout.little),
            read_number(&entry[20..24], layout.little),
        ),
    };
    within(image, offset, length).map(|_| ())
}

fn within(image: &[u8], offset: u64, length: u64) -> Result<&[u8], String> {
    let end = offset.checked_add(length).ok_or("unexpected EOF")?;
    image
        .get(offset as usize..end as usize)
        .ok_or_else(|| "unexpected EOF".to_owned())
}

fn read_number(bytes: &[u8], little: bool) -> u64 {
    let mut value = 0u64;
    let ordered: Box<dyn Iterator<Item = &u8>> = match little {
        true => Box::new(bytes.iter().rev()),
        false => Box::new(bytes.iter()),
    };
    for byte in ordered {
        value = (value << 8) | u64::from(*byte);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::super::testing::{ELF_HEADER_LEN, elf_with_payload};
    use super::*;

    fn validated(bytes: &[u8]) -> Result<(), WarmError> {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("img");
        std::fs::write(&path, bytes).unwrap();
        validate_image(&path, bytes.len() as u64)
    }

    #[test]
    fn structural_validation_owns_incompleteness_only() {
        let full = elf_with_payload("the-whole-binary");

        assert!(
            validated(&full[..20]).is_err(),
            "an image cut inside its header must be rejected structurally"
        );

        let mut corrupt = full.clone();
        corrupt[ELF_HEADER_LEN..].fill(0xff);
        assert_eq!(
            validated(&corrupt),
            Ok(()),
            "corruption past the headers parses; judging it is the probe's job"
        );
    }

    #[test]
    fn only_the_parse_judges_the_bytes() {
        let full = elf_with_payload("the-whole-binary");
        let directory = tempfile::tempdir().unwrap();

        assert!(
            validated(&full[..20]).unwrap_err().is(Kind::BadImage),
            "truncation is a verdict"
        );

        let short = directory.path().join("img");
        std::fs::write(&short, &full).unwrap();
        let error = validate_image(&short, full.len() as u64 + 4096).unwrap_err();
        assert!(
            !error.is(Kind::BadImage),
            "a copy shorter than its identity is this node's fault"
        );

        let unreadable = validate_image(directory.path(), 64).unwrap_err();
        assert!(
            !unreadable.is(Kind::BadImage),
            "failing to read the copy is about this node"
        );
    }

    #[test]
    fn a_complete_section_table_with_a_string_table_past_the_end_is_incomplete() {
        let mut image = elf_with_payload("data");
        let shoff = image.len() as u64;
        image[40..48].copy_from_slice(&shoff.to_le_bytes());
        image[60..62].copy_from_slice(&2u16.to_le_bytes());
        image[62..64].copy_from_slice(&1u16.to_le_bytes());
        let mut table = vec![0u8; 128];
        table[64 + 4..64 + 8].copy_from_slice(&3u32.to_le_bytes());
        table[64 + 24..64 + 32].copy_from_slice(&(image.len() as u64 + 128).to_le_bytes());
        table[64 + 32..64 + 40].copy_from_slice(&8u64.to_le_bytes());
        image.extend(table);

        assert!(
            validated(&image).unwrap_err().is(Kind::BadImage),
            "the string table is cut off"
        );
    }

    #[test]
    fn bad_magic_and_unknown_classes_are_rejected() {
        let mut image = elf_with_payload("x");
        image[0] = 0;
        assert!(validated(&image).unwrap_err().is(Kind::BadImage));

        let mut image = elf_with_payload("x");
        image[4] = 9;
        assert!(validated(&image).unwrap_err().is(Kind::BadImage));
    }
}
