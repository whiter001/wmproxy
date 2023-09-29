use webparse::{Buf, BufMut};

use crate::{
    prot::{ProtFlag, ProtKind},
    ProxyResult,
};

use super::ProtFrameHeader;

/// 旧的Socket连接关闭, 接收到则关闭掉当前的连接
#[derive(Debug)]
pub struct ProtClose {
    sock_map: u32,
}

impl ProtClose {
    pub fn new(sock_map: u32) -> ProtClose {
        ProtClose { sock_map }
    }

    pub fn parse<T: Buf>(header: ProtFrameHeader, _buf: T) -> ProxyResult<ProtClose> {
        // let _mode = buf.get_u8();
        Ok(ProtClose {
            sock_map: header.sock_map(),
        })
    }

    pub fn encode<B: Buf + BufMut>(self, buf: &mut B) -> ProxyResult<usize> {
        let mut head = ProtFrameHeader::new(ProtKind::Close, ProtFlag::zero(), self.sock_map);
        head.length = 0;
        let mut size = 0;
        size += head.encode(buf)?;
        Ok(size)
    }

    pub fn sock_map(&self) -> u32 {
        self.sock_map
    }
}