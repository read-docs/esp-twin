/// One Ethernet frame travelling through a zero-copy channel between the two network stacks.
pub struct Frame {
    pub data: [u8; Self::MAX_LEN],
    pub len: usize,
}

impl Frame {
    pub const MAX_LEN: usize = 1514;
    pub const EMPTY: Frame = Frame {
        data: [0; Self::MAX_LEN],
        len: 0,
    };
}
