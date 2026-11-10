//! S3-compatible object storage, signed with AWS Signature Version 4.
//!
//! Credentials come from environment variables the pod fills from an existing Secret.

use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cubeharbor_core::backup::{S3Storage, check_relative_path};
use hmac::{Hmac, KeyInit, Mac};
use reqwest::Method;
use reqwest::blocking::{Client, Response};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{Backend, ObjectStore, PodWiring, checked};

/// Buffered in memory; S3 allows 10,000 parts, so archives can reach about 320 GiB compressed.
pub const PART_SIZE: usize = 32 * 1024 * 1024;
const ATTEMPTS: u32 = 4;

const ENV_ACCESS_KEY: &str = "AWS_ACCESS_KEY_ID";
const ENV_SECRET_KEY: &str = "AWS_SECRET_ACCESS_KEY";
const ENV_SESSION_TOKEN: &str = "AWS_SESSION_TOKEN";

pub struct S3 {
    cfg: S3Storage,
}

impl S3 {
    pub fn new(cfg: &S3Storage) -> Result<Self, String> {
        if cfg.bucket.is_empty() || cfg.bucket.contains('/') {
            return Err("s3.bucket must be a bucket name".into());
        }
        if cfg.credentials_secret.name.is_empty() {
            return Err("s3.credentialsSecret.name is required".into());
        }
        if let Some(e) = &cfg.endpoint
            && !(e.starts_with("https://") || e.starts_with("http://"))
        {
            return Err("s3.endpoint must start with https:// or http://".into());
        }
        if let Some(p) = &cfg.prefix {
            check_relative_path(p.trim_end_matches('/')).map_err(|e| format!("s3.prefix: {e}"))?;
        }
        Ok(Self { cfg: cfg.clone() })
    }
}

impl Backend for S3 {
    fn wiring(&self) -> PodWiring {
        let sec = &self.cfg.credentials_secret;
        let var = |name: &str, key: &Option<String>, default: &str, optional: bool| {
            json!({ "name": name, "valueFrom": { "secretKeyRef": {
                "name": sec.name, "key": key.as_deref().unwrap_or(default), "optional": optional,
            } } })
        };
        PodWiring {
            env: vec![
                var(ENV_ACCESS_KEY, &sec.access_key_id_key, "accessKeyId", false),
                var(
                    ENV_SECRET_KEY,
                    &sec.secret_access_key_key,
                    "secretAccessKey",
                    false,
                ),
                var(
                    ENV_SESSION_TOKEN,
                    &sec.session_token_key,
                    "sessionToken",
                    true,
                ),
            ],
            volumes: vec![],
            mounts: vec![],
        }
    }

    fn open(&self) -> Result<Box<dyn ObjectStore>> {
        let env = |n: &str| std::env::var(n).with_context(|| format!("{n} is not set"));
        let creds = Credentials {
            access_key: env(ENV_ACCESS_KEY)?,
            secret_key: env(ENV_SECRET_KEY)?,
            session_token: std::env::var(ENV_SESSION_TOKEN)
                .ok()
                .filter(|t| !t.is_empty()),
        };
        Ok(Box::new(Bucket::new(&self.cfg, creds, PART_SIZE)?))
    }
}

pub struct Credentials {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
}

pub struct Bucket {
    http: Client,
    /// Everything before the key, with no trailing slash.
    base: String,
    prefix: String,
    region: String,
    creds: Credentials,
    part_size: usize,
}

fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes any key length");
    m.update(msg);
    m.finalize().into_bytes().to_vec()
}

fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The parts of a request that get signed. `headers` are lower-case and sorted by name; `query`
/// is already canonical.
struct Signed<'a> {
    method: &'a str,
    path: &'a str,
    query: &'a str,
    headers: &'a [(&'a str, &'a str)],
    payload_hash: &'a str,
    amz_date: &'a str,
}

/// The `Authorization` header for a request.
fn authorization(creds: &Credentials, region: &str, service: &str, req: &Signed<'_>) -> String {
    let Signed {
        method,
        path,
        query,
        headers,
        payload_hash,
        amz_date,
    } = *req;
    let date = &amz_date[..8];
    let canonical_headers: String = headers
        .iter()
        .map(|(k, v)| format!("{k}:{}\n", v.trim()))
        .collect();
    let signed = headers
        .iter()
        .map(|(k, _)| *k)
        .collect::<Vec<_>>()
        .join(";");
    let request =
        format!("{method}\n{path}\n{query}\n{canonical_headers}\n{signed}\n{payload_hash}");
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(request.as_bytes())
    );
    let mut key = hmac(
        format!("AWS4{}", creds.secret_key).as_bytes(),
        date.as_bytes(),
    );
    for part in [region, service, "aws4_request"] {
        key = hmac(&key, part.as_bytes());
    }
    let signature = hex(&hmac(&key, to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
        creds.access_key
    )
}

