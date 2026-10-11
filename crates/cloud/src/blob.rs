//! Azure Blob Storage as a [`RemoteFs`]: `/` lists containers, `/container/a/b` are blobs
//! under the virtual folder `a/b`.
//!
//! Folders are name prefixes; New folder writes an empty `prefix/` blob. Rename copies
//! then deletes (files only). Uploads are one Put Blob up to 8 MiB, else Put Block + Put
//! Block List.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use data_encoding::BASE64;
use futures::future::BoxFuture;
use reqwest::Method;
use secrecy::ExposeSecret as _;
use switchyard_remote::{EntryKind, FileEntry, FsError, FsReader, FsWriter, ListPage, RemoteFs};

use crate::azure::{AzureAuth, authorize};
use crate::error::{CloudError, Result};
use crate::http::{Req, Resp, client, uri_encode};
use crate::s3::guess_content_type;
use crate::stream::{ChunkSink, body_reader, upload_writer};
use crate::time::http_date;
use crate::xml;

/// Storage REST API version.
pub const API_VERSION: &str = "2023-11-03";
const MAX_LIST: usize = 100_000;

/// A storage account's blob service.
#[derive(Clone, Debug)]
pub struct BlobConfig {
    /// Name shown in the Files tab.
    pub name: String,
    /// `https://<account>.blob.core.windows.net` (or an emulator URL with the account in
    /// its path).
    pub endpoint: url::Url,
    /// Folder opened first (`/container/prefix`).
    pub home: Option<String>,
    /// Refuse every change.
    pub read_only: bool,
}

struct Inner {
    cfg: BlobConfig,
    auth: AzureAuth,
    http: reqwest::Client,
}

/// Blob storage as a file system.
#[derive(Clone)]
pub struct BlobFs {
    inner: Arc<Inner>,
}

fn split(path: &Path) -> (Option<String>, String) {
    let s = path.to_string_lossy().replace('\\', "/");
    let s = s.trim_matches('/');
    if s.is_empty() {
        return (None, String::new());
    }
    match s.split_once('/') {
        Some((c, k)) => (Some(c.to_owned()), k.trim_matches('/').to_owned()),
        None => (Some(s.to_owned()), String::new()),
    }
}

fn last_segment(name: &str) -> String {
    name.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// The `NextMarker` of a list response, when there are more results.
fn next_marker(root: &xml::Node) -> Option<String> {
    root.text_of("NextMarker")
        .filter(|m| !m.is_empty())
        .map(str::to_owned)
}

fn blob_error(status: u16, headers: &reqwest::header::HeaderMap, body: &[u8]) -> CloudError {
    let header_code = headers
        .get("x-ms-error-code")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    match xml::parse(body) {
        Ok(e) if e.name == "Error" => {
            let msg = e
                .text_of("Message")
                .unwrap_or_default()
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned();
            let code = e.text_of("Code").map(str::to_owned).or(header_code);
            let msg = match code.as_deref() {
                Some("AuthenticationFailed" | "AuthorizationFailure") => {
                    "The account key or signature was refused".into()
                }
                Some("AuthorizationPermissionMismatch") => "Signed in, but this account has no \
                     data role on the storage account (needs Storage Blob Data Reader or \
                     Contributor)"
                    .into(),
                _ => msg,
            };
            CloudError::api(status, code, msg)
        }
        _ => CloudError::api(status, header_code, ""),
    }
}

impl BlobFs {
    /// A file system over `cfg`.
    pub fn new(cfg: BlobConfig, auth: AzureAuth) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(Inner {
                cfg,
                auth,
                http: client()?,
            }),
        })
    }

    /// `Azure Blob · acme.blob.core.windows.net`.
    pub fn describe(&self) -> String {
        format!(
            "Azure Blob · {}",
            self.inner.cfg.endpoint.host_str().unwrap_or_default()
        )
    }
}

impl Inner {
    fn req(&self, method: Method, container: Option<&str>, blob: &str) -> Req {
        let path = match container {
            None => "/".to_owned(),
            Some(c) if blob.is_empty() => format!("/{}", uri_encode(c, false)),
            Some(c) => format!("/{}/{}", uri_encode(c, false), uri_encode(blob, true)),
        };
        Req::new(method, &self.cfg.endpoint, path)
    }

