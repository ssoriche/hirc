//! Reading CRLF lines off a socket. Bounded (an endless line can't eat
//! memory) and cancel-safe (a half-read line survives `select!`).

use crate::proto::MAX_LINE;
use std::io;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

pub struct LineReader<R> {
    inner: R,
    buf: Vec<u8>,
    overflow: bool,
}

impl<R: AsyncBufRead + Unpin> LineReader<R> {
    pub fn new(inner: R) -> LineReader<R> {
        LineReader { inner, buf: Vec::new(), overflow: false }
    }

    /// The next line including its line ending, `""` for a line longer than
    /// `MAX_LINE`, or `None` at end of stream. Bytes that aren't UTF-8
    /// become U+FFFD.
    pub async fn next_line(&mut self) -> io::Result<Option<String>> {
        loop {
            let avail = self.inner.fill_buf().await?;
            if avail.is_empty() {
                return Ok(None);
            }
            let (n, done) = match avail.iter().position(|&b| b == b'\n') {
                Some(i) => (i + 1, true),
                None => (avail.len(), false),
            };
            if self.buf.len() + n <= MAX_LINE + 2 {
                self.buf.extend_from_slice(&avail[..n]);
            } else {
                self.overflow = true;
            }
            self.inner.consume(n);
            if done {
                let line = if self.overflow { String::new() } else { String::from_utf8_lossy(&self.buf).into_owned() };
                self.buf.clear();
                self.overflow = false;
                return Ok(Some(line));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn reads_lines_and_handles_invalid_utf8_and_overflow() {
        let mut data = b"PING :a\r\n".to_vec();
        data.extend_from_slice(b"bad \xff byte\n");
        data.extend(std::iter::repeat_n(b'x', crate::proto::MAX_LINE + 10));
        data.extend_from_slice(b"\r\nlast");
        let mut r = LineReader::new(BufReader::with_capacity(16, &data[..]));
        assert_eq!(r.next_line().await.unwrap().as_deref(), Some("PING :a\r\n"));
        assert_eq!(r.next_line().await.unwrap().as_deref(), Some("bad \u{fffd} byte\n"));
        assert_eq!(r.next_line().await.unwrap().as_deref(), Some(""));
        // A final line without a newline is dropped when the stream ends.
        assert_eq!(r.next_line().await.unwrap(), None);
    }
}
