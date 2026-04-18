use std::{fmt, fs::File, path::Path, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use reqwest::{
    StatusCode,
    blocking::{Body, Client, RequestBuilder, Response},
    header::{AUTHORIZATION, RANGE},
    redirect::Policy,
};
use serde::Deserialize;
use thiserror::Error;

use crate::auth::{AuthError, AuthManager};

const API_BASE: &str = "https://cloud-api.yandex.net/v1/disk";
const USER_AGENT: &str = "discohack-daemon/0.1.0";

pub trait AccessTokenProvider: Send + Sync {
    fn access_token(&self) -> Result<String, YandexError>;
    fn refresh_access_token(&self) -> Result<String, YandexError>;
}

impl AccessTokenProvider for AuthManager {
    fn access_token(&self) -> Result<String, YandexError> {
        self.access_token().map_err(YandexError::from)
    }

    fn refresh_access_token(&self) -> Result<String, YandexError> {
        self.refresh_access_token().map_err(YandexError::from)
    }
}

#[derive(Clone)]
pub struct YandexDiskClient {
    api: Client,
    transfers: Client,
    auth: Arc<dyn AccessTokenProvider>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Directory,
    File,
}

#[derive(Debug, Clone)]
pub struct ResourceEntry {
    pub path: String,
    pub name: String,
    pub kind: ResourceKind,
    pub size: u64,
    pub created: Option<std::time::SystemTime>,
    pub modified: Option<std::time::SystemTime>,
    pub remote_version: Option<String>,
}

#[derive(Debug, Error)]
pub enum YandexError {
    #[error("Yandex Disk entry not found")]
    NotFound,
    #[error("Yandex Disk authentication failed")]
    Unauthorized,
    #[error("Yandex Disk access denied")]
    Forbidden,
    #[error("Yandex Disk reported a resource conflict: {0}")]
    Conflict(String),
    #[error("authentication unavailable: {0}")]
    Auth(#[from] AuthError),
    #[error("local I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid Yandex Disk response: {0}")]
    InvalidResponse(String),
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Yandex Disk returned {status}: {body}")]
    Status { status: StatusCode, body: String },
}

impl YandexDiskClient {
    pub fn new(auth: Arc<dyn AccessTokenProvider>) -> Result<Self, YandexError> {
        let api = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(60))
            .build()?;

        let transfers = Client::builder()
            .user_agent(USER_AGENT)
            .redirect(Policy::limited(10))
            .timeout(Duration::from_secs(300))
            .build()?;

        Ok(Self {
            api,
            transfers,
            auth,
        })
    }

    pub fn fetch_resource_metadata(&self, path: &str) -> Result<ResourceEntry, YandexError> {
        let response: ApiResource = self.send_json(|client| {
            client
                .get(format!("{API_BASE}/resources"))
                .query(&[("path", path)])
        })?;

        response.try_into_resource()
    }

    pub fn list_directory(&self, path: &str) -> Result<Vec<ResourceEntry>, YandexError> {
        let mut offset = 0usize;
        let limit = 1000usize;
        let mut items = Vec::new();

        loop {
            let response: ApiResource = self.send_json(|client| {
                client.get(format!("{API_BASE}/resources")).query(&[
                    ("path", path),
                    ("limit", &limit.to_string()),
                    ("offset", &offset.to_string()),
                ])
            })?;

            let embedded = response.embedded.ok_or_else(|| {
                YandexError::InvalidResponse(format!("directory {path} is missing _embedded"))
            })?;

            let batch_size = embedded.items.len();
            let total = embedded.total.unwrap_or(batch_size);

            for item in embedded.items {
                items.push(item.try_into_resource()?);
            }

            offset += batch_size;
            if batch_size == 0 || offset >= total {
                break;
            }
        }

        Ok(items)
    }

    pub fn create_directory(&self, path: &str) -> Result<(), YandexError> {
        let response = self.send_authorized_request(|client| {
            client
                .put(format!("{API_BASE}/resources"))
                .query(&[("path", path)])
        })?;
        Self::expect_success_or_operation(response)?;
        Ok(())
    }

    pub fn delete_resource(&self, path: &str, permanently: bool) -> Result<(), YandexError> {
        let response = self.send_authorized_request(|client| {
            client.delete(format!("{API_BASE}/resources")).query(&[
                ("path", path),
                ("permanently", if permanently { "true" } else { "false" }),
            ])
        })?;

        let status = response.status();
        if status == StatusCode::NO_CONTENT {
            return Ok(());
        }

        Self::expect_success_or_operation(response)?;
        Ok(())
    }

    pub fn move_resource(&self, from: &str, to: &str, overwrite: bool) -> Result<(), YandexError> {
        let response = self.send_authorized_request(|client| {
            client.post(format!("{API_BASE}/resources/move")).query(&[
                ("from", from),
                ("path", to),
                ("overwrite", if overwrite { "true" } else { "false" }),
            ])
        })?;
        Self::expect_success_or_operation(response)?;
        Ok(())
    }

    pub fn resolve_download_url(&self, path: &str) -> Result<String, YandexError> {
        let response: TransferLink = self.send_json(|client| {
            client
                .get(format!("{API_BASE}/resources/download"))
                .query(&[("path", path)])
        })?;

        expect_non_empty_href(response.href, "download")
    }

    pub fn resolve_upload_url(&self, path: &str, overwrite: bool) -> Result<String, YandexError> {
        let response: TransferLink = self.send_json(|client| {
            client.get(format!("{API_BASE}/resources/upload")).query(&[
                ("path", path),
                ("overwrite", if overwrite { "true" } else { "false" }),
            ])
        })?;

        expect_non_empty_href(response.href, "upload")
    }

    pub fn upload_file(&self, href: &str, local_path: &Path) -> Result<(), YandexError> {
        let file = File::open(local_path)?;
        let response = self.transfers.put(href).body(Body::new(file)).send()?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }

        Err(Self::error_from_response(response)?)
    }

    pub fn download_file(&self, href: &str) -> Result<Vec<u8>, YandexError> {
        let response = self.transfers.get(href).send()?;
        let status = response.status();
        if !status.is_success() {
            return Err(Self::error_from_response(response)?);
        }

        Ok(response.bytes()?.to_vec())
    }

    pub fn read_file_range(
        &self,
        href: &str,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>, YandexError> {
        if size == 0 {
            return Ok(Vec::new());
        }

        let end = offset.saturating_add(size as u64).saturating_sub(1);
        let response = self
            .transfers
            .get(href)
            .header(RANGE, format!("bytes={offset}-{end}"))
            .send()?;

        let status = response.status();
        if status == StatusCode::PARTIAL_CONTENT {
            return Ok(response.bytes()?.to_vec());
        }

        if status == StatusCode::RANGE_NOT_SATISFIABLE {
            return Ok(Vec::new());
        }

        if !status.is_success() {
            return Err(Self::error_from_response(response)?);
        }

        let body = response.bytes()?;
        let start = offset as usize;
        if start >= body.len() {
            return Ok(Vec::new());
        }

        let end = (start + size as usize).min(body.len());
        Ok(body[start..end].to_vec())
    }

    fn send_json<T: for<'de> Deserialize<'de>, F>(&self, build: F) -> Result<T, YandexError>
    where
        F: Fn(&Client) -> RequestBuilder,
    {
        let response = self.send_authorized_request(build)?;
        let status = response.status();
        if !status.is_success() {
            return Err(Self::error_from_response(response)?);
        }

        Ok(response.json()?)
    }

    fn send_authorized_request<F>(&self, build: F) -> Result<Response, YandexError>
    where
        F: Fn(&Client) -> RequestBuilder,
    {
        let token = self.auth.access_token()?;
        let response = build(&self.api)
            .header(AUTHORIZATION, format!("OAuth {token}"))
            .send()?;

        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }

        let token = self.auth.refresh_access_token()?;
        Ok(build(&self.api)
            .header(AUTHORIZATION, format!("OAuth {token}"))
            .send()?)
    }

    fn expect_success_or_operation(response: Response) -> Result<TransferLink, YandexError> {
        let status = response.status();
        if status.is_success() {
            if status == StatusCode::NO_CONTENT {
                return Ok(TransferLink {
                    href: String::new(),
                    method: String::new(),
                    templated: false,
                });
            }

            let link: TransferLink = response.json()?;
            return Ok(link);
        }

        Err(Self::error_from_response(response)?)
    }

    fn error_from_response(response: Response) -> Result<YandexError, YandexError> {
        let status = response.status();
        let body = response
            .text()
            .unwrap_or_else(|_| String::from("<unable to read body>"));
        let error = match status {
            StatusCode::NOT_FOUND => YandexError::NotFound,
            StatusCode::UNAUTHORIZED => YandexError::Unauthorized,
            StatusCode::FORBIDDEN => YandexError::Forbidden,
            StatusCode::CONFLICT => YandexError::Conflict(body),
            _ => YandexError::Status { status, body },
        };
        Err(error)
    }
}