    async fn send_raw(&self, mut req: Req) -> Result<reqwest::Response> {
        if self.cfg.read_only && !matches!(req.method, Method::GET | Method::HEAD) {
            return Err(CloudError::ReadOnly);
        }
        req.set_header("x-ms-date", http_date(SystemTime::now()));
        req.set_header("x-ms-version", API_VERSION);
        if !req.body.is_empty() || matches!(req.method, Method::PUT | Method::POST) {
            req.set_header("content-length", req.body.len().to_string());
        }
        authorize(&mut req, &self.auth).await?;
        req.send(&self.http).await
    }

    async fn send(&self, req: Req) -> Result<Resp> {
        let resp = Resp::read(self.send_raw(req).await?).await?;
        if !resp.ok() {
            return Err(blob_error(resp.status, &resp.headers, &resp.body));
        }
        Ok(resp)
    }

    /// One page of containers whose names start with `prefix`.
    async fn containers_page(
        &self,
        prefix: &str,
        max: Option<u32>,
        marker: Option<&str>,
    ) -> Result<ListPage> {
        let mut req = self.req(Method::GET, None, "").query("comp", "list");
        if !prefix.is_empty() {
            req = req.query("prefix", prefix);
        }
        if let Some(m) = max {
            req = req.query("maxresults", m.to_string());
        }
        if let Some(m) = marker {
            req = req.query("marker", m);
        }
        let root = xml::parse(&self.send(req).await?.body)?;
        let mut entries = Vec::new();
        if let Some(cs) = root.child("Containers") {
            for c in cs.all("Container") {
                if let Some(name) = c.text_of("Name") {
                    entries.push(FileEntry {
                        name: name.to_owned(),
                        kind: EntryKind::Dir,
                        size: 0,
                        modified_ms: c
                            .path(&["Properties", "Last-Modified"])
                            .and_then(crate::time::parse_http_date_ms),
                        mode: None,
                    });
                }
            }
        }
        Ok(ListPage {
            entries,
            next: next_marker(&root),
        })
    }

    async fn list_containers(&self) -> Result<Vec<FileEntry>> {
        let mut out = Vec::new();
        let mut marker: Option<String> = None;
        loop {
            let page = self.containers_page("", None, marker.as_deref()).await?;
            out.extend(page.entries);
            marker = page.next;
            if marker.is_none() {
                break;
            }
        }
        switchyard_remote::fs::sort_entries(&mut out);
        Ok(out)
    }

    async fn list_page(
        &self,
        container: &str,
        prefix: &str,
        delimiter: bool,
        max: Option<u32>,
        marker: Option<&str>,
    ) -> Result<xml::Node> {
        let mut req = self
            .req(Method::GET, Some(container), "")
            .query("restype", "container")
            .query("comp", "list")
            .query("prefix", prefix);
        if delimiter {
            req = req.query("delimiter", "/");
        }
        if let Some(m) = max {
            req = req.query("maxresults", m.to_string());
        }
        if let Some(m) = marker {
            req = req.query("marker", m);
        }
        xml::parse(&self.send(req).await?.body)
    }

