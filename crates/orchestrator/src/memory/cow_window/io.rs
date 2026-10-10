use std::borrow::Cow;
use std::io;

pub trait PageSource: Send + Sync {
    fn read_page(&self, offset: u64, length: u64) -> io::Result<Cow<'_, [u8]>>;
}

pub trait PageSink: Send + Sync {
    fn write_page(&self, data: &[u8], offset: u64) -> io::Result<()>;
}
