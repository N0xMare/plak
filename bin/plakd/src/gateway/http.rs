//! The buffered call the dispatcher and its tests share. The listener's relay
//! server owns the HTTP connection; this is not a second codec.

use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub(crate) struct Request {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

pub(crate) struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    pub(crate) fn json(status: u16, body: Vec<u8>) -> Self {
        Self { status, body }
    }
}