    /// One page of folders and blobs in `dir` whose names start with `name_prefix`.
    async fn blobs_page(
        &self,
        container: &str,
        dir: &str,
        name_prefix: &str,
        max: Option<u32>,
        marker: Option<&str>,
    ) -> Result<ListPage> {
        let prefix = if dir.is_empty() {
            name_prefix.to_owned()
        } else {
            format!("{dir}/{name_prefix}")
        };
        let page = self
            .list_page(container, &prefix, true, max, marker)
            .await?;
        let mut entries = Vec::new();
        if let Some(blobs) = page.child("Blobs") {
            for p in blobs.all("BlobPrefix") {
                let name = p.text_of("Name").map(last_segment).unwrap_or_default();
                if !name.is_empty() {
                    entries.push(FileEntry {
                        name,
                        kind: EntryKind::Dir,
                        size: 0,
                        modified_ms: None,
                        mode: None,
                    });
                }
            }
            for b in blobs.all("Blob") {
                let Some(name) = b.text_of("Name") else {
                    continue;
                };
                if name.ends_with('/') {
                    continue;
                }
                entries.push(FileEntry {
                    name: last_segment(name),
                    kind: EntryKind::File,
                    size: b
                        .path(&["Properties", "Content-Length"])
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0),
                    modified_ms: b
                        .path(&["Properties", "Last-Modified"])
                        .and_then(crate::time::parse_http_date_ms),
                    mode: None,
                });
            }
        }
        Ok(ListPage {
            entries,
            next: next_marker(&page),
        })
    }

    async fn list(&self, container: &str, dir: &str) -> Result<Vec<FileEntry>> {
        let mut out = Vec::new();
        let mut marker: Option<String> = None;
        loop {
            let page = self
                .blobs_page(container, dir, "", None, marker.as_deref())
                .await?;
            out.extend(page.entries);
            marker = page.next;
            if marker.is_none() || out.len() >= MAX_LIST {
                break;
            }
        }
        switchyard_remote::fs::sort_entries(&mut out);
        Ok(out)
    }

    async fn stat(&self, container: Option<&str>, blob: &str) -> Result<FileEntry> {
        let dir = |name: &str| FileEntry {
            name: name.to_owned(),
            kind: EntryKind::Dir,
            size: 0,
            modified_ms: None,
            mode: None,
        };
        let Some(c) = container else {
            return Ok(dir("/"));
        };
        if blob.is_empty() {
            let resp = Resp::read(
                self.send_raw(
                    self.req(Method::HEAD, Some(c), "")
                        .query("restype", "container"),
                )
                .await?,
            )
            .await?;
            return match resp.status {
                200..=299 | 403 => Ok(dir(c)),
                s => Err(blob_error(s, &resp.headers, &resp.body)),
            };
        }
        let resp = Resp::read(self.send_raw(self.req(Method::HEAD, Some(c), blob)).await?).await?;
        if resp.ok() {
            return Ok(FileEntry {
                name: last_segment(blob),
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
            return Err(blob_error(resp.status, &resp.headers, &resp.body));
        }
        let page = self
            .list_page(c, &format!("{blob}/"), false, Some(1), None)
            .await?;
        if page
            .child("Blobs")
            .is_some_and(|b| b.child("Blob").is_some())
        {
            Ok(dir(&last_segment(blob)))
        } else {
            Err(CloudError::api(
                404,
                Some("BlobNotFound".into()),
                format!("{blob} not found"),
            ))
        }
    }

    async fn put(&self, container: &str, blob: &str, data: Vec<u8>) -> Result<()> {
        self.send(
            self.req(Method::PUT, Some(container), blob)
                .header("x-ms-blob-type", "BlockBlob")
                .header("x-ms-blob-content-type", guess_content_type(blob))
                .body(data),
        )
        .await?;
        Ok(())
    }

    async fn delete(&self, container: &str, blob: &str) -> Result<()> {
        self.send(self.req(Method::DELETE, Some(container), blob))
            .await?;
        Ok(())
    }

    /// The URL another request can copy `blob` from (with the SAS when that is the auth).
    fn source_url(&self, container: &str, blob: &str) -> Result<String> {
        let mut u = self.req(Method::GET, Some(container), blob).url()?;
        if let AzureAuth::Sas(sas) = &self.auth {
            u.set_query(Some(sas.expose_secret()));
        }
        Ok(u.to_string())
    }

    async fn copy(&self, from: (&str, &str), to: (&str, &str)) -> Result<()> {
        let source = self.source_url(from.0, from.1)?;
        let resp = self
            .send(
                self.req(Method::PUT, Some(to.0), to.1)
                    .header("x-ms-copy-source", source),
            )
            .await?;
        let mut status = resp.header("x-ms-copy-status").map(str::to_owned);
        // Copies inside an account usually finish at once; otherwise wait (bounded).
        for _ in 0..600 {
            match status.as_deref() {
                Some("success") | None => return Ok(()),
                Some("pending") => {}
                Some(other) => {
                    return Err(CloudError::api(500, None, format!("copy {other}")));
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            let r = self.send(self.req(Method::HEAD, Some(to.0), to.1)).await?;
            status = r.header("x-ms-copy-status").map(str::to_owned);
        }
        Err(CloudError::Network(
            "the copy did not finish in 5 minutes".into(),
        ))
    }
}

/// Block id for part `n`: same length for every block, as Azure requires.
fn block_id(n: u32) -> String {
    BASE64.encode(format!("swy-{n:08}").as_bytes())
}

struct Upload {
    inner: Arc<Inner>,
    container: String,
    blob: String,
}

impl ChunkSink for Upload {
    fn put_whole(&self, data: Vec<u8>) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.inner.put(&self.container, &self.blob, data))
    }

    fn begin(&self) -> BoxFuture<'_, Result<String>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn put_part<'a>(
        &'a self,
        _upload: &'a str,
        n: u32,
        data: Vec<u8>,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let id = block_id(n);
            self.inner
                .send(
                    self.inner
                        .req(Method::PUT, Some(&self.container), &self.blob)
                        .query("comp", "block")
                        .query("blockid", id.clone())
                        .body(data),
                )
                .await?;
            Ok(id)
        })
    }

    fn complete<'a>(&'a self, _upload: &'a str, parts: Vec<String>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut body = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList>");
            for id in &parts {
                body.push_str(&format!("<Latest>{id}</Latest>"));
            }
            body.push_str("</BlockList>");
            self.inner
                .send(
                    self.inner
                        .req(Method::PUT, Some(&self.container), &self.blob)
                        .query("comp", "blocklist")
                        .header("x-ms-blob-content-type", guess_content_type(&self.blob))
                        .body(body.into_bytes()),
                )
                .await?;
            Ok(())
        })
    }

    /// Uncommitted blocks are discarded by the service after a week.
    fn abort<'a>(&'a self, _upload: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

