//! Peer ID 黑名单模块（`peer-id-blacklist`）。

use super::string_blacklist::StringBlacklist;

/// PeerId 黑名单（具体逻辑见 [`StringBlacklist`]）。
pub type PeerIdBlacklist = StringBlacklist;

pub fn new() -> StringBlacklist {
    StringBlacklist::peer_id()
}
