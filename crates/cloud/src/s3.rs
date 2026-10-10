//! Amazon S3 and S3-compatible storage (Cloudflare R2, MinIO, Wasabi…) as a
//! [`RemoteFs`]: `/` lists buckets, `/bucket/a/b` are keys under `a/b`.
//!
//! S3 has no folders: a "folder" is a common key prefix, and New folder writes an empty
//! `prefix/` marker object (as the AWS console does). Rename copies then deletes, so it is
//! for files only. Uploads are one PUT up to 8 MiB, else a multipart upload.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use futures::future::BoxFuture;
use reqwest::Method;
use switchyard_remote::{EntryKind, FileEntry, FsError, FsReader, FsWriter, RemoteFs};
use tracing::debug;

use crate::aws::AwsAuth;
use crate::error::{CloudError, Result};
use crate::http::{Req, Resp, client, uri_encode};
use crate::sigv4::sign;
use crate::stream::{ChunkSink, body_reader, upload_writer};
use crate::xml;

/// Most entries one folder listing returns.
const MAX_LIST: usize = 100_000;

/// Where and how to reach the storage.
#[derive(Clone, Debug)]
pub struct S3Config {
    /// Name shown in the Files tab.
    pub name: String,
    /// A custom endpoint (R2, MinIO); `None` for AWS.
    pub endpoint: Option<url::Url>,
    /// Region (`us-east-1`; R2 uses `auto`).
    pub region: String,
    /// Folder opened first (`/bucket/prefix`).
    pub home: Option<String>,
    /// Refuse every change.
    pub read_only: bool,
    /// Short service name for messages (`S3`, `R2`).
    pub service: &'static str,
}

impl S3Config {
    /// Cloudflare R2 for an account id.
    pub fn r2(name: &str, account_id: &str) -> Result<Self> {
        let id = account_id.trim();
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(CloudError::Invalid(
                "the Cloudflare account id is 32 letters and digits".into(),
            ));
        }
        Ok(Self {
            name: name.to_owned(),
            endpoint: Some(
                url::Url::parse(&format!("https://{id}.r2.cloudflarestorage.com"))
                    .map_err(|e| CloudError::Invalid(e.to_string()))?,
            ),
            region: "auto".into(),
            home: None,
            read_only: false,
            service: "R2",
        })
    }
}

struct Inner {
    cfg: S3Config,
    auth: Arc<AwsAuth>,
    http: reqwest::Client,
    /// Buckets found in another region than the connection's.
    regions: Mutex<HashMap<String, String>>,
}

/// S3 storage as a file system.
#[derive(Clone)]
pub struct S3Fs {
    inner: Arc<Inner>,
}

/// `(bucket, key)` of a path; the key has no leading or trailing `/`.
fn split(path: &Path) -> (Option<String>, String) {
    let s = path.to_string_lossy().replace('\\', "/");
    let s = s.trim_matches('/');
    if s.is_empty() {
        return (None, String::new());
    }
    match s.split_once('/') {
        Some((b, k)) => (Some(b.to_owned()), k.trim_matches('/').to_owned()),
        None => (Some(s.to_owned()), String::new()),
    }
}

fn last_segment(key: &str) -> String {
    key.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// A content type from the file extension (static sites served from a bucket need it).
pub(crate) fn guess_content_type(key: &str) -> &'static str {
    let ext = key.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("txt" | "md" | "log" | "csv") => "text/plain; charset=utf-8",
        Some("xml") => "application/xml",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("pdf") => "application/pdf",
        Some("wasm") => "application/wasm",
        Some("zip") => "application/zip",
        Some("gz") => "application/gzip",
        _ => "application/octet-stream",
    }
}

/// The error in an S3 XML error body.
fn s3_error(status: u16, body: &[u8]) -> CloudError {
    match xml::parse(body) {
        Ok(e) if e.name == "Error" => {
            let code = e.text_of("Code").map(str::to_owned);
            let msg = e.text_of("Message").unwrap_or_default();
            let msg = match code.as_deref() {
                Some("NoSuchBucket") => format!("No such bucket: {msg}"),
                Some("SignatureDoesNotMatch") => {
                    "The secret access key does not match (SignatureDoesNotMatch)".into()
                }
                Some("InvalidAccessKeyId") => {
                    "The access key id is not known to the service (InvalidAccessKeyId)".into()
                }
                _ => msg.to_owned(),
            };
            CloudError::api(status, code, msg)
        }
        _ => CloudError::api(status, None, ""),
    }
}

