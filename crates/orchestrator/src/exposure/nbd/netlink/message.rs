use netlink_packet_core::{
    DecodeError, DefaultNla, Emitable, ErrorContext, Nla, NlaBuffer, NlasIterator, Parseable, ParseableParametrized,
    emit_u32, emit_u64, parse_u8, parse_u32,
};
use netlink_packet_generic::{GenlFamily, GenlHeader};

const FAMILY: &str = "nbd";

const ATTR_INDEX: u16 = 1;
const ATTR_SIZE_BYTES: u16 = 2;
const ATTR_BLOCK_SIZE_BYTES: u16 = 3;
const ATTR_TIMEOUT: u16 = 4;
const ATTR_SERVER_FLAGS: u16 = 5;
const ATTR_CLIENT_FLAGS: u16 = 6;
const ATTR_SOCKETS: u16 = 7;
const ATTR_DEAD_CONN_TIMEOUT: u16 = 8;
const ATTR_DEVICE_LIST: u16 = 9;

const SOCK_ITEM: u16 = 1;
const SOCK_FD: u16 = 1;
const DEVICE_ITEM: u16 = 1;
const DEVICE_INDEX: u16 = 1;
const DEVICE_CONNECTED: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NbdCommand {
    Connect = 1,
    Disconnect = 2,
    Status = 5,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NbdAttribute {
    Index(u32),
    SizeBytes(u64),
    BlockSizeBytes(u64),
    Timeout(u64),
    ServerFlags(u64),
    ClientFlags(u64),
    Sockets(Vec<u32>),
    DeadConnTimeout(u64),
    Devices(Vec<DeviceStatus>),
    Other(DefaultNla),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceStatus {
    pub index: u32,
    pub connected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NbdMessage {
    pub command: NbdCommand,
    pub attributes: Vec<NbdAttribute>,
}

struct U32Nla {
    kind: u16,
    value: u32,
}

struct NestedNla<T> {
    kind: u16,
    items: Vec<T>,
}

impl GenlFamily for NbdMessage {
    fn family_name() -> &'static str {
        FAMILY
    }

    fn command(&self) -> u8 {
        self.command as u8
    }

    fn version(&self) -> u8 {
        0
    }
}

impl Emitable for NbdMessage {
    fn buffer_len(&self) -> usize {
        self.attributes.as_slice().buffer_len()
    }

    fn emit(&self, buffer: &mut [u8]) {
        self.attributes.as_slice().emit(buffer)
    }
}

impl ParseableParametrized<[u8], GenlHeader> for NbdMessage {
    fn parse_with_param(buffer: &[u8], header: GenlHeader) -> Result<Self, DecodeError> {
        Ok(Self {
            command: NbdCommand::from_byte(header.cmd)?,
            attributes: parse_all(buffer, |nla| NbdAttribute::parse(&nla)).context("parse nbd attributes")?,
        })
    }
}

impl NbdCommand {
    fn from_byte(command: u8) -> Result<Self, DecodeError> {
        match command {
            1 => Ok(Self::Connect),
            2 => Ok(Self::Disconnect),
            5 => Ok(Self::Status),
            other => Err(DecodeError::from(format!("unknown nbd command {other}"))),
        }
    }
}

impl NbdAttribute {
    fn nested_sockets(fds: &[u32]) -> NestedNla<NestedNla<U32Nla>> {
        NestedNla {
            kind: ATTR_SOCKETS,
            items: fds
                .iter()
                .map(|fd| NestedNla {
                    kind: SOCK_ITEM,
                    items: vec![U32Nla {
                        kind: SOCK_FD,
                        value: *fd,
                    }],
                })
                .collect(),
        }
    }
}

impl Nla for NbdAttribute {
    fn value_len(&self) -> usize {
        match self {
            Self::Index(_) => 4,
            Self::SizeBytes(_)
            | Self::BlockSizeBytes(_)
            | Self::Timeout(_)
            | Self::ServerFlags(_)
            | Self::ClientFlags(_)
            | Self::DeadConnTimeout(_) => 8,
            Self::Sockets(fds) => Self::nested_sockets(fds).value_len(),
            Self::Devices(_) => 0,
            Self::Other(nla) => nla.value_len(),
        }
    }

    fn kind(&self) -> u16 {
        match self {
            Self::Index(_) => ATTR_INDEX,
            Self::SizeBytes(_) => ATTR_SIZE_BYTES,
            Self::BlockSizeBytes(_) => ATTR_BLOCK_SIZE_BYTES,
            Self::Timeout(_) => ATTR_TIMEOUT,
            Self::ServerFlags(_) => ATTR_SERVER_FLAGS,
            Self::ClientFlags(_) => ATTR_CLIENT_FLAGS,
            Self::Sockets(_) => ATTR_SOCKETS,
            Self::DeadConnTimeout(_) => ATTR_DEAD_CONN_TIMEOUT,
            Self::Devices(_) => ATTR_DEVICE_LIST,
            Self::Other(nla) => nla.kind(),
        }
    }

    fn emit_value(&self, buffer: &mut [u8]) {
        match self {
            Self::Index(value) => emit_u32(buffer, *value).unwrap_or_default(),
            Self::SizeBytes(value)
            | Self::BlockSizeBytes(value)
            | Self::Timeout(value)
            | Self::ServerFlags(value)
            | Self::ClientFlags(value)
            | Self::DeadConnTimeout(value) => emit_u64(buffer, *value).unwrap_or_default(),
            Self::Sockets(fds) => Self::nested_sockets(fds).emit_value(buffer),
            Self::Devices(_) => {}
            Self::Other(nla) => nla.emit_value(buffer),
        }
    }
}

impl<T: AsRef<[u8]> + ?Sized> Parseable<NlaBuffer<&T>> for NbdAttribute {
    fn parse(nla: &NlaBuffer<&T>) -> Result<Self, DecodeError> {
        let value = nla.value();
        Ok(match nla.kind() {
            ATTR_INDEX => Self::Index(parse_u32(value).context("parse nbd index")?),
            ATTR_DEVICE_LIST => Self::Devices(parse_all(value, DeviceStatus::parse).context("parse nbd device list")?),
            _ => Self::Other(DefaultNla::parse(nla)?),
        })
    }
}

impl DeviceStatus {
    fn parse(item: NlaBuffer<&[u8]>) -> Result<Self, DecodeError> {
        if item.kind() != DEVICE_ITEM {
            return Err(DecodeError::from(format!(
                "unexpected nbd device list item {}",
                item.kind()
            )));
        }
        let mut status = Self {
            index: u32::MAX,
            connected: false,
        };
        for nla in NlasIterator::new(item.value()) {
            let nla = nla?;
            match nla.kind() {
                DEVICE_INDEX => status.index = parse_u32(nla.value()).context("parse nbd device index")?,
                DEVICE_CONNECTED => status.connected = parse_u8(nla.value()).context("parse nbd connected")? != 0,
                _ => {}
            }
        }
        Ok(status)
    }
}

impl Nla for U32Nla {
    fn value_len(&self) -> usize {
        4
    }

    fn kind(&self) -> u16 {
        self.kind
    }

    fn emit_value(&self, buffer: &mut [u8]) {
        emit_u32(buffer, self.value).unwrap_or_default()
    }
}

impl<T: Nla> Nla for NestedNla<T> {
    fn value_len(&self) -> usize {
        self.items.as_slice().buffer_len()
    }

    fn kind(&self) -> u16 {
        self.kind
    }

    fn emit_value(&self, buffer: &mut [u8]) {
        self.items.as_slice().emit(buffer)
    }
}

fn parse_all<T>(
    buffer: &[u8],
    parse: impl Fn(NlaBuffer<&[u8]>) -> Result<T, DecodeError>,
) -> Result<Vec<T>, DecodeError> {
    NlasIterator::new(buffer).map(|nla| nla.and_then(&parse)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emitted(attributes: Vec<NbdAttribute>) -> Vec<u8> {
        let message = NbdMessage {
            command: NbdCommand::Connect,
            attributes,
        };
        let mut buffer = vec![0; message.buffer_len()];
        message.emit(&mut buffer);
        buffer
    }

    #[test]
    fn sockets_nest_each_fd_in_its_own_item_like_nbdnl() {
        let bytes = emitted(vec![NbdAttribute::Sockets(vec![7, 9])]);

        assert_eq!(
            bytes,
            [
                [28, 0, 7, 0].as_slice(),
                &[12, 0, 1, 0, 8, 0, 1, 0, 7, 0, 0, 0],
                &[12, 0, 1, 0, 8, 0, 1, 0, 9, 0, 0, 0],
            ]
            .concat()
        );
    }

    #[test]
    fn sizes_and_timeouts_are_u64_and_the_index_is_u32() {
        let bytes = emitted(vec![NbdAttribute::Index(3), NbdAttribute::Timeout(90)]);

        assert_eq!(
            bytes,
            [
                [8, 0, 1, 0, 3, 0, 0, 0].as_slice(),
                &[12, 0, 4, 0, 90, 0, 0, 0, 0, 0, 0, 0]
            ]
            .concat()
        );
    }

    #[test]
    fn a_status_reply_lists_devices_with_their_connection_state() {
        let reply = [
            [32, 0, 9, 0].as_slice(),
            &[
                28, 0, 1, 0, 8, 0, 1, 0, 4, 0, 0, 0, 5, 0, 2, 0, 1, 0, 0, 0, 8, 0, 99, 0, 0, 0, 0, 0,
            ],
        ]
        .concat();
        let header = GenlHeader {
            cmd: NbdCommand::Status as u8,
            version: 0,
        };

        let message = NbdMessage::parse_with_param(&reply, header).unwrap();

        assert_eq!(
            message.attributes,
            vec![NbdAttribute::Devices(vec![DeviceStatus {
                index: 4,
                connected: true
            }])]
        );
    }
}