#[derive(Debug, Deserialize)]
struct ApiResource {
    path: String,
    name: String,
    #[serde(rename = "type")]
    kind: String,
    size: Option<u64>,
    created: Option<String>,
    modified: Option<String>,
    revision: Option<serde_json::Value>,
    etag: Option<String>,
    #[serde(rename = "_embedded")]
    embedded: Option<ApiEmbedded>,
}

#[derive(Debug, Deserialize)]
struct ApiEmbedded {
    items: Vec<ApiResource>,
    total: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct TransferLink {
    href: String,
    method: String,
    templated: bool,
}

impl ApiResource {
    fn try_into_resource(self) -> Result<ResourceEntry, YandexError> {
        let kind = match self.kind.as_str() {
            "dir" => ResourceKind::Directory,
            "file" => ResourceKind::File,
            other => {
                return Err(YandexError::InvalidResponse(format!(
                    "unsupported resource type {other} for {}",
                    self.path
                )));
            }
        };

        Ok(ResourceEntry {
            path: self.path,
            name: self.name,
            kind,
            size: self.size.unwrap_or(0),
            created: parse_time(self.created.as_deref()),
            modified: parse_time(self.modified.as_deref()),
            remote_version: self
                .revision
                .map(|value| match value {
                    serde_json::Value::String(raw) => raw,
                    other => other.to_string(),
                })
                .or(self.etag),
        })
    }
}

fn expect_non_empty_href(href: String, operation: &str) -> Result<String, YandexError> {
    if href.trim().is_empty() {
        return Err(YandexError::InvalidResponse(format!(
            "{operation} URL is empty"
        )));
    }

    Ok(href)
}

fn parse_time(value: Option<&str>) -> Option<std::time::SystemTime> {
    let raw = value?;
    let parsed = DateTime::parse_from_rfc3339(raw).ok()?;
    let utc: DateTime<Utc> = parsed.with_timezone(&Utc);
    Some(utc.into())
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResourceKind::Directory => write!(f, "dir"),
            ResourceKind::File => write!(f, "file"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_blank_transfer_href() {
        let err = expect_non_empty_href(String::from("   "), "upload").unwrap_err();
        assert!(matches!(err, YandexError::InvalidResponse(_)));
    }

    #[test]
    fn reject_unsupported_resource_type() {
        let err = ApiResource {
            path: String::from("disk:/weird"),
            name: String::from("weird"),
            kind: String::from("weird"),
            size: None,
            created: None,
            modified: None,
            revision: None,
            etag: None,
            embedded: None,
        }
        .try_into_resource()
        .unwrap_err();

        assert!(matches!(err, YandexError::InvalidResponse(_)));
    }
}