impl S3Fs {
    /// A file system over `cfg`, signing with `auth`.
    pub fn new(cfg: S3Config, auth: Arc<AwsAuth>) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(Inner {
                cfg,
                auth,
                http: client()?,
                regions: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// `R2 · 3 buckets`-style description for Test connection.
    pub fn describe(&self) -> String {
        match &self.inner.cfg.endpoint {
            Some(e) => format!(
                "{} · {}",
                self.inner.cfg.service,
                e.host_str().unwrap_or_default()
            ),
            None => format!("S3 · {}", self.inner.cfg.region),
        }
    }
}

impl Inner {
    fn region_for(&self, bucket: Option<&str>) -> String {
        bucket
            .and_then(|b| {
                self.regions
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(b)
                    .cloned()
            })
            .unwrap_or_else(|| self.cfg.region.clone())
    }

    /// A request for `key` in `bucket` (or the service root).
    fn req(&self, method: Method, bucket: Option<&str>, key: &str, region: &str) -> Result<Req> {
        let key = uri_encode(key, true);
        let invalid = |e: url::ParseError| CloudError::Invalid(e.to_string());
        let (base, path) = match (&self.cfg.endpoint, bucket) {
            (Some(e), None) => (e.clone(), String::new()),
            (Some(e), Some(b)) => (e.clone(), format!("/{}/{key}", uri_encode(b, false))),
            (None, b) => {
                let host = if region == "us-east-1" {
                    "s3.amazonaws.com".to_owned()
                } else {
                    format!("s3.{region}.amazonaws.com")
                };
                match b {
                    // Virtual-hosted style, except names with dots (they break TLS).
                    Some(b) if !b.contains('.') => (
                        url::Url::parse(&format!("https://{b}.{host}")).map_err(invalid)?,
                        format!("/{key}"),
                    ),
                    Some(b) => (
                        url::Url::parse(&format!("https://{host}")).map_err(invalid)?,
                        format!("/{}/{key}", uri_encode(b, false)),
                    ),
                    None => (
                        url::Url::parse(&format!("https://{host}")).map_err(invalid)?,
                        String::new(),
                    ),
                }
            }
        };
        let path = if bucket.is_some() && key.is_empty() && self.cfg.endpoint.is_some() {
            path.trim_end_matches('/').to_owned()
        } else {
            path
        };
        Ok(Req::new(method, &base, path))
    }

    async fn signed(&self, mut req: Req, region: &str) -> Result<reqwest::Response> {
        if self.cfg.read_only && !matches!(req.method, Method::GET | Method::HEAD) {
            return Err(CloudError::ReadOnly);
        }
        let creds = self.auth.credentials().await?;
        sign(&mut req, &creds, region, "s3", SystemTime::now(), true);
        req.send(&self.http).await
    }

    /// Send a request built by `build`, following S3's "wrong region" answers once.
    async fn call_raw(
        &self,
        bucket: Option<&str>,
        build: impl Fn(&str) -> Result<Req>,
    ) -> Result<reqwest::Response> {
        let region = self.region_for(bucket);
        let resp = self.signed(build(&region)?, &region).await?;
        let moved = resp
            .headers()
            .get("x-amz-bucket-region")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        match (bucket, moved) {
            (Some(b), Some(r))
                if r != region && matches!(resp.status().as_u16(), 301 | 307 | 400 | 403) =>
            {
                debug!(bucket = b, region = %r, "bucket in another region");
                self.regions
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(b.to_owned(), r.clone());
                self.signed(build(&r)?, &r).await
            }
            _ => Ok(resp),
        }
    }

    /// Like [`Inner::call_raw`], reading the body and turning failures into errors.
    async fn call(
        &self,
        bucket: Option<&str>,
        build: impl Fn(&str) -> Result<Req>,
    ) -> Result<Resp> {
        let resp = Resp::read(self.call_raw(bucket, build).await?).await?;
        if !resp.ok() {
            return Err(s3_error(resp.status, &resp.body));
        }
        Ok(resp)
    }

    async fn list_buckets(&self) -> Result<Vec<FileEntry>> {
        let resp = self
            .call(None, |r| self.req(Method::GET, None, "", r))
            .await?;
        let root = xml::parse(&resp.body)?;
        let mut out: Vec<FileEntry> = root
            .child("Buckets")
            .map(|b| {
                b.all("Bucket")
                    .filter_map(|b| {
                        Some(FileEntry {
                            name: b.text_of("Name")?.to_owned(),
                            kind: EntryKind::Dir,
                            size: 0,
                            modified_ms: b
                                .text_of("CreationDate")
                                .and_then(crate::time::parse_iso_ms),
                            mode: None,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        switchyard_remote::fs::sort_entries(&mut out);
        Ok(out)
    }

    /// One page of `ListObjectsV2`.
    async fn list_page(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: bool,
        max: Option<u32>,
        token: Option<&str>,
    ) -> Result<xml::Node> {
        let resp = self
            .call(Some(bucket), |r| {
                let mut req = self
                    .req(Method::GET, Some(bucket), "", r)?
                    .query("list-type", "2")
                    .query("prefix", prefix);
                if delimiter {
                    req = req.query("delimiter", "/");
                }
                if let Some(m) = max {
                    req = req.query("max-keys", m.to_string());
                }
                if let Some(t) = token {
                    req = req.query("continuation-token", t);
                }
                Ok(req)
            })
            .await?;
        xml::parse(&resp.body)
    }

    async fn list(&self, bucket: &str, key: &str) -> Result<Vec<FileEntry>> {
        let prefix = if key.is_empty() {
            String::new()
        } else {
            format!("{key}/")
        };
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let page = self
                .list_page(bucket, &prefix, true, None, token.as_deref())
                .await?;
            for p in page.all("CommonPrefixes") {
                if let Some(p) = p.text_of("Prefix") {
                    let name = last_segment(p);
                    if !name.is_empty() {
                        out.push(FileEntry {
                            name,
                            kind: EntryKind::Dir,
                            size: 0,
                            modified_ms: None,
                            mode: None,
                        });
                    }
                }
            }
            for c in page.all("Contents") {
                let Some(k) = c.text_of("Key") else { continue };
                // The folder's own marker object.
                if k == prefix || k.ends_with('/') {
                    continue;
                }
                out.push(FileEntry {
                    name: last_segment(k),
                    kind: EntryKind::File,
                    size: c.text_of("Size").and_then(|s| s.parse().ok()).unwrap_or(0),
                    modified_ms: c
                        .text_of("LastModified")
                        .and_then(crate::time::parse_iso_ms),
                    mode: None,
                });
            }
            token = page
                .text_of("NextContinuationToken")
                .filter(|_| page.text_of("IsTruncated") == Some("true"))
                .map(str::to_owned);
            if token.is_none() || out.len() >= MAX_LIST {
                break;
            }
        }
        switchyard_remote::fs::sort_entries(&mut out);
        Ok(out)
    }

    async fn stat(&self, bucket: Option<&str>, key: &str) -> Result<FileEntry> {
        let Some(bucket) = bucket else {
            return Ok(FileEntry {
                name: "/".into(),
                kind: EntryKind::Dir,
                size: 0,
                modified_ms: None,
                mode: None,
            });
        };
        let dir = |name: &str| FileEntry {
            name: name.to_owned(),
            kind: EntryKind::Dir,
            size: 0,
            modified_ms: None,
            mode: None,
        };
        if key.is_empty() {
            let resp = Resp::read(
                self.call_raw(Some(bucket), |r| {
                    self.req(Method::HEAD, Some(bucket), "", r)
                })
                .await?,
            )
            .await?;
            return match resp.status {
                // 403: it exists, the credentials just may not inspect it.
                200..=299 | 301 | 403 => Ok(dir(bucket)),
                s => Err(CloudError::api(s, Some("NoSuchBucket".into()), "")),
            };
        }
        let resp = Resp::read(
            self.call_raw(Some(bucket), |r| {
                self.req(Method::HEAD, Some(bucket), key, r)
            })
            .await?,
        )
        .await?;
        if resp.ok() {
            return Ok(FileEntry {
                name: last_segment(key),
                kind: EntryKind::File,
                size: resp
                    .header("content-length")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
                modified_ms: resp
                    .header("last-modified")
                    .and_then(crate::time::parse_http_date_ms),
                mode: None,
            });
        }
        if resp.status != 404 {
            return Err(s3_error(resp.status, &resp.body));
        }
        let page = self
            .list_page(bucket, &format!("{key}/"), false, Some(1), None)
            .await?;
        if page.child("Contents").is_some() {
            Ok(dir(&last_segment(key)))
        } else {
            Err(CloudError::api(
                404,
                Some("NoSuchKey".into()),
                format!("{key} not found"),
            ))
        }
    }

    async fn put(&self, bucket: &str, key: &str, data: Vec<u8>) -> Result<()> {
        let ct = guess_content_type(key);
        self.call(Some(bucket), |r| {
            Ok(self
                .req(Method::PUT, Some(bucket), key, r)?
                .header("content-type", ct)
                .body(data.clone()))
        })
        .await?;
        Ok(())
    }

    async fn delete(&self, bucket: &str, key: &str) -> Result<()> {
        self.call(Some(bucket), |r| {
            self.req(Method::DELETE, Some(bucket), key, r)
        })
        .await?;
        Ok(())
    }

    async fn create_bucket(&self, bucket: &str) -> Result<()> {
        let region = self.cfg.region.clone();
        let body = if self.cfg.endpoint.is_none() && region != "us-east-1" {
            format!(
                "<CreateBucketConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                 <LocationConstraint>{}</LocationConstraint></CreateBucketConfiguration>",
                xml::escape(&region)
            )
            .into_bytes()
        } else {
            Vec::new()
        };
        self.call(None, |r| {
            Ok(self
                .req(Method::PUT, Some(bucket), "", r)?
                .body(body.clone()))
        })
        .await?;
        Ok(())
    }

    async fn copy(&self, from: (&str, &str), to: (&str, &str)) -> Result<()> {
        let source = format!(
            "/{}/{}",
            uri_encode(from.0, false),
            uri_encode(from.1, true)
        );
        let resp = self
            .call(Some(to.0), |r| {
                Ok(self
                    .req(Method::PUT, Some(to.0), to.1, r)?
                    .header("x-amz-copy-source", source.clone()))
            })
            .await?;
        // A copy can fail after S3 has answered 200.
        if let Ok(e) = xml::parse(&resp.body)
            && e.name == "Error"
        {
            return Err(s3_error(500, &resp.body));
        }
        Ok(())
    }
}

/// Upload target for one object.
struct Upload {
    inner: Arc<Inner>,
    bucket: String,
    key: String,
}

impl ChunkSink for Upload {
    fn put_whole(&self, data: Vec<u8>) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.inner.put(&self.bucket, &self.key, data))
    }

    fn begin(&self) -> BoxFuture<'_, Result<String>> {
        Box::pin(async move {
            let ct = guess_content_type(&self.key);
            let resp = self
                .inner
                .call(Some(&self.bucket), |r| {
                    Ok(self
                        .inner
                        .req(Method::POST, Some(&self.bucket), &self.key, r)?
                        .query("uploads", "")
                        .header("content-type", ct))
                })
                .await?;
            xml::parse(&resp.body)?
                .text_of("UploadId")
                .map(str::to_owned)
                .ok_or_else(|| CloudError::Network("no UploadId in the reply".into()))
        })
    }

    fn put_part<'a>(
        &'a self,
        upload: &'a str,
        n: u32,
        data: Vec<u8>,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let resp = self
                .inner
                .call(Some(&self.bucket), |r| {
                    Ok(self
                        .inner
                        .req(Method::PUT, Some(&self.bucket), &self.key, r)?
                        .query("partNumber", n.to_string())
                        .query("uploadId", upload)
                        .body(data.clone()))
                })
                .await?;
            resp.header("etag")
                .map(str::to_owned)
                .ok_or_else(|| CloudError::Network("no ETag for an uploaded part".into()))
        })
    }

    fn complete<'a>(&'a self, upload: &'a str, parts: Vec<String>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut body = String::from("<CompleteMultipartUpload>");
            for (i, etag) in parts.iter().enumerate() {
                body.push_str(&format!(
                    "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
                    i + 1,
                    xml::escape(etag)
                ));
            }
            body.push_str("</CompleteMultipartUpload>");
            let resp = self
                .inner
                .call(Some(&self.bucket), |r| {
                    Ok(self
                        .inner
                        .req(Method::POST, Some(&self.bucket), &self.key, r)?
                        .query("uploadId", upload)
                        .header("content-type", "application/xml")
                        .body(body.clone().into_bytes()))
                })
                .await?;
            if let Ok(e) = xml::parse(&resp.body)
                && e.name == "Error"
            {
                return Err(s3_error(500, &resp.body));
            }
            Ok(())
        })
    }

    fn abort<'a>(&'a self, upload: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.inner
                .call(Some(&self.bucket), |r| {
                    Ok(self
                        .inner
                        .req(Method::DELETE, Some(&self.bucket), &self.key, r)?
                        .query("uploadId", upload))
                })
                .await?;
            Ok(())
        })
    }
}

