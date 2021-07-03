use std::net::SocketAddr;

use crate::sha1w::ISha1;
use log::debug;
use parking_lot::{Mutex, RwLock};
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    buffers::{ByteBuf, ByteString},
    constants::CHUNK_SIZE,
    lengths::{ceil_div_u64, last_element_size_u64, ChunkInfo},
    peer_binary_protocol::{
        extended::{handshake::ExtendedHandshake, ut_metadata::UtMetadata, ExtendedMessage},
        Handshake, Message,
    },
    peer_connection::{PeerConnection, PeerConnectionHandler, WriterRequest},
    serde_bencode_de::from_bytes,
    torrent_metainfo::TorrentMetaV1Info,
    type_aliases::Sha1,
};

pub async fn read_metainfo_from_peer(
    addr: SocketAddr,
    peer_id: [u8; 20],
    info_hash: [u8; 20],
) -> anyhow::Result<TorrentMetaV1Info<ByteString>> {
    let (result_tx, result_rx) =
        tokio::sync::oneshot::channel::<anyhow::Result<TorrentMetaV1Info<ByteString>>>();
    let (writer_tx, writer_rx) = tokio::sync::mpsc::unbounded_channel::<WriterRequest>();
    let handler = Handler {
        addr,
        info_hash,
        writer_tx,
        result_tx: Mutex::new(Some(result_tx)),
        locked: RwLock::new(None),
    };
    let connection = PeerConnection::new(addr, info_hash, peer_id, handler);

    let result_reader = async move { result_rx.await? };
    let connection_runner = async move { connection.manage_peer(writer_rx).await };

    tokio::select! {
        result = result_reader => result,
        whatever = connection_runner => match whatever {
            Ok(_) => anyhow::bail!("connection runner completed first"),
            Err(e) => Err(e)
        }
    }
}

#[derive(Default)]
struct HandlerLocked {
    metadata_size: u32,
    total_pieces: usize,
    buffer: Vec<u8>,
    received_pieces: Vec<bool>,
}

