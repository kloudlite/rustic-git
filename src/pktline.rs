use std::io::{self, BufRead, Write};

pub enum Pkt {
    Data(Vec<u8>),
    Flush,
    Delim,
}

pub fn read_pkt(r: &mut dyn BufRead) -> io::Result<Option<Pkt>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let s = std::str::from_utf8(&len).map_err(|_| io::Error::other("bad pkt len"))?;
    let n = usize::from_str_radix(s, 16).map_err(|_| io::Error::other("bad pkt len"))?;
    Ok(Some(match n {
        0 => Pkt::Flush,
        1 => Pkt::Delim,
        2 | 3 => return Err(io::Error::other("bad pkt len")),
        _ => {
            let mut d = vec![0u8; n - 4];
            r.read_exact(&mut d)?;
            Pkt::Data(d)
        }
    }))
}

pub fn write_pkt(w: &mut dyn Write, data: &[u8]) -> io::Result<()> {
    if data.len() + 4 > 0xffff {
        return Err(io::Error::other("pkt too long"));
    }
    write!(w, "{:04x}", data.len() + 4)?;
    w.write_all(data)
}
pub fn write_text(w: &mut dyn Write, s: &str) -> io::Result<()> {
    let mut v = s.as_bytes().to_vec();
    v.push(b'\n');
    write_pkt(w, &v)
}
pub fn write_flush(w: &mut dyn Write) -> io::Result<()> {
    w.write_all(b"0000")
}
pub fn write_delim(w: &mut dyn Write) -> io::Result<()> {
    w.write_all(b"0001")
}
const MAX_BAND: usize = 65515;
pub fn write_band(w: &mut dyn Write, band: u8, data: &[u8]) -> io::Result<()> {
    for chunk in data.chunks(MAX_BAND) {
        let mut v = Vec::with_capacity(chunk.len() + 1);
        v.push(band);
        v.extend_from_slice(chunk);
        write_pkt(w, &v)?;
    }
    Ok(())
}

pub struct BandWriter<'a> {
    pub w: &'a mut dyn Write,
    pub band: u8,
}
impl Write for BandWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        write_band(self.w, self.band, buf)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

/// Read all Data pkts until a Flush; returns lines (with trailing \n stripped). Errors if the
/// stream ends (EOF) before a flush: a truncated command list must not be treated as complete,
/// or a client killed mid-push could still have its ref deletions applied.
pub fn read_lines_until_flush(r: &mut dyn BufRead) -> io::Result<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    // cap the number of lines: a client streaming pkt-lines forever must not grow this unbounded
    const MAX_LINES: usize = 100_000;
    loop {
        match read_pkt(r)? {
            Some(Pkt::Data(mut d)) => {
                if d.last() == Some(&b'\n') {
                    d.pop();
                }
                out.push(d);
                if out.len() > MAX_LINES {
                    return Err(io::Error::other("too many pkt-lines"));
                }
            }
            Some(Pkt::Flush) => return Ok(out),
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated pkt-line stream (no flush)",
                ))
            }
            Some(Pkt::Delim) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    #[test]
    fn roundtrip() {
        let mut buf = Vec::new();
        write_text(&mut buf, "hello").unwrap();
        write_flush(&mut buf).unwrap();
        write_delim(&mut buf).unwrap();
        write_pkt(&mut buf, b"raw").unwrap();
        assert_eq!(&buf[..10], b"000ahello\n");
        let mut c = Cursor::new(buf);
        assert!(matches!(read_pkt(&mut c).unwrap(), Some(Pkt::Data(d)) if d == b"hello\n"));
        assert!(matches!(read_pkt(&mut c).unwrap(), Some(Pkt::Flush)));
        assert!(matches!(read_pkt(&mut c).unwrap(), Some(Pkt::Delim)));
        assert!(matches!(read_pkt(&mut c).unwrap(), Some(Pkt::Data(d)) if d == b"raw"));
        assert!(read_pkt(&mut c).unwrap().is_none());
    }
    #[test]
    fn truncated_stream_is_an_error() {
        let mut buf = Vec::new();
        write_text(&mut buf, "0000 1111 refs/heads/x").unwrap();
        let mut c = Cursor::new(buf);
        assert!(read_lines_until_flush(&mut c).is_err());
        let mut buf = Vec::new();
        write_text(&mut buf, "0000 1111 refs/heads/x").unwrap();
        write_flush(&mut buf).unwrap();
        let mut c = Cursor::new(buf);
        assert_eq!(read_lines_until_flush(&mut c).unwrap().len(), 1);
    }

    #[test]
    fn oversized_pkt_is_rejected() {
        let mut buf = Vec::new();
        assert!(write_pkt(&mut buf, &vec![0u8; 0xffff]).is_err());
    }

    #[test]
    fn band_chunks() {
        let mut buf = Vec::new();
        write_band(&mut buf, 1, &vec![7u8; 70000]).unwrap();
        let mut c = Cursor::new(buf);
        let mut total = 0;
        while let Some(Pkt::Data(d)) = read_pkt(&mut c).unwrap() {
            assert_eq!(d[0], 1);
            total += d.len() - 1;
        }
        assert_eq!(total, 70000);
    }
}
