mod message;

use std::io;
use std::time::Duration;

use futures::{Stream, StreamExt};
use genetlink::GenetlinkHandle;
use netlink_packet_core::{DecodeError, NLM_F_ACK, NLM_F_REQUEST, NetlinkMessage, NetlinkPayload};
use netlink_packet_generic::GenlMessage;

use self::message::{NbdAttribute, NbdCommand, NbdMessage};

const FLAG_HAS_FLAGS: u64 = 1 << 0;
const FLAG_SEND_TRIM: u64 = 1 << 5;
const FLAG_SEND_WRITE_ZEROES: u64 = 1 << 6;
const FLAG_CAN_MULTICONN: u64 = 1 << 8;
const SERVER_FLAGS: u64 = FLAG_HAS_FLAGS | FLAG_CAN_MULTICONN | FLAG_SEND_TRIM | FLAG_SEND_WRITE_ZEROES;

#[derive(Clone)]
pub struct NbdNetlink {
    handle: GenetlinkHandle,
}

pub struct Connect<'a> {
    pub index: u32,
    pub sockets: &'a [u32],
    pub size: u64,
    pub block_size: u64,
    pub timeout: Duration,
    pub dead_connection_timeout: Duration,
}

impl NbdNetlink {
    pub fn open() -> io::Result<Self> {
        let (connection, handle, _) = genetlink::new_connection()?;
        tokio::spawn(connection);
        Ok(Self { handle })
    }

    pub async fn connect(&self, request: &Connect<'_>) -> io::Result<u32> {
        let attributes = vec![
            NbdAttribute::Index(request.index),
            NbdAttribute::SizeBytes(request.size),
            NbdAttribute::BlockSizeBytes(request.block_size),
            NbdAttribute::Timeout(request.timeout.as_secs()),
            NbdAttribute::DeadConnTimeout(request.dead_connection_timeout.as_secs()),
            NbdAttribute::Sockets(request.sockets.to_vec()),
            NbdAttribute::ClientFlags(0),
            NbdAttribute::ServerFlags(SERVER_FLAGS),
        ];
        self.ask(NbdCommand::Connect, attributes, NLM_F_REQUEST)
            .await?
            .find_map(|attribute| match attribute {
                NbdAttribute::Index(index) => Some(index),
                _ => None,
            })
            .ok_or_else(|| io::Error::other("nbd connect reply has no device index"))
    }

    pub async fn is_connected(&self, index: u32) -> io::Result<bool> {
        Ok(self
            .ask(NbdCommand::Status, vec![NbdAttribute::Index(index)], NLM_F_REQUEST)
            .await?
            .flat_map(|attribute| match attribute {
                NbdAttribute::Devices(devices) => devices,
                _ => Vec::new(),
            })
            .any(|device| device.index == index && device.connected))
    }

    pub async fn disconnect(&self, index: u32) -> io::Result<()> {
        self.ask(
            NbdCommand::Disconnect,
            vec![NbdAttribute::Index(index)],
            NLM_F_REQUEST | NLM_F_ACK,
        )
        .await
        .map(drop)
    }

    async fn ask(
        &self,
        command: NbdCommand,
        attributes: Vec<NbdAttribute>,
        flags: u16,
    ) -> io::Result<impl Iterator<Item = NbdAttribute>> {
        let mut message = NetlinkMessage::from(GenlMessage::from_payload(NbdMessage { command, attributes }));
        message.header.flags = flags;
        let replies = self.handle.clone().request(message).await.map_err(io::Error::other)?;
        let attributes = collect_attributes(replies).await?;
        Ok(attributes.into_iter())
    }
}

async fn collect_attributes(
    replies: impl Stream<Item = Result<NetlinkMessage<GenlMessage<NbdMessage>>, DecodeError>>,
) -> io::Result<Vec<NbdAttribute>> {
    let mut attributes = Vec::new();
    futures::pin_mut!(replies);
    while let Some(reply) = replies.next().await {
        match reply.map_err(io::Error::other)?.payload {
            NetlinkPayload::InnerMessage(message) => attributes.extend(message.payload.attributes),
            NetlinkPayload::Error(error) if error.code.is_some() => return Err(error.to_io()),
            _ => {}
        }
    }
    Ok(attributes)
}