fn need_key(bucket: Option<String>, key: &str, what: &'static str) -> Result<String> {
    match bucket {
        Some(b) if !key.is_empty() => Ok(b),
        _ => Err(CloudError::Unsupported(what)),
    }
}

impl RemoteFs for S3Fs {
    fn name(&self) -> &str {
        &self.inner.cfg.name
    }

    fn home(&self) -> PathBuf {
        let h = self.inner.cfg.home.as_deref().unwrap_or("/").trim();
        PathBuf::from(format!("/{}", h.trim_matches('/')))
    }

    fn atomic_writes(&self) -> bool {
        true
    }

    fn list<'a>(
        &'a self,
        path: &'a Path,
    ) -> BoxFuture<'a, std::result::Result<Vec<FileEntry>, FsError>> {
        Box::pin(async move {
            let (bucket, key) = split(path);
            Ok(match bucket {
                None => self.inner.list_buckets().await?,
                Some(b) => self.inner.list(&b, &key).await?,
            })
        })
    }

    fn mkdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, std::result::Result<(), FsError>> {
        Box::pin(async move {
            match split(path) {
                (Some(b), k) if k.is_empty() => Ok(self.inner.create_bucket(&b).await?),
                (Some(b), k) => Ok(self.inner.put(&b, &format!("{k}/"), Vec::new()).await?),
                (None, _) => Err(FsError::Unsupported("creating the root")),
            }
        })
    }

    fn rename<'a>(
        &'a self,
        from: &'a Path,
        to: &'a Path,
    ) -> BoxFuture<'a, std::result::Result<(), FsError>> {
        Box::pin(async move {
            let (fb, fk) = split(from);
            let (tb, tk) = split(to);
            let fb = need_key(fb, &fk, "renaming buckets")?;
            let tb = need_key(tb, &tk, "renaming buckets")?;
            if self.inner.stat(Some(&fb), &fk).await?.is_dir() {
                return Err(FsError::Unsupported(
                    "renaming folders on object storage (copy them instead)",
                ));
            }
            self.inner.copy((&fb, &fk), (&tb, &tk)).await?;
            self.inner.delete(&fb, &fk).await?;
            Ok(())
        })
    }

    fn delete<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, std::result::Result<(), FsError>> {
        Box::pin(async move {
            match split(path) {
                (Some(b), k) if k.is_empty() => {
                    self.inner
                        .call(Some(&b), |r| {
                            self.inner.req(Method::DELETE, Some(&b), "", r)
                        })
                        .await?;
                    Ok(())
                }
                (Some(b), k) => {
                    let is_dir = match self.inner.stat(Some(&b), &k).await {
                        Ok(e) => e.is_dir(),
                        Err(e) if e.is_not_found() => return Ok(()),
                        Err(e) => return Err(e.into()),
                    };
                    let key = if is_dir { format!("{k}/") } else { k };
                    Ok(self.inner.delete(&b, &key).await?)
                }
                (None, _) => Err(FsError::Unsupported("deleting the root")),
            }
        })
    }

    fn stat<'a>(
        &'a self,
        path: &'a Path,
    ) -> BoxFuture<'a, std::result::Result<FileEntry, FsError>> {
        Box::pin(async move {
            let (b, k) = split(path);
            Ok(self.inner.stat(b.as_deref(), &k).await?)
        })
    }

    fn open_read<'a>(
        &'a self,
        path: &'a Path,
    ) -> BoxFuture<'a, std::result::Result<FsReader, FsError>> {
        self.open_read_from(path, 0)
    }

    fn create<'a>(
        &'a self,
        path: &'a Path,
    ) -> BoxFuture<'a, std::result::Result<FsWriter, FsError>> {
        Box::pin(async move {
            if self.inner.cfg.read_only {
                return Err(FsError::Remote(CloudError::ReadOnly.to_string()));
            }
            let (b, k) = split(path);
            let bucket = need_key(b, &k, "writing outside a bucket")?;
            Ok(upload_writer(Arc::new(Upload {
                inner: self.inner.clone(),
                bucket,
                key: k,
            })))
        })
    }

    fn open_read_from<'a>(
        &'a self,
        path: &'a Path,
        offset: u64,
    ) -> BoxFuture<'a, std::result::Result<FsReader, FsError>> {
        Box::pin(async move {
            let (b, k) = split(path);
            let bucket = need_key(b, &k, "reading a bucket as a file")?;
            let resp = self
                .inner
                .call_raw(Some(&bucket), |r| {
                    let req = self.inner.req(Method::GET, Some(&bucket), &k, r)?;
                    Ok(if offset > 0 {
                        req.header("range", format!("bytes={offset}-"))
                    } else {
                        req
                    })
                })
                .await?;
            if !resp.status().is_success() {
                let r = Resp::read(resp).await?;
                return Err(s3_error(r.status, &r.body).into());
            }
            Ok(body_reader(resp))
        })
    }

    fn open_write_from<'a>(
        &'a self,
        path: &'a Path,
        offset: u64,
    ) -> BoxFuture<'a, std::result::Result<FsWriter, FsError>> {
        if offset == 0 {
            return self.create(path);
        }
        Box::pin(async { Err(FsError::Unsupported("resuming uploads to object storage")) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(split(Path::new("/")), (None, String::new()));
        assert_eq!(split(Path::new("/b")), (Some("b".into()), String::new()));
        assert_eq!(
            split(Path::new("/b/x/y.txt")),
            (Some("b".into()), "x/y.txt".into())
        );
        assert_eq!(split(Path::new("b/x/")), (Some("b".into()), "x".into()));
        assert_eq!(last_segment("a/b/c/"), "c");
        assert_eq!(
            guess_content_type("site/index.HTML"),
            "text/html; charset=utf-8"
        );
    }

    fn fs(endpoint: Option<&str>) -> S3Fs {
        let auth = Arc::new(AwsAuth::new(crate::aws::AwsSource::Keys(
            crate::sigv4::AwsCredentials {
                access_key_id: "AK".into(),
                secret_access_key: secrecy::SecretString::from("s".to_owned()),
                session_token: None,
                expires_ms: None,
            },
        )));
        S3Fs::new(
            S3Config {
                name: "t".into(),
                endpoint: endpoint.map(|e| url::Url::parse(e).unwrap()),
                region: "eu-west-1".into(),
                home: Some("/bucket/dir/".into()),
                read_only: false,
                service: "S3",
            },
            auth,
        )
        .unwrap()
    }

    #[test]
    fn addressing() {
        let f = fs(None);
        let r = f
            .inner
            .req(Method::GET, Some("bkt"), "a b/c+d.txt", "eu-west-1")
            .unwrap();
        assert_eq!(
            r.url().unwrap().as_str(),
            "https://bkt.s3.eu-west-1.amazonaws.com/a%20b/c%2Bd.txt"
        );
        let r = f
            .inner
            .req(Method::GET, Some("my.bucket"), "k", "us-east-1")
            .unwrap();
        assert_eq!(
            r.url().unwrap().as_str(),
            "https://s3.amazonaws.com/my.bucket/k"
        );
        let f = fs(Some("http://127.0.0.1:9000"));
        let r = f.inner.req(Method::GET, Some("b"), "", "auto").unwrap();
        assert_eq!(r.url().unwrap().as_str(), "http://127.0.0.1:9000/b");
        assert_eq!(f.home(), PathBuf::from("/bucket/dir"));
        let r2 = S3Config::r2("r2", "0123456789abcdef0123456789abcdef").unwrap();
        assert_eq!(
            r2.endpoint.unwrap().as_str(),
            "https://0123456789abcdef0123456789abcdef.r2.cloudflarestorage.com/"
        );
        assert!(S3Config::r2("r2", "bad id").is_err());
    }

    #[test]
    fn errors() {
        let e = s3_error(
            404,
            b"<Error><Code>NoSuchBucket</Code><Message>The specified bucket does not exist</Message></Error>",
        );
        assert!(e.is_not_found());
        assert!(e.to_string().contains("No such bucket"));
        assert_eq!(
            s3_error(403, b"").to_string(),
            "Access denied (403): the credentials lack permission for this"
        );
    }
}
