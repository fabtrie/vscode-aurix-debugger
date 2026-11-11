//! A small blocking HTTP/1.1 client: one request per connection, `Content-Length` or chunked
//! bodies. The hub uses it for its workers (loopback), the command-line tools and the stdio
//! wrapper for the hub.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde_json::Value;

/// A response.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    /// Header names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> Result<Value, String> {
        serde_json::from_slice(&self.body).map_err(|e| format!("HTTP {}: {e}: {}", self.status, self.text()))
    }
}

/// `http://host:port[/path]` -> (`host:port`, `/path`).
pub fn split_url(url: &str) -> Result<(String, String), String> {
    let rest = url.trim().strip_prefix("http://").ok_or_else(|| format!("not an http:// URL: {url}"))?;
    let (auth, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if auth.is_empty() {
        return Err(format!("no host in {url}"));
    }
    let auth = if auth.rsplit_once(':').is_some_and(|(_, p)| p.chars().all(|c| c.is_ascii_digit())) && !auth.ends_with(']') {
        auth.to_string()
    } else {
        format!("{auth}:80")
    };
    Ok((auth, path.trim_end_matches('/').to_string()))
}

fn resolve(authority: &str) -> Result<SocketAddr, String> {
    authority.to_socket_addrs().map_err(|e| format!("{authority}: {e}"))?.next().ok_or_else(|| format!("{authority}: no address"))
}

/// One request to `base` (`http://host:port[/prefix]`) + `path`.
pub fn request(base: &str, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&[u8]>, timeout: Duration) -> Result<Response, String> {
    let (authority, prefix) = split_url(base)?;
    let addr = resolve(&authority)?;
    let mut s = TcpStream::connect_timeout(&addr, timeout.min(Duration::from_secs(10))).map_err(|e| format!("{authority}: {e}"))?;
    s.set_read_timeout(Some(timeout)).ok();
    s.set_write_timeout(Some(timeout)).ok();
    let _ = s.set_nodelay(true);
    let body = body.unwrap_or_default();
    let mut req = format!("{method} {prefix}{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    if !body.is_empty() && !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
        req += "Content-Type: application/json\r\n";
    }
    for (k, v) in headers {
        req += &format!("{k}: {v}\r\n");
    }
    req += "\r\n";
    s.write_all(req.as_bytes()).and_then(|_| s.write_all(body)).map_err(|e| format!("{authority}: {e}"))?;
    read_response(s).map_err(|e| format!("{authority}: {e}"))
}

fn read_response(s: TcpStream) -> std::io::Result<Response> {
    let mut r = BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let status = line.split(' ').nth(1).and_then(|c| c.parse().ok()).ok_or_else(|| std::io::Error::other(format!("bad status line: {line:?}")))?;
    let mut headers = Vec::new();
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            break;
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let get = |n: &str| headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    let mut body = Vec::new();
    if get("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        loop {
            line.clear();
            r.read_line(&mut line)?;
            let n = usize::from_str_radix(line.trim().split(';').next().unwrap_or("0"), 16).unwrap_or(0);
            if n == 0 {
                break;
            }
            let mut chunk = vec![0u8; n];
            r.read_exact(&mut chunk)?;
            body.extend_from_slice(&chunk);
            line.clear();
            r.read_line(&mut line)?;
        }
    } else if let Some(n) = get("content-length").and_then(|v| v.parse::<usize>().ok()) {
        body.resize(n, 0);
        r.read_exact(&mut body)?;
    } else {
        r.read_to_end(&mut body)?;
    }
    Ok(Response { status, headers, body })
}

/// A JSON request; returns the status and the JSON body (null when there is none).
pub fn json(base: &str, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&Value>, timeout: Duration) -> Result<(u16, Value), String> {
    let bytes = body.map(|b| b.to_string().into_bytes());
    let r = request(base, method, path, headers, bytes.as_deref(), timeout)?;
    let v = if r.body.is_empty() { Value::Null } else { r.json()? };
    Ok((r.status, v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(split_url("http://127.0.0.1:3001").unwrap(), ("127.0.0.1:3001".into(), "".into()));
        assert_eq!(split_url("http://host/x/").unwrap(), ("host:80".into(), "/x".into()));
        assert!(split_url("https://x").is_err());
    }
}