impl Bucket {
    pub fn new(cfg: &S3Storage, creds: Credentials, part_size: usize) -> Result<Self> {
        let endpoint = cfg
            .endpoint
            .clone()
            .unwrap_or_else(|| format!("https://s3.{}.amazonaws.com", cfg.region));
        let endpoint = endpoint.trim_end_matches('/').to_string();
        let path_style = cfg.path_style.unwrap_or(cfg.endpoint.is_some());
        let base = if path_style {
            format!("{endpoint}/{}", cfg.bucket)
        } else {
            let (scheme, host) = endpoint
                .split_once("://")
                .context("endpoint has no scheme")?;
            format!("{scheme}://{}.{host}", cfg.bucket)
        };
        let prefix = cfg
            .prefix
            .as_deref()
            .map(|p| format!("{}/", p.trim_matches('/')))
            .unwrap_or_default();
        // No overall timeout: downloads of large archives are limited by the Job's deadline.
        let http = Client::builder()
            .timeout(None::<Duration>)
            .connect_timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            base,
            prefix,
            region: cfg.region.clone(),
            creds,
            part_size,
        })
    }

    fn send(&self, method: Method, key: &str, query: &str, body: &[u8]) -> Result<Response> {
        let url = format!(
            "{}/{}{}",
            self.base,
            uri_encode(&format!("{}{key}", self.prefix), true),
            if query.is_empty() {
                String::new()
            } else {
                format!("?{query}")
            }
        );
        let mut last = None;
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(Duration::from_secs(1 << attempt));
            }
            match self.attempt(&method, &url, query, body) {
                Ok(r) if r.status().is_success() => return Ok(r),
                Ok(r) if r.status().is_server_error() => {
                    last = Some(format!("{method} {key}: HTTP {}", r.status()))
                }
                Ok(r) => bail!(
                    "{method} {key}: HTTP {}: {}",
                    r.status(),
                    r.text()
                        .unwrap_or_default()
                        .chars()
                        .take(300)
                        .collect::<String>()
                ),
                Err(e) => last = Some(format!("{method} {key}: {e:#}")),
            }
        }
        bail!("{}", last.unwrap_or_default())
    }

    fn attempt(&self, method: &Method, url: &str, query: &str, body: &[u8]) -> Result<Response> {
        let parsed = reqwest::Url::parse(url)?;
        let host = match (parsed.host_str().context("URL has no host")?, parsed.port()) {
            (h, Some(p)) => format!("{h}:{p}"),
            (h, None) => h.to_string(),
        };
        let amz_date = jiff::Timestamp::now()
            .strftime("%Y%m%dT%H%M%SZ")
            .to_string();
        let payload_hash = sha256_hex(body);
        let mut headers = vec![
            ("host", host.as_str()),
            ("x-amz-content-sha256", payload_hash.as_str()),
            ("x-amz-date", amz_date.as_str()),
        ];
        if let Some(t) = &self.creds.session_token {
            headers.push(("x-amz-security-token", t.as_str()));
        }
        let auth = authorization(
            &self.creds,
            &self.region,
            "s3",
            &Signed {
                method: method.as_str(),
                path: parsed.path(),
                query,
                headers: &headers,
                payload_hash: &payload_hash,
                amz_date: &amz_date,
            },
        );
        let mut req = self
            .http
            .request(method.clone(), parsed)
            .header("authorization", auth)
            .body(body.to_vec());
        for (k, v) in headers.iter().filter(|(k, _)| *k != "host") {
            req = req.header(*k, *v);
        }
        Ok(req.send()?)
    }

    fn multipart(
        &self,
        key: &str,
        upload_id: &str,
        first: Vec<u8>,
        data: &mut dyn Read,
    ) -> Result<u64> {
        let mut parts = String::new();
        let mut total = 0u64;
        let mut part = first;
        for number in 1u32.. {
            anyhow::ensure!(
                number <= 10_000,
                "archive is too large for a multipart upload"
            );
            let query = format!(
                "partNumber={number}&uploadId={}",
                uri_encode(upload_id, false)
            );
            let response = self.send(Method::PUT, key, &query, &part)?;
            let etag = response
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .context("no ETag on an uploaded part")?;
            parts.push_str(&format!(
                "<Part><PartNumber>{number}</PartNumber><ETag>{}</ETag></Part>",
                etag.replace('"', "&quot;")
            ));
            total += part.len() as u64;
            part = read_part(data, self.part_size)?;
            if part.is_empty() {
                break;
            }
        }
        let query = format!("uploadId={}", uri_encode(upload_id, false));
        let done = format!("<CompleteMultipartUpload>{parts}</CompleteMultipartUpload>");
        let response = self.send(Method::POST, key, &query, done.as_bytes())?;
        // S3 can answer 200 and report the failure in the body.
        let text = response.text()?;
        anyhow::ensure!(
            !text.contains("<Error>"),
            "completing the upload of {key}: {}",
            text.chars().take(300).collect::<String>()
        );
        Ok(total)
    }
}