impl HandlerLocked {
    fn new(metadata_size: u32) -> anyhow::Result<Self> {
        if metadata_size > 1024 * 1024 {
            anyhow::bail!("metadata size {} is too big", metadata_size);
        }
        let buffer = vec![0u8; metadata_size as usize];
        let total_pieces = ceil_div_u64(metadata_size as u64, CHUNK_SIZE as u64);
        let received_pieces = vec![false; total_pieces as usize];
        Ok(Self {
            metadata_size,
            received_pieces,
            buffer,
            total_pieces: total_pieces as usize,
        })
    }
    fn piece_size(&self, index: u32) -> usize {
        if index as usize == self.total_pieces - 1 {
            last_element_size_u64(self.metadata_size as u64, CHUNK_SIZE as u64) as usize
        } else {
            CHUNK_SIZE as usize
        }
    }
    fn record_piece(
        &mut self,
        index: u32,
        data: &[u8],
        info_hash: [u8; 20],
    ) -> anyhow::Result<bool> {
        if index as usize >= self.total_pieces {
            anyhow::bail!("wrong index");
        }
        let offset = (index * CHUNK_SIZE) as usize;
        let size = self.piece_size(index);
        if data.len() != size {
            anyhow::bail!(
                "expected length of piece {} to be {}, but got {}",
                index,
                size,
                data.len()
            );
        }
        if self.received_pieces[index as usize] {
            anyhow::bail!("already received piece {}", index);
        }
        let offset_end = offset + size;
        (&mut self.buffer[offset..offset_end]).copy_from_slice(data);
        self.received_pieces[index as usize] = true;

        if self.received_pieces.iter().all(|p| *p) {
            // check metadata
            let mut hash = Sha1::new();
            hash.update(&self.buffer);
            if hash.finish() != info_hash {
                anyhow::bail!("info checksum invalid");
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

struct Handler {
    addr: SocketAddr,
    info_hash: [u8; 20],
    writer_tx: UnboundedSender<WriterRequest>,
    result_tx:
        Mutex<Option<tokio::sync::oneshot::Sender<anyhow::Result<TorrentMetaV1Info<ByteString>>>>>,
    locked: RwLock<Option<HandlerLocked>>,
}

impl PeerConnectionHandler for Handler {
    fn get_have_bytes(&self) -> u64 {
        0
    }

    fn serialize_bitfield_message_to_buf(&self, _buf: &mut Vec<u8>) -> Option<usize> {
        None
    }

    fn on_handshake(&self, handshake: Handshake) -> anyhow::Result<()> {
        if !handshake.supports_extended() {
            anyhow::bail!("this peer does not support extended handshaking, which is a prerequisite to download metadata")
        }
        Ok(())
    }

    fn on_received_message(&self, msg: Message<ByteBuf<'_>>) -> anyhow::Result<()> {
        debug!("{}: received message: {:?}", self.addr, msg);

        if let Message::Extended(ExtendedMessage::UtMetadata(UtMetadata::Data {
            piece,
            total_size: _,
            data,
        })) = msg
        {
            let piece_ready =
                self.locked
                    .write()
                    .as_mut()
                    .unwrap()
                    .record_piece(piece, &data, self.info_hash)?;
            if piece_ready {
                let buf = self.locked.write().take().unwrap().buffer;
                let info = from_bytes::<TorrentMetaV1Info<ByteString>>(&buf);
                self.result_tx
                    .lock()
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("oneshot is consumed"))?
                    .send(info)
                    .map_err(|_| {
                        anyhow::anyhow!("torrent info deserialized, but consumer closed")
                    })?;
            }
        }
        Ok(())
    }

    fn on_uploaded_bytes(&self, _bytes: u32) {}

    fn read_chunk(&self, _chunk: &ChunkInfo, _buf: &mut [u8]) -> anyhow::Result<()> {
        anyhow::bail!("the peer is not supposed to be requesting chunks")
    }

    fn on_extended_handshake(
        &self,
        extended_handshake: &ExtendedHandshake<ByteBuf>,
    ) -> anyhow::Result<()> {
        let metadata_size = match extended_handshake.metadata_size {
            Some(metadata_size) => metadata_size,
            None => anyhow::bail!("peer does not have metadata_size"),
        };

        if extended_handshake.get_msgid(b"ut_metadata").is_none() {
            anyhow::bail!("peer does not support ut_metadata");
        }

        self.writer_tx
            .send(WriterRequest::Message(Message::Unchoke))?;
        self.writer_tx
            .send(WriterRequest::Message(Message::Interested))?;

        let inner = HandlerLocked::new(metadata_size)?;
        let total_pieces = inner.total_pieces;

        self.locked.write().replace(inner);

        for i in 0..total_pieces {
            self.writer_tx
                .send(WriterRequest::Message(Message::Extended(
                    ExtendedMessage::UtMetadata(UtMetadata::Request(i as u32)),
                )))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, str::FromStr, sync::Once};

    use crate::peer_id::generate_peer_id;

    use super::read_metainfo_from_peer;

    static LOG_INIT: Once = std::sync::Once::new();

    fn init_logging() {
        LOG_INIT.call_once(pretty_env_logger::init)
    }

    fn decode_info_hash(hash_str: &str) -> [u8; 20] {
        let mut hash_arr = [0u8; 20];
        hex::decode_to_slice(hash_str, &mut hash_arr).unwrap();
        hash_arr
    }

    #[tokio::test]
    async fn test_get_torrent_metadata_from_localhost_bittorrent_client() {
        init_logging();

        let addr = SocketAddr::from_str("127.0.0.1:27311").unwrap();
        let peer_id = generate_peer_id();
        let info_hash = decode_info_hash("9905f844e5d8787ecd5e08fb46b2eb0a42c131d7");
        dbg!(read_metainfo_from_peer(addr, peer_id, info_hash)
            .await
            .unwrap());
    }
}