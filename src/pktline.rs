//! Git pkt-line framing: 4 hex digits of length, then payload. Lengths 0000, 0001 and
//! 0002 are the flush, delimiter and response-end markers.

use std::io::{self, BufRead, Write};

/// Largest pkt-line payload (65520 total minus the 4 length bytes).
pub const MAX_PAYLOAD: usize = 65516;

pub fn line(out: &mut Vec<u8>, payload: impl AsRef<[u8]>) {
    let p = payload.as_ref();
    out.extend_from_slice(format!("{:04x}", p.len() + 4).as_bytes());
    out.extend_from_slice(p);
}

pub fn flush(out: &mut Vec<u8>) {
    out.extend_from_slice(b"0000");
}

pub fn delim(out: &mut Vec<u8>) {
    out.extend_from_slice(b"0001");
}

/// Wrap bytes in side-band packets on `band` (1 data, 2 progress, 3 error).
pub fn sideband(out: &mut Vec<u8>, band: u8, mut data: &[u8]) {
    while !data.is_empty() {
        let n = data.len().min(MAX_PAYLOAD - 1);
        out.extend_from_slice(format!("{:04x}", n + 5).as_bytes());
        out.push(band);
        out.extend_from_slice(&data[..n]);
        data = &data[n..];
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Packet {
    Data(Vec<u8>),
    Flush,
    Delim,
    ResponseEnd,
}

/// Read one packet. Returns `None` at a clean end of stream.
pub fn read<R: BufRead + ?Sized>(r: &mut R) -> io::Result<Option<Packet>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = std::str::from_utf8(&len)
        .ok()
        .and_then(|s| usize::from_str_radix(s, 16).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad pkt-line length"))?;
    Ok(Some(match len {
        0 => Packet::Flush,
        1 => Packet::Delim,
        2 => Packet::ResponseEnd,
        3 => return Err(io::Error::new(io::ErrorKind::InvalidData, "bad pkt-line length 3")),
        n => {
            let mut buf = vec![0u8; n - 4];
            r.read_exact(&mut buf)?;
            Packet::Data(buf)
        }
    }))
}

/// Read packets until a flush (or delimiter), returning the payloads with any trailing
/// newline removed. The terminator that ended the section is returned too.
pub fn read_section<R: BufRead + ?Sized>(r: &mut R) -> io::Result<(Vec<String>, Option<Packet>)> {
    let mut lines = Vec::new();
    loop {
        match read(r)? {
            Some(Packet::Data(d)) => {
                let mut s = String::from_utf8_lossy(&d).into_owned();
                if s.ends_with('\n') {
                    s.pop();
                }
                lines.push(s);
            }
            other => return Ok((lines, other)),
        }
    }
}

/// A `Write` adapter that frames everything written to it as side-band 1 packets.
pub struct SidebandWriter<W: Write> {
    inner: W,
    buf: Vec<u8>,
}

impl<W: Write> SidebandWriter<W> {
    pub fn new(inner: W) -> Self {
        SidebandWriter { inner, buf: Vec::with_capacity(MAX_PAYLOAD) }
    }

    pub fn into_inner(mut self) -> io::Result<W> {
        self.flush()?;
        Ok(self.inner)
    }
}

impl<W: Write> Write for SidebandWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let room = MAX_PAYLOAD - 1 - self.buf.len();
        let n = data.len().min(room);
        self.buf.extend_from_slice(&data[..n]);
        if self.buf.len() == MAX_PAYLOAD - 1 {
            self.flush()?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            let mut framed = Vec::with_capacity(self.buf.len() + 5);
            sideband(&mut framed, 1, &self.buf);
            self.inner.write_all(&framed)?;
            self.buf.clear();
        }
        self.inner.flush()
    }
}