fn read_part(data: &mut dyn Read, size: usize) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(size.min(1 << 20));
    data.take(size as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = xml.find(&open)? + open.len();
    Some(&xml[start..start + xml[start..].find('<')?])
}

impl ObjectStore for Bucket {
    fn put(&self, key: &str, data: &mut dyn Read) -> Result<u64> {
        checked(key)?;
        let first = read_part(data, self.part_size)?;
        if first.len() < self.part_size {
            self.send(Method::PUT, key, "", &first)?;
            return Ok(first.len() as u64);
        }
        let xml = self.send(Method::POST, key, "uploads=", b"")?.text()?;
        let upload_id = tag(&xml, "UploadId")
            .context("no UploadId in the response")?
            .to_string();
        let result = self.multipart(key, &upload_id, first, data);
        if result.is_err() {
            let query = format!("uploadId={}", uri_encode(&upload_id, false));
            let _ = self.send(Method::DELETE, key, &query, b"");
        }
        result
    }

    fn get(&self, key: &str) -> Result<Box<dyn Read + Send>> {
        checked(key)?;
        Ok(Box::new(self.send(Method::GET, key, "", b"")?))
    }

    fn delete(&self, key: &str) -> Result<()> {
        checked(key)?;
        self.send(Method::DELETE, key, "", b"")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use super::*;
    use cubeharbor_core::backup::S3CredentialsSecret;

    fn creds() -> Credentials {
        Credentials {
            access_key: "AKIDEXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        }
    }

    /// `get-vanilla` from the AWS Signature Version 4 test suite.
    #[test]
    fn signature_matches_the_aws_test_suite() {
        let date = "20150830T123600Z";
        let auth = authorization(
            &creds(),
            "us-east-1",
            "service",
            &Signed {
                method: "GET",
                path: "/",
                query: "",
                headers: &[("host", "example.amazonaws.com"), ("x-amz-date", date)],
                payload_hash: &sha256_hex(b""),
                amz_date: date,
            },
        );
        assert!(
            auth.ends_with(
                "Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
            ),
            "{auth}"
        );
    }

    #[test]
    fn encoding() {
        assert_eq!(uri_encode("a b/c~", true), "a%20b/c~");
        assert_eq!(uri_encode("a/b", false), "a%2Fb");
    }

    #[test]
    fn config_is_validated_and_credentials_stay_in_the_secret() {
        let good = S3Storage {
            bucket: "b".into(),
            region: "r".into(),
            credentials_secret: S3CredentialsSecret {
                name: "s3".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let wiring = S3::new(&good).unwrap().wiring();
        assert!(wiring.env.iter().all(|e| e.get("value").is_none() && e["valueFrom"]["secretKeyRef"]["name"] == "s3"));
        assert!(
            S3::new(&S3Storage {
                bucket: "".into(),
                ..good.clone()
            })
            .is_err()
        );
        assert!(
            S3::new(&S3Storage {
                endpoint: Some("ftp://x".into()),
                ..good.clone()
            })
            .is_err()
        );
        assert!(
            S3::new(&S3Storage {
                prefix: Some("../x".into()),
                ..good.clone()
            })
            .is_err()
        );
        assert!(
            S3::new(&S3Storage {
                credentials_secret: S3CredentialsSecret::default(),
                ..good
            })
            .is_err()
        );
    }

    #[derive(Default)]
    struct Fake {
        objects: HashMap<String, Vec<u8>>,
        parts: Vec<(u32, Vec<u8>)>,
        aborted: u32,
        fail_part: Option<u32>,
    }

    /// Just enough of S3 for the upload flows, on a local socket.
    fn serve(state: Arc<Mutex<Fake>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let state = state.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(conn.try_clone().unwrap());
                    let mut conn = conn;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        let mut it = line.split_whitespace();
                        let (method, target) = (
                            it.next().unwrap().to_string(),
                            it.next().unwrap().to_string(),
                        );
                        let mut len = 0usize;
                        let mut signed = false;
                        loop {
                            let mut h = String::new();
                            reader.read_line(&mut h).unwrap();
                            if h.trim().is_empty() {
                                break;
                            }
                            let l = h.to_ascii_lowercase();
                            if let Some(v) = l.strip_prefix("content-length:") {
                                len = v.trim().parse().unwrap();
                            }
                            signed |= l.starts_with("authorization: aws4-hmac-sha256");
                        }
                        assert!(signed);
                        let mut body = vec![0; len];
                        reader.read_exact(&mut body).unwrap();
                        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
                        let mut s = state.lock().unwrap();
                        let (status, extra, out): (&str, String, Vec<u8>) =
                            match (method.as_str(), query) {
                                ("POST", "uploads=") => (
                                    "200 OK",
                                    String::new(),
                                    b"<R><UploadId>a/b+c</UploadId></R>".to_vec(),
                                ),
                                ("POST", _) => {
                                    let mut all = vec![];
                                    s.parts.sort_by_key(|p| p.0);
                                    for (_, p) in s.parts.drain(..) {
                                        all.extend(p);
                                    }
                                    s.objects.insert(path.to_string(), all);
                                    ("200 OK", String::new(), b"<Done/>".to_vec())
                                }
                                ("PUT", q) if q.starts_with("partNumber=") => {
                                    assert!(q.contains("uploadId=a%2Fb%2Bc"), "{q}");
                                    let n: u32 = q["partNumber=".len()..q.find('&').unwrap()]
                                        .parse()
                                        .unwrap();
                                    if s.fail_part == Some(n) {
                                        ("403 Forbidden", String::new(), b"denied".to_vec())
                                    } else {
                                        s.parts.push((n, body));
                                        ("200 OK", format!("ETag: \"e{n}\"\r\n"), vec![])
                                    }
                                }
                                ("PUT", _) => {
                                    s.objects.insert(path.to_string(), body);
                                    ("200 OK", String::new(), vec![])
                                }
                                ("DELETE", q) if q.starts_with("uploadId=") => {
                                    s.aborted += 1;
                                    ("204 No Content", String::new(), vec![])
                                }
                                ("DELETE", _) => {
                                    s.objects.remove(path);
                                    ("204 No Content", String::new(), vec![])
                                }
                                ("GET", _) => match s.objects.get(path) {
                                    Some(o) => ("200 OK", String::new(), o.clone()),
                                    None => ("404 Not Found", String::new(), vec![]),
                                },
                                _ => ("400 Bad Request", String::new(), vec![]),
                            };
                        drop(s);
                        let head = format!(
                            "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\n\r\n",
                            out.len()
                        );
                        conn.write_all(head.as_bytes()).unwrap();
                        conn.write_all(&out).unwrap();
                    }
                });
            }
        });
        format!("http://{addr}")
    }

    fn bucket(endpoint: String, part_size: usize) -> Bucket {
        let cfg = S3Storage {
            bucket: "b".into(),
            region: "us-east-1".into(),
            endpoint: Some(endpoint),
            prefix: Some("pre".into()),
            ..Default::default()
        };
        Bucket::new(&cfg, creds(), part_size).unwrap()
    }

    #[test]
    fn small_object_round_trip() {
        let state = Arc::new(Mutex::new(Fake::default()));
        let b = bucket(serve(state.clone()), 1024);
        assert_eq!(b.put("x/a.tar.gz", &mut &b"small"[..]).unwrap(), 5);
        assert!(
            state
                .lock()
                .unwrap()
                .objects
                .contains_key("/b/pre/x/a.tar.gz")
        );
        let mut out = vec![];
        b.get("x/a.tar.gz").unwrap().read_to_end(&mut out).unwrap();
        assert_eq!(out, b"small");
        b.delete("x/a.tar.gz").unwrap();
        assert!(b.get("x/a.tar.gz").is_err());
    }

    #[test]
    fn large_object_uses_multipart() {
        let state = Arc::new(Mutex::new(Fake::default()));
        let b = bucket(serve(state.clone()), 4);
        let data: Vec<u8> = (0..10).collect();
        assert_eq!(b.put("x/big", &mut data.as_slice()).unwrap(), 10);
        assert_eq!(state.lock().unwrap().objects["/b/pre/x/big"], data);
    }

    #[test]
    fn failed_part_aborts_the_upload() {
        let state = Arc::new(Mutex::new(Fake {
            fail_part: Some(2),
            ..Default::default()
        }));
        let b = bucket(serve(state.clone()), 4);
        let data: Vec<u8> = (0..10).collect();
        assert!(b.put("x/big", &mut data.as_slice()).is_err());
        let s = state.lock().unwrap();
        assert_eq!(s.aborted, 1);
        assert!(s.objects.is_empty());
    }

    #[test]
    fn read_error_aborts_the_upload() {
        /// Five bytes, then an error, as when the archiver fails.
        struct Broken(usize);
        impl Read for Broken {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                if self.0 == 5 {
                    return Err(std::io::Error::other("source failed"));
                }
                self.0 += 1;
                b[0] = 7;
                Ok(1)
            }
        }
        let state = Arc::new(Mutex::new(Fake::default()));
        let b = bucket(serve(state.clone()), 4);
        assert!(b.put("x/big", &mut Broken(0)).is_err());
        let s = state.lock().unwrap();
        assert_eq!(s.aborted, 1);
        assert!(s.objects.is_empty());
    }
}
