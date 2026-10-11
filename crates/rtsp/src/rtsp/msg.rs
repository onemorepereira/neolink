//! RTSP 1.0 messages: incremental request parsing and response building (DESIGN §6.1)
//!
//! The read side of a connection carries requests and, once media is set up, interleaved
//! `$` frames (RTCP from the client). [`parse`] takes whatever is buffered and returns the
//! next complete item, or asks for more.

use bytes::{Buf, Bytes, BytesMut};

/// Largest request head (request line + headers)
pub const MAX_HEAD: usize = 8 * 1024;
/// Largest request body (SPEC R24: 413 above)
pub const MAX_BODY: usize = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub uri: String,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl Request {
    /// A header value, by case-insensitive name
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn cseq(&self) -> Option<&str> {
        self.header("CSeq")
    }
}

/// What the parser found at the front of the buffer
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Request(Request),
    /// An interleaved frame from the client
    Interleaved {
        channel: u8,
        data: Bytes,
    },
}

/// Why a request was rejected outright
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// 400
    Malformed,
    /// 431 (head over 8 KiB)
    HeadTooLarge,
    /// 413
    BodyTooLarge,
    /// 505
    BadVersion,
}

impl ParseError {
    pub fn status(self) -> u16 {
        match self {
            ParseError::Malformed => 400,
            ParseError::HeadTooLarge => 431,
            ParseError::BodyTooLarge => 413,
            ParseError::BadVersion => 505,
        }
    }
}

/// Parse the next item from `buf`, consuming it. `Ok(None)` means "need more bytes".
pub fn parse(buf: &mut BytesMut) -> Result<Option<Parsed>, ParseError> {
    // Tolerate keep-alive CRLFs between messages
    while buf.first().is_some_and(|b| *b == b'\r' || *b == b'\n') {
        buf.advance(1);
    }
    match buf.first() {
        None => return Ok(None),
        Some(b'$') => {
            if buf.len() < 4 {
                return Ok(None);
            }
            let channel = buf[1];
            let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
            if buf.len() < 4 + len {
                return Ok(None);
            }
            buf.advance(4);
            let data = buf.split_to(len).freeze();
            return Ok(Some(Parsed::Interleaved { channel, data }));
        }
        _ => {}
    }
    let Some(end) = find(buf, b"\r\n\r\n") else {
        if buf.len() > MAX_HEAD {
            return Err(ParseError::HeadTooLarge);
        }
        return Ok(None);
    };
    if end > MAX_HEAD {
        return Err(ParseError::HeadTooLarge);
    }
    let head = std::str::from_utf8(&buf[..end]).map_err(|_| ParseError::Malformed)?;
    let mut lines = head.split("\r\n");
    let mut first = lines
        .next()
        .unwrap_or("")
        .split(' ')
        .filter(|p| !p.is_empty());
    let (method, uri, version) = match (first.next(), first.next(), first.next(), first.next()) {
        (Some(m), Some(u), Some(v), None) => (m, u, v),
        _ => return Err(ParseError::Malformed),
    };
    if !version.starts_with("RTSP/") {
        return Err(ParseError::Malformed);
    }
    if version != "RTSP/1.0" {
        return Err(ParseError::BadVersion);
    }
    if !method.bytes().all(|b| b.is_ascii_uppercase() || b == b'_') {
        return Err(ParseError::Malformed);
    }
    let mut headers: Vec<(String, String)> = vec![];
    for line in lines {
        if line.starts_with(' ') || line.starts_with('\t') {
            // Obsolete line folding: append to the previous header
            match headers.last_mut() {
                Some((_, v)) => {
                    v.push(' ');
                    v.push_str(line.trim());
                    continue;
                }
                None => return Err(ParseError::Malformed),
            }
        }
        let (k, v) = line.split_once(':').ok_or(ParseError::Malformed)?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    let content_length = match headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
    {
        Some((_, v)) => v.parse::<usize>().map_err(|_| ParseError::Malformed)?,
        None => 0,
    };
    if content_length > MAX_BODY {
        return Err(ParseError::BodyTooLarge);
    }
    if buf.len() < end + 4 + content_length {
        return Ok(None);
    }
    let req = Request {
        method: method.to_string(),
        uri: uri.to_string(),
        headers,
        body: Bytes::new(),
    };
    buf.advance(end + 4);
    let body = buf.split_to(content_length).freeze();
    Ok(Some(Parsed::Request(Request { body, ..req })))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A response being built
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        413 => "Request Entity Too Large",
        431 => "Request Header Fields Too Large",
        454 => "Session Not Found",
        455 => "Method Not Valid in This State",
        459 => "Aggregate Operation Not Allowed",
        461 => "Unsupported Transport",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        505 => "RTSP Version Not Supported",
        _ => "Unknown",
    }
}

