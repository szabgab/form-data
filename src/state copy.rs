use std::{
    fmt,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use anyhow::{Error, Result};
use bytes::{Buf, Bytes, BytesMut};
use futures_util::stream::Stream;
use memchr::memmem;

use crate::utils::{CR, CRLF, CRLFS, CRLF_DASHES, DASH, DEFAULT_BUF_SIZE, LF};

#[derive(Debug, PartialEq)]
enum Flag {
    Header,
    Body,
}

struct Cursor {
    crlf_d_b_crlf: Vec<u8>,
    crlf_d_b_d_crlf: Vec<u8>,
}

impl Cursor {
    pub(crate) fn new(boundary: Vec<u8>) -> Self {
        // `\r\n--boundary\r\n`
        let mut crlf_d_b_crlf = boundary.clone();
        crlf_d_b_crlf.insert(0, DASH);
        crlf_d_b_crlf.insert(0, DASH);
        crlf_d_b_crlf.insert(0, LF);
        crlf_d_b_crlf.insert(0, CR);

        // `\r\n--boundary--\r\n`
        let mut crlf_d_b_d_crlf = crlf_d_b_crlf.clone();

        crlf_d_b_crlf.push(CR);
        crlf_d_b_crlf.push(LF);

        crlf_d_b_d_crlf.push(DASH);
        crlf_d_b_d_crlf.push(DASH);
        crlf_d_b_d_crlf.push(CR);
        crlf_d_b_d_crlf.push(LF);

        Self {
            crlf_d_b_crlf,
            crlf_d_b_d_crlf,
        }
    }
}

impl fmt::Debug for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cursor")
            .field(
                "crlf_dash_boundary_crlf",
                &String::from_utf8_lossy(&self.crlf_d_b_crlf),
            )
            .field("crlf_dash_boundary_crlf_len", &self.crlf_d_b_crlf.len())
            .field(
                "crlf_dash_boundary_dash_crlf",
                &String::from_utf8_lossy(&self.crlf_d_b_d_crlf),
            )
            .field(
                "crlf_dash_boundary_dash_crlf_len",
                &self.crlf_d_b_d_crlf.len(),
            )
            .finish()
    }
}

/// IO State
pub struct State<T> {
    io: T,
    eof: bool,
    length: u64,
    total: usize,
    cursor: Cursor,
    boundary: Vec<u8>,
    waker: Option<Waker>,
    buffer: BytesMut,
    max_buf_size: usize,
    /// parse flags
    flag: Flag,
    x: Option<usize>,
    y: Option<usize>,
    z: bool,
}

impl<T> State<T> {
    /// Creates new State.
    pub fn new<B: AsRef<[u8]>>(b: B, io: T) -> Self {
        // `boundary`
        let boundary = b.as_ref().to_owned();
        let cursor = Cursor::new(boundary.to_owned());

        Self {
            io,
            cursor,
            boundary,
            total: 0,
            length: 0,
            eof: false,
            waker: None,
            // placeholder `\r\n` , let first boundary is `\r\n--boundary`
            buffer: BytesMut::from(&CRLF[..]),
            max_buf_size: DEFAULT_BUF_SIZE,
            flag: Flag::Body,
            x: None,
            y: None,
            z: false,
        }
    }

    /// Sets max buffer size.
    pub fn set_max_buf_size(&mut self, max: usize) {
        assert!(
            max >= DEFAULT_BUF_SIZE,
            "The max_buf_size cannot be smaller than {}.",
            DEFAULT_BUF_SIZE,
        );
        self.max_buf_size = max;
    }

    /// Gets io.
    pub fn io_mut(&mut self) -> &mut T {
        &mut self.io
    }

    /// Gets waker.
    pub fn waker(&self) -> Option<&Waker> {
        self.waker.as_ref()
    }

    /// Gets waker.
    pub fn waker_mut(&mut self) -> &mut Option<Waker> {
        &mut self.waker
    }

    /// Splits buffer.
    pub fn split_buffer(&mut self, n: usize) -> Bytes {
        self.buffer.split_to(n).freeze()
    }

    /// Gets the index of the field.
    pub fn index(&mut self) -> usize {
        let index = self.total;
        self.total += 1;
        index
    }

    /// Gets the length of the form-data.
    pub fn len(&self) -> u64 {
        self.length
    }

    /// Gets EOF.
    pub fn eof(&self) -> bool {
        self.eof
    }

    /// Counts the fields.
    pub fn total(&self) -> usize {
        self.total
    }

    /// `boundary`
    fn boundary(&self) -> &[u8] {
        &self.boundary
    }

    /// `\r\n--boundary\r\n`
    fn crlf_d_b_crlf(&self) -> &[u8] {
        &self.cursor.crlf_d_b_crlf
    }

    /// 6: `\r\n--\r\n`
    fn crlf_d_b_crlf_len(&self) -> usize {
        // self.boundary.len() + 2 + 2 + 2
        self.cursor.crlf_d_b_crlf.len()
    }