fn need_blob(container: Option<String>, blob: &str, what: &'static str) -> Result<String> {
    match container {
        Some(c) if !blob.is_empty() => Ok(c),
        _ => Err(CloudError::Unsupported(what)),
    }
}

type FsResult<T> = std::result::Result<T, FsError>;

impl RemoteFs for BlobFs {
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

    fn list<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<Vec<FileEntry>>> {
        Box::pin(async move {
            Ok(match split(path) {
                (None, _) => self.inner.list_containers().await?,
                (Some(c), d) => self.inner.list(&c, &d).await?,
            })
        })
    }

    fn list_page<'a>(
        &'a self,
        path: &'a Path,
        prefix: &'a str,
        cursor: Option<&'a str>,
        limit: u32,
    ) -> BoxFuture<'a, FsResult<ListPage>> {
        Box::pin(async move {
            let limit = Some(limit.clamp(1, 5000));
            let mut page = match split(path) {
                (None, _) => self.inner.containers_page(prefix, limit, cursor).await?,
                (Some(c), d) => self.inner.blobs_page(&c, &d, prefix, limit, cursor).await?,
            };
            switchyard_remote::fs::sort_entries(&mut page.entries);
            Ok(page)
        })
    }

    fn mkdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<()>> {
        Box::pin(async move {
            match split(path) {
                (Some(c), d) if d.is_empty() => {
                    self.inner
                        .send(
                            self.inner
                                .req(Method::PUT, Some(&c), "")
                                .query("restype", "container"),
                        )
                        .await?;
                    Ok(())
                }
                (Some(c), d) => Ok(self.inner.put(&c, &format!("{d}/"), Vec::new()).await?),
                (None, _) => Err(FsError::Unsupported("creating the root")),
            }
        })
    }

    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, FsResult<()>> {
        Box::pin(async move {
            let (fc, fb) = split(from);
            let (tc, tb) = split(to);
            let fc = need_blob(fc, &fb, "renaming containers")?;
            let tc = need_blob(tc, &tb, "renaming containers")?;
            if self.inner.stat(Some(&fc), &fb).await?.is_dir() {
                return Err(FsError::Unsupported(
                    "renaming folders on object storage (copy them instead)",
                ));
            }
            self.inner.copy((&fc, &fb), (&tc, &tb)).await?;
            self.inner.delete(&fc, &fb).await?;
            Ok(())
        })
    }

    fn delete<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<()>> {
        Box::pin(async move {
            match split(path) {
                (Some(c), b) if b.is_empty() => {
                    self.inner
                        .send(
                            self.inner
                                .req(Method::DELETE, Some(&c), "")
                                .query("restype", "container"),
                        )
                        .await?;
                    Ok(())
                }
                (Some(c), b) => {
                    let is_dir = match self.inner.stat(Some(&c), &b).await {
                        Ok(e) => e.is_dir(),
                        Err(e) if e.is_not_found() => return Ok(()),
                        Err(e) => return Err(e.into()),
                    };
                    if !is_dir {
                        return Ok(self.inner.delete(&c, &b).await?);
                    }
                    // The folder marker, when there is one.
                    match self.inner.delete(&c, &format!("{b}/")).await {
                        Err(e) if !e.is_not_found() => Err(e.into()),
                        _ => Ok(()),
                    }
                }
                (None, _) => Err(FsError::Unsupported("deleting the root")),
            }
        })
    }

    fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<FileEntry>> {
        Box::pin(async move {
            let (c, b) = split(path);
            Ok(self.inner.stat(c.as_deref(), &b).await?)
        })
    }

    fn open_read<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<FsReader>> {
        self.open_read_from(path, 0)
    }

    fn create<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, FsResult<FsWriter>> {
        Box::pin(async move {
            if self.inner.cfg.read_only {
                return Err(FsError::Remote(CloudError::ReadOnly.to_string()));
            }
            let (c, b) = split(path);
            let container = need_blob(c, &b, "writing outside a container")?;
            Ok(upload_writer(Arc::new(Upload {
                inner: self.inner.clone(),
                container,
                blob: b,
            })))
        })
    }

    fn open_read_from<'a>(
        &'a self,
        path: &'a Path,
        offset: u64,
    ) -> BoxFuture<'a, FsResult<FsReader>> {
        Box::pin(async move {
            let (c, b) = split(path);
            let container = need_blob(c, &b, "reading a container as a file")?;
            let mut req = self.inner.req(Method::GET, Some(&container), &b);
            if offset > 0 {
                req = req.header("x-ms-range", format!("bytes={offset}-"));
            }
            let resp = self.inner.send_raw(req).await?;
            if !resp.status().is_success() {
                let r = Resp::read(resp).await?;
                return Err(blob_error(r.status, &r.headers, &r.body).into());
            }
            Ok(body_reader(resp))
        })
    }

    fn open_write_from<'a>(
        &'a self,
        path: &'a Path,
        offset: u64,
    ) -> BoxFuture<'a, FsResult<FsWriter>> {
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
    fn block_ids_have_one_length() {
        assert_eq!(block_id(1).len(), block_id(99_999).len());
    }

    #[test]
    fn urls() {
        let fs = BlobFs::new(
            BlobConfig {
                name: "x".into(),
                endpoint: url::Url::parse("http://127.0.0.1:10000/devstoreaccount1").unwrap(),
                home: None,
                read_only: false,
            },
            AzureAuth::Sas(secrecy::SecretString::from("sv=1&sig=a".to_owned())),
        )
        .unwrap();
        let r = fs.inner.req(Method::GET, Some("c"), "a b/x.txt");
        assert_eq!(
            r.url().unwrap().as_str(),
            "http://127.0.0.1:10000/devstoreaccount1/c/a%20b/x.txt"
        );
        assert_eq!(r.full_path(), "/devstoreaccount1/c/a%20b/x.txt");
        assert_eq!(
            fs.inner.source_url("c", "k").unwrap(),
            "http://127.0.0.1:10000/devstoreaccount1/c/k?sv=1&sig=a"
        );
        assert_eq!(fs.home(), PathBuf::from("/"));
    }
}
