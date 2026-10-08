pub const REQUEST_SIZE: usize = 28;
pub const REPLY_SIZE: usize = 16;
pub const BACKEND_ERROR: u32 = 1;
pub const MAX_WRITE: u32 = 32 * 1024 * 1024;

const REQUEST_MAGIC: u32 = 0x2560_9513;
const REPLY_MAGIC: u32 = 0x6744_6698;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Read,
    Write,
    Disconnect,
    Flush,
    Trim,
    WriteZeroes,
}

impl Command {
    pub fn code(&self) -> u16 {
        match self {
            Command::Read => 0,
            Command::Write => 1,
            Command::Disconnect => 2,
            Command::Flush => 3,
            Command::Trim => 4,
            Command::WriteZeroes => 6,
        }
    }

    fn from_code(code: u16) -> Result<Self, String> {
        match code {
            0 => Ok(Command::Read),
            1 => Ok(Command::Write),
            2 => Ok(Command::Disconnect),
            3 => Ok(Command::Flush),
            4 => Ok(Command::Trim),
            6 => Ok(Command::WriteZeroes),
            other => Err(format!("unsupported nbd command {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub command: Command,
    pub handle: [u8; 8],
    pub offset: u64,
    pub length: u32,
}

impl Request {
    pub fn decode(bytes: &[u8; REQUEST_SIZE]) -> Result<Self, String> {
        let magic = u32::from_be_bytes(field(bytes, 0));
        if magic != REQUEST_MAGIC {
            return Err(format!("bad nbd request magic {magic:#x}"));
        }
        Ok(Self {
            command: Command::from_code(u16::from_be_bytes(field(bytes, 6)))?,
            handle: field(bytes, 8),
            offset: u64::from_be_bytes(field(bytes, 16)),
            length: u32::from_be_bytes(field(bytes, 24)),
        })
    }

    pub fn encode(&self) -> [u8; REQUEST_SIZE] {
        let parts: [&[u8]; 6] = [
            &REQUEST_MAGIC.to_be_bytes(),
            &0u16.to_be_bytes(),
            &self.command.code().to_be_bytes(),
            &self.handle,
            &self.offset.to_be_bytes(),
            &self.length.to_be_bytes(),
        ];
        parts.concat().try_into().unwrap_or([0; REQUEST_SIZE])
    }
}

pub fn encode_reply(handle: [u8; 8], error: u32) -> [u8; REPLY_SIZE] {
    let parts: [&[u8]; 3] = [&REPLY_MAGIC.to_be_bytes(), &error.to_be_bytes(), &handle];
    parts.concat().try_into().unwrap_or([0; REPLY_SIZE])
}

fn field<const N: usize>(bytes: &[u8], start: usize) -> [u8; N] {
    bytes[start..start + N].try_into().unwrap_or([0; N])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(parts: &[&[u8]]) -> [u8; REQUEST_SIZE] {
        parts.concat().try_into().unwrap()
    }

    #[test]
    fn decodes_a_big_endian_request() {
        let raw = bytes(&[
            &0x2560_9513u32.to_be_bytes(),
            &0u16.to_be_bytes(),
            &1u16.to_be_bytes(),
            &[7; 8],
            &8192u64.to_be_bytes(),
            &4096u32.to_be_bytes(),
        ]);
        let request = Request::decode(&raw).unwrap();
        assert_eq!(
            request,
            Request {
                command: Command::Write,
                handle: [7; 8],
                offset: 8192,
                length: 4096,
            }
        );
        assert_eq!(request.encode(), raw);
    }

    #[test]
    fn rejects_bad_magic_and_unknown_commands() {
        let bad_magic = bytes(&[&[0; 4], &[0; 2], &[0; 2], &[0; 8], &[0; 8], &[0; 4]]);
        assert_eq!(Request::decode(&bad_magic).unwrap_err(), "bad nbd request magic 0x0");

        let unknown = bytes(&[
            &0x2560_9513u32.to_be_bytes(),
            &[0; 2],
            &5u16.to_be_bytes(),
            &[0; 8],
            &[0; 8],
            &[0; 4],
        ]);
        assert_eq!(Request::decode(&unknown).unwrap_err(), "unsupported nbd command 5");
    }

    #[test]
    fn encodes_a_reply() {
        let reply = encode_reply([3; 8], BACKEND_ERROR);
        assert_eq!(reply[..4], 0x6744_6698u32.to_be_bytes());
        assert_eq!(reply[4..8], 1u32.to_be_bytes());
        assert_eq!(reply[8..], [3; 8]);
    }
}