    /// `\r\n--boundary--\r\n`
    fn crlf_d_b_d_crlf(&self) -> &[u8] {
        &self.cursor.crlf_d_b_d_crlf
    }

    /// 8: `\r\n----\r\n`
    fn crlf_d_b_d_crlf_len(&self) -> usize {
        // self.boundary.len() + 2 + 2 + 2 + 2
        self.cursor.crlf_d_b_d_crlf.len()
    }
}

impl<T> fmt::Debug for State<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("State")
            .field("boundary", &String::from_utf8_lossy(self.boundary()))
            .field("eof", &self.eof)
            .field("length", &self.length)
            .field("total", &self.total)
            .field("cursor", &self.cursor)
            .finish()
    }
}

impl<T, E> Stream for State<T>
where
    T: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<Error>,
{
    // 0 is EOF!
    // First: if found a boundary then returns size of headers to `Form`
    // Second: returns of payload data to `Field`
    type Item = Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // `\r\n--boundary\r\n` or // `\r\n--boundary--`
        let min_size = 2 + 2 + self.boundary.len() + 2;
        let max_buf_size = self.max_buf_size;

        loop {
            if self.eof {
                return Poll::Ready(None);
            }

            if Flag::Body == self.flag {
                // `\r\n--`
                if self.x == None {
                    self.x = memmem::find(&self.buffer, &CRLF_DASHES);
                }

                if let Some(mut x) = self.x {
                    // we dont found first part, so need to consume data
                    if self.total == 0 && x > 0 {
                        self.buffer.advance(x);
                        x = 0;
                        self.x.replace(x);
                    }

                    // `\r\n--boundary\r\n`
                    if self.y == None {
                        self.y = memmem::find(&self.buffer, self.crlf_d_b_crlf());
                    }

                    // found new part
                    if let Some(mut y) = self.y {
                        // Buffer size is limited by 8KB.
                        // So we need do that for large data.
                        if y < max_buf_size {
                            self.x = None;
                            self.flag = Flag::Header;
                        }

                        // has previous part
                        if self.total > 0 {
                            // previous part is end
                            if y == 0 {
                                return Poll::Ready(None);
                            }

                            // Buffer size is limited by 8KB.
                            // So we need do that for large data.
                            let n = if y < max_buf_size {
                                self.z = true;
                                self.y = None;
                                y
                            } else {
                                y -= max_buf_size;
                                self.y.replace(y);
                                max_buf_size
                            };

                            return Poll::Ready(Some(Ok(self.split_buffer(n))));
                        }
                    }

                    if Flag::Body == self.flag {
                        // keep consume data of current part
                        if x > 0 {
                            // Buffer size is limited by 8KB.
                            // So we need do that for large data.
                            let n = if x < max_buf_size {
                                self.x = None;
                                x
                            } else {
                                x -= max_buf_size;
                                self.x.replace(x);
                                max_buf_size
                            };

                            return Poll::Ready(Some(Ok(self.split_buffer(n))));
                        }

                        // payload data is end
                        if let Some(z) = memmem::find(&self.buffer, self.crlf_d_b_d_crlf()) {
                            self.eof = true;
                            self.x = None;
                            self.y = None;
                            self.flag = Flag::Body;

                            if z == 0 {
                                let n = self.crlf_d_b_d_crlf_len();
                                self.buffer.advance(n);
                                self.length -= self.buffer.len() as u64;
                                self.length -= 2; // --
                                self.buffer.clear();
                                return Poll::Ready(None);
                            } else {
                                // last data of last part
                                return Poll::Ready(Some(Ok(self.split_buffer(z))));
                            }
                        }
                    }
                } else {
                    // the large data of part
                    if self.total > 0 && self.buffer.len() > max_buf_size {
                        return Poll::Ready(Some(Ok(self.split_buffer(max_buf_size))));
                    }
                }
            }

            if Flag::Header == self.flag {
                // previous part is end
                if self.z {
                    self.z = false;
                    return Poll::Ready(None);
                }

                // found headers of part
                if let Some(h) = memmem::find(&self.buffer, &CRLFS) {
                    self.x = None;
                    self.y = None;
                    self.flag = Flag::Body;
                    return Poll::Ready(Some(Ok(self
                        .buffer
                        .split_to(h + 4)
                        .split_off(self.crlf_d_b_crlf_len())
                        .freeze())));
                }
            }

            match Pin::new(self.io_mut()).poll_next(cx) {
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e.into()))),
                Poll::Ready(Some(Ok(b))) => {
                    let l = b.len();
                    // @TODO: need check payload data length
                    self.length += l as u64;
                    self.buffer.extend_from_slice(&b);
                    tracing::trace!("polled bytes {}/{}/{}", l, self.buffer.len(), self.length);
                }
                Poll::Ready(None) => {
                    self.eof = true;
                    tracing::trace!("polled total bytes: {}", self.length);
                    return Poll::Ready(None);
                }
                Poll::Pending => {
                    tracing::trace!("polled pending");
                    return Poll::Pending;
                }
            }
        }
    }
}
