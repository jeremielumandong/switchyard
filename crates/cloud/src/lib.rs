//! Cloud storage and developer services over their REST APIs.
//!
//! - Object storage as a [`switchyard_remote::RemoteFs`]: Amazon S3 and S3-compatible
//!   endpoints including Cloudflare R2 ([`s3::S3Fs`]), and Azure Blob Storage
//!   ([`blob::BlobFs`]). Buckets and containers are the top-level folders.
//! - Key / value tools behind one trait ([`kv::KvService`]): Azure App Configuration (with
//!   labels, locks and feature flags), Azure Key Vault secrets, AWS Secrets Manager, AWS
//!   Systems Manager Parameter Store and Cloudflare Workers KV.
//!
//! Requests are signed here (AWS Signature V4, Azure Shared Key, App Configuration HMAC);
//! sign-in that needs a person (Microsoft Entra) happens in `core`, which hands the clients a
//! [`auth::TokenSource`].

pub mod appconfig;
pub mod auth;
pub mod aws;
pub mod azure;
pub mod blob;
pub mod cloudflare;
mod error;
mod http;
pub mod keyvault;
pub mod kv;
pub mod parameters;
pub mod s3;
pub mod secrets_manager;
pub mod sigv4;
mod stream;
mod time;
pub mod workers_kv;
mod xml;

pub use error::{CloudError, Result};
pub use kv::{KvCaps, KvItem, KvPage, KvQuery, KvService, KvWrite};
pub use time::display_ms;
