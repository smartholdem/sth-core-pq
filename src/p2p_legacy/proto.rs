//! Author: TechnoL0g
//!
//! Hand-written prost messages of the legacy P2P codecs (`p2p.peer.*`, `p2p.blocks.*`,
//! `p2p.transactions.*`). Tags mirror the original `.proto` files, so no protoc is needed.

use prost::Message;

#[derive(Clone, PartialEq, Message)]
pub struct Headers {
    #[prost(string, tag = "1")]
    pub version: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetStatusRequest {
    #[prost(message, optional, tag = "1")]
    pub headers: Option<Headers>,
}

#[derive(Clone, PartialEq, Message)]
pub struct StatusBlockHeader {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(uint32, tag = "7")]
    pub height: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct StatusState {
    #[prost(uint32, tag = "1")]
    pub height: u32,
    #[prost(bool, tag = "2")]
    pub forging_allowed: bool,
    #[prost(uint32, tag = "3")]
    pub current_slot: u32,
    #[prost(message, optional, tag = "4")]
    pub header: Option<StatusBlockHeader>,
}

#[derive(Clone, PartialEq, Message)]
pub struct StatusNetwork {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub nethash: String,
    #[prost(uint32, tag = "5")]
    pub version: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct StatusConfig {
    #[prost(string, tag = "1")]
    pub version: String,
    #[prost(message, optional, tag = "2")]
    pub network: Option<StatusNetwork>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetStatusResponse {
    #[prost(message, optional, tag = "1")]
    pub state: Option<StatusState>,
    #[prost(message, optional, tag = "2")]
    pub config: Option<StatusConfig>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetPeersRequest {
    #[prost(message, optional, tag = "1")]
    pub headers: Option<Headers>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PeerInfo {
    #[prost(string, tag = "1")]
    pub ip: String,
    #[prost(uint32, tag = "2")]
    pub port: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetPeersResponse {
    #[prost(message, repeated, tag = "1")]
    pub peers: Vec<PeerInfo>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetBlocksRequest {
    #[prost(uint32, tag = "1")]
    pub last_block_height: u32,
    #[prost(uint32, tag = "2")]
    pub block_limit: u32,
    #[prost(bool, tag = "3")]
    pub headers_only: bool,
    #[prost(bool, tag = "4")]
    pub serialized: bool,
    #[prost(message, optional, tag = "5")]
    pub headers: Option<Headers>,
}

#[derive(Clone, PartialEq, Message)]
pub struct BlockHeaderProto {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(uint32, tag = "3")]
    pub version: u32,
    #[prost(uint32, tag = "4")]
    pub timestamp: u32,
    #[prost(string, tag = "5")]
    pub previous_block: String,
    #[prost(uint32, tag = "7")]
    pub height: u32,
    #[prost(uint32, tag = "8")]
    pub number_of_transactions: u32,
    #[prost(string, tag = "9")]
    pub total_amount: String,
    #[prost(string, tag = "10")]
    pub total_fee: String,
    #[prost(string, tag = "11")]
    pub reward: String,
    #[prost(uint32, tag = "12")]
    pub payload_length: u32,
    #[prost(string, tag = "13")]
    pub payload_hash: String,
    #[prost(string, tag = "14")]
    pub generator_public_key: String,
    #[prost(string, tag = "15")]
    pub block_signature: String,
    #[prost(bytes = "vec", tag = "16")]
    pub transactions: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PostTransactionsRequest {
    #[prost(bytes = "vec", repeated, tag = "1")]
    pub transactions: Vec<Vec<u8>>,
    #[prost(message, optional, tag = "2")]
    pub headers: Option<Headers>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PostBlockRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub block: Vec<u8>,
    #[prost(message, optional, tag = "2")]
    pub headers: Option<Headers>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PostBlockResponse {
    #[prost(bool, tag = "1")]
    pub status: bool,
    #[prost(uint32, tag = "2")]
    pub height: u32,
}

pub(super) fn headers() -> Option<Headers> {
    Some(Headers { version: super::PEER_VERSION.to_string() })
}
