//! Client Name 黑名单模块（`client-name-blacklist`）。

use super::string_blacklist::StringBlacklist;

/// ClientName 黑名单（具体逻辑见 [`StringBlacklist`]）。
pub type ClientNameBlacklist = StringBlacklist;

pub fn new() -> StringBlacklist {
    StringBlacklist::client_name()
}
