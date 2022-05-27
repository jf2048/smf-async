mod read;
pub use read::*;
mod write;
pub use write::*;

#[derive(Copy, Clone, Debug, Hash, Ord, PartialOrd, Eq, PartialEq)]
pub enum Format {
    Single,
    Multiple,
    Sequential,
}

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub enum Division {
    PPQN(u16),
    SMPTE { fps: u8, tpf: u8 },
}