impl Response {
    pub fn new(status: u16) -> Self {
        Self {
            status,
            headers: vec![],
            body: Bytes::new(),
        }
    }

    pub fn header(mut self, k: &str, v: impl Into<String>) -> Self {
        self.headers.push((k.to_string(), v.into()));
        self
    }

    pub fn body(mut self, content_type: &str, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self.headers
            .push(("Content-Type".to_string(), content_type.to_string()));
        self
    }

    /// Serialize, echoing `cseq` first like GStreamer
    pub fn to_bytes(&self, cseq: Option<&str>) -> Bytes {
        let mut out = format!("RTSP/1.0 {} {}\r\n", self.status, reason(self.status));
        if let Some(c) = cseq {
            out.push_str(&format!("CSeq: {c}\r\n"));
        }
        for (k, v) in &self.headers {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        out.push_str("Server: neolink\r\n");
        if !self.body.is_empty() {
            out.push_str(&format!("Content-Length: {}\r\n", self.body.len()));
        }
        out.push_str("\r\n");
        let mut b = BytesMut::from(out.as_bytes());
        b.extend_from_slice(&self.body);
        b.freeze()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(data: &[u8], step: usize) -> Vec<Result<Parsed, ParseError>> {
        let mut buf = BytesMut::new();
        let mut out = vec![];
        for chunk in data.chunks(step.max(1)) {
            buf.extend_from_slice(chunk);
            loop {
                match parse(&mut buf) {
                    Ok(Some(p)) => out.push(Ok(p)),
                    Ok(None) => break,
                    Err(e) => {
                        out.push(Err(e));
                        return out;
                    }
                }
            }
        }
        out
    }

    #[test]
    fn test_requests_and_interleaved_in_any_chunking() {
        let data = b"OPTIONS rtsp://h/argus/main RTSP/1.0\r\nCSeq: 1\r\nUser-Agent: x\r\n\r\n\
\r\n$\x01\x00\x03abcSET_PARAMETER rtsp://h/x RTSP/1.0\r\ncseq: 9\r\nContent-Length: 4\r\n\r\nbody";
        for step in [1, 2, 7, 1000] {
            let out = parse_all(data, step);
            assert_eq!(out.len(), 3, "step {step}");
            match &out[0] {
                Ok(Parsed::Request(r)) => {
                    assert_eq!(
                        (r.method.as_str(), r.uri.as_str()),
                        ("OPTIONS", "rtsp://h/argus/main")
                    );
                    assert_eq!(r.cseq(), Some("1"));
                }
                o => panic!("{o:?}"),
            }
            assert_eq!(
                out[1],
                Ok(Parsed::Interleaved {
                    channel: 1,
                    data: Bytes::from_static(b"abc")
                })
            );
            match &out[2] {
                Ok(Parsed::Request(r)) => {
                    assert_eq!(r.cseq(), Some("9"));
                    assert_eq!(&r.body[..], b"body");
                }
                o => panic!("{o:?}"),
            }
        }
    }

    #[test]
    fn test_rejections() {
        let e = |d: &[u8]| parse(&mut BytesMut::from(d));
        assert_eq!(e(b"GET / HTTP/1.1\r\n\r\n"), Err(ParseError::Malformed));
        assert_eq!(
            e(b"OPTIONS * RTSP/2.0\r\n\r\n"),
            Err(ParseError::BadVersion)
        );
        assert_eq!(
            e(b"OPTIONS * RTSP/1.0\r\nnocolon\r\n\r\n"),
            Err(ParseError::Malformed)
        );
        assert_eq!(
            e(b"ANNOUNCE * RTSP/1.0\r\nContent-Length: 99999\r\n\r\n"),
            Err(ParseError::BodyTooLarge)
        );
        let big = vec![b'A'; MAX_HEAD + 10];
        assert_eq!(e(&big), Err(ParseError::HeadTooLarge));
        // Folded header
        match e(b"OPTIONS * RTSP/1.0\r\nX: a\r\n b\r\nCSeq: 2\r\n\r\n") {
            Ok(Some(Parsed::Request(r))) => assert_eq!(r.header("x"), Some("a b")),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn test_response_bytes() {
        let r = Response::new(200).header("Public", "OPTIONS, DESCRIBE");
        let b = r.to_bytes(Some("3"));
        assert_eq!(
            std::str::from_utf8(&b).unwrap(),
            "RTSP/1.0 200 OK\r\nCSeq: 3\r\nPublic: OPTIONS, DESCRIBE\r\nServer: neolink\r\n\r\n"
        );
        let r = Response::new(200).body("application/sdp", "v=0\r\n");
        let s = String::from_utf8(r.to_bytes(None).to_vec()).unwrap();
        assert!(s.ends_with("Content-Length: 5\r\n\r\nv=0\r\n"));
    }
}
