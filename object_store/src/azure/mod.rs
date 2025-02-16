// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! An object store implementation for Azure blob storage
//!
//! ## Streaming uploads
//!
//! [ObjectStore::put_multipart] will upload data in blocks and write a blob from those
//! blocks. Data is buffered internally to make blocks of at least 5MB and blocks
//! are uploaded concurrently.
//!
//! [ObjectStore::abort_multipart] is a no-op, since Azure Blob Store doesn't provide
//! a way to drop old blocks. Instead unused blocks are automatically cleaned up
//! after 7 days.
use self::client::{BlockId, BlockList};
use crate::{
    multipart::{CloudMultiPartUpload, CloudMultiPartUploadImpl, UploadPart},
    path::Path,
    ClientOptions, GetResult, ListResult, MultipartId, ObjectMeta, ObjectStore, Result,
    RetryConfig,
};
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{TimeZone, Utc};
use futures::{stream::BoxStream, StreamExt, TryStreamExt};
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use snafu::{OptionExt, ResultExt, Snafu};
use std::fmt::{Debug, Formatter};
use std::io;
use std::ops::Range;
use std::sync::Arc;
use std::{collections::BTreeSet, str::FromStr};
use tokio::io::AsyncWrite;
use url::Url;

use crate::util::{str_is_truthy, RFC1123_FMT};
pub use credential::authority_hosts;

mod client;
mod credential;

/// The well-known account used by Azurite and the legacy Azure Storage Emulator.
/// <https://docs.microsoft.com/azure/storage/common/storage-use-azurite#well-known-storage-account-and-key>
const EMULATOR_ACCOUNT: &str = "devstoreaccount1";

/// The well-known account key used by Azurite and the legacy Azure Storage Emulator.
/// <https://docs.microsoft.com/azure/storage/common/storage-use-azurite#well-known-storage-account-and-key>
const EMULATOR_ACCOUNT_KEY: &str =
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";

/// A specialized `Error` for Azure object store-related errors
#[derive(Debug, Snafu)]
#[allow(missing_docs)]
enum Error {
    #[snafu(display("Last-Modified Header missing from response"))]
    MissingLastModified,

    #[snafu(display("Content-Length Header missing from response"))]
    MissingContentLength,

    #[snafu(display("Invalid last modified '{}': {}", last_modified, source))]
    InvalidLastModified {
        last_modified: String,
        source: chrono::ParseError,
    },

    #[snafu(display("Invalid content length '{}': {}", content_length, source))]
    InvalidContentLength {
        content_length: String,
        source: std::num::ParseIntError,
    },

    #[snafu(display("Received header containing non-ASCII data"))]
    BadHeader { source: reqwest::header::ToStrError },

    #[snafu(display("Unable parse source url. Url: {}, Error: {}", url, source))]
    UnableToParseUrl {
        source: url::ParseError,
        url: String,
    },

    #[snafu(display(
        "Unable parse emulator url {}={}, Error: {}",
        env_name,
        env_value,
        source
    ))]
    UnableToParseEmulatorUrl {
        env_name: String,
        env_value: String,
        source: url::ParseError,
    },

    #[snafu(display("Account must be specified"))]
    MissingAccount {},

    #[snafu(display("Container name must be specified"))]
    MissingContainerName {},

    #[snafu(display("At least one authorization option must be specified"))]
    MissingCredentials {},

    #[snafu(display("Azure credential error: {}", source), context(false))]
    Credential { source: credential::Error },

    #[snafu(display(
        "Unknown url scheme cannot be parsed into storage location: {}",
        scheme
    ))]
    UnknownUrlScheme { scheme: String },

    #[snafu(display("URL did not match any known pattern for scheme: {}", url))]
    UrlNotRecognised { url: String },

    #[snafu(display("Failed parsing an SAS key"))]
    DecodeSasKey { source: std::str::Utf8Error },

    #[snafu(display("Missing component in SAS query pair"))]
    MissingSasComponent {},

    #[snafu(display("Configuration key: '{}' is not known.", key))]
    UnknownConfigurationKey { key: String },
}

impl From<Error> for super::Error {
    fn from(source: Error) -> Self {
        match source {
            Error::UnknownConfigurationKey { key } => Self::UnknownConfigurationKey {
                store: "MicrosoftAzure",
                key,
            },
            _ => Self::Generic {
                store: "MicrosoftAzure",
                source: Box::new(source),
            },
        }
    }
}

/// Interface for [Microsoft Azure Blob Storage](https://azure.microsoft.com/en-us/services/storage/blobs/).
#[derive(Debug)]
pub struct MicrosoftAzure {
    client: Arc<client::AzureClient>,
}

impl std::fmt::Display for MicrosoftAzure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "MicrosoftAzure {{ account: {}, container: {} }}",
            self.client.config().account,
            self.client.config().container
        )
    }
}

#[async_trait]
impl ObjectStore for MicrosoftAzure {
    async fn put(&self, location: &Path, bytes: Bytes) -> Result<()> {
        self.client
            .put_request(location, Some(bytes), false, &())
            .await?;
        Ok(())
    }

    async fn put_multipart(
        &self,
        location: &Path,
    ) -> Result<(MultipartId, Box<dyn AsyncWrite + Unpin + Send>)> {
        let inner = AzureMultiPartUpload {
            client: Arc::clone(&self.client),
            location: location.to_owned(),
        };
        Ok((String::new(), Box::new(CloudMultiPartUpload::new(inner, 8))))
    }

    async fn abort_multipart(
        &self,
        _location: &Path,
        _multipart_id: &MultipartId,
    ) -> Result<()> {
        // There is no way to drop blocks that have been uploaded. Instead, they simply
        // expire in 7 days.
        Ok(())
    }

    async fn get(&self, location: &Path) -> Result<GetResult> {
        let response = self.client.get_request(location, None, false).await?;
        let stream = response
            .bytes_stream()
            .map_err(|source| crate::Error::Generic {
                store: "MicrosoftAzure",
                source: Box::new(source),
            })
            .boxed();

        Ok(GetResult::Stream(stream))
    }

    async fn get_range(&self, location: &Path, range: Range<usize>) -> Result<Bytes> {
        let bytes = self
            .client
            .get_request(location, Some(range), false)
            .await?
            .bytes()
            .await
            .map_err(|source| client::Error::GetResponseBody {
                source,
                path: location.to_string(),
            })?;
        Ok(bytes)
    }

    async fn head(&self, location: &Path) -> Result<ObjectMeta> {
        use reqwest::header::{CONTENT_LENGTH, LAST_MODIFIED};

        // Extract meta from headers
        // https://docs.microsoft.com/en-us/rest/api/storageservices/get-blob-properties
        let response = self.client.get_request(location, None, true).await?;
        let headers = response.headers();

        let last_modified = headers
            .get(LAST_MODIFIED)
            .ok_or(Error::MissingLastModified)?
            .to_str()
            .context(BadHeaderSnafu)?;
        let last_modified = Utc
            .datetime_from_str(last_modified, RFC1123_FMT)
            .context(InvalidLastModifiedSnafu { last_modified })?;

        let content_length = headers
            .get(CONTENT_LENGTH)
            .ok_or(Error::MissingContentLength)?
            .to_str()
            .context(BadHeaderSnafu)?;
        let content_length = content_length
            .parse()
            .context(InvalidContentLengthSnafu { content_length })?;

        Ok(ObjectMeta {
            location: location.clone(),
            last_modified,
            size: content_length,
        })
    }

    async fn delete(&self, location: &Path) -> Result<()> {
        self.client.delete_request(location, &()).await
    }

    async fn list(
        &self,
        prefix: Option<&Path>,
    ) -> Result<BoxStream<'_, Result<ObjectMeta>>> {
        let stream = self
            .client
            .list_paginated(prefix, false)
            .map_ok(|r| futures::stream::iter(r.objects.into_iter().map(Ok)))
            .try_flatten()
            .boxed();

        Ok(stream)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        let mut stream = self.client.list_paginated(prefix, true);

        let mut common_prefixes = BTreeSet::new();
        let mut objects = Vec::new();

        while let Some(result) = stream.next().await {
            let response = result?;
            common_prefixes.extend(response.common_prefixes.into_iter());
            objects.extend(response.objects.into_iter());
        }

        Ok(ListResult {
            common_prefixes: common_prefixes.into_iter().collect(),
            objects,
        })
    }

    async fn copy(&self, from: &Path, to: &Path) -> Result<()> {
        self.client.copy_request(from, to, true).await
    }

    async fn copy_if_not_exists(&self, from: &Path, to: &Path) -> Result<()> {
        self.client.copy_request(from, to, false).await
    }
}

/// Relevant docs: <https://azure.github.io/Storage/docs/application-and-user-data/basics/azure-blob-storage-upload-apis/>
/// In Azure Blob Store, parts are "blocks"
/// put_multipart_part -> PUT block
/// complete -> PUT block list
/// abort -> No equivalent; blocks are simply dropped after 7 days
#[derive(Debug, Clone)]
struct AzureMultiPartUpload {
    client: Arc<client::AzureClient>,
    location: Path,
}

#[async_trait]
impl CloudMultiPartUploadImpl for AzureMultiPartUpload {
    async fn put_multipart_part(
        &self,
        buf: Vec<u8>,
        part_idx: usize,
    ) -> Result<UploadPart, io::Error> {
        let content_id = format!("{:20}", part_idx);
        let block_id: BlockId = content_id.clone().into();

        self.client
            .put_request(
                &self.location,
                Some(buf.into()),
                true,
                &[("comp", "block"), ("blockid", &base64::encode(block_id))],
            )
            .await?;

        Ok(UploadPart { content_id })
    }

    async fn complete(&self, completed_parts: Vec<UploadPart>) -> Result<(), io::Error> {
        let blocks = completed_parts
            .into_iter()
            .map(|part| BlockId::from(part.content_id))
            .collect();

        let block_list = BlockList { blocks };
        let block_xml = block_list.to_xml();

        self.client
            .put_request(
                &self.location,
                Some(block_xml.into()),
                true,
                &[("comp", "blocklist")],
            )
            .await?;

        Ok(())
    }
}

/// Configure a connection to Microsoft Azure Blob Storage container using
/// the specified credentials.
///
/// # Example
/// ```
/// # let ACCOUNT = "foo";
/// # let BUCKET_NAME = "foo";
/// # let ACCESS_KEY = "foo";
/// # use object_store::azure::MicrosoftAzureBuilder;
/// let azure = MicrosoftAzureBuilder::new()
///  .with_account(ACCOUNT)
///  .with_access_key(ACCESS_KEY)
///  .with_container_name(BUCKET_NAME)
///  .build();
/// ```
#[derive(Default, Clone)]
pub struct MicrosoftAzureBuilder {
    account_name: Option<String>,
    access_key: Option<String>,
    container_name: Option<String>,
    bearer_token: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    tenant_id: Option<String>,
    sas_query_pairs: Option<Vec<(String, String)>>,
    sas_key: Option<String>,
    authority_host: Option<String>,
    url: Option<String>,
    use_emulator: bool,
    retry_config: RetryConfig,
    client_options: ClientOptions,
}

/// Configuration keys for [`MicrosoftAzureBuilder`]
///
/// Configuration via keys can be dome via the [`try_with_option`](MicrosoftAzureBuilder::try_with_option)
/// or [`with_options`](MicrosoftAzureBuilder::try_with_options) methods on the builder.
///
/// # Example
/// ```
/// use std::collections::HashMap;
/// use object_store::azure::{MicrosoftAzureBuilder, AzureConfigKey};
///
/// let options = HashMap::from([
///     ("azure_client_id", "my-client-id"),
///     ("azure_client_secret", "my-account-name"),
/// ]);
/// let typed_options = vec![
///     (AzureConfigKey::AccountName, "my-account-name"),
/// ];
/// let azure = MicrosoftAzureBuilder::new()
///     .try_with_options(options)
///     .unwrap()
///     .try_with_options(typed_options)
///     .unwrap()
///     .try_with_option(AzureConfigKey::AuthorityId, "my-tenant-id")
///     .unwrap();
/// ```
#[derive(PartialEq, Eq, Hash, Clone, Debug, Copy, Deserialize, Serialize)]
pub enum AzureConfigKey {
    /// The name of the azure storage account
    ///
    /// Supported keys:
    /// - `azure_storage_account_name`
    /// - `account_name`
    AccountName,

    /// Master key for accessing storage account
    ///
    /// Supported keys:
    /// - `azure_storage_account_key`
    /// - `azure_storage_access_key`
    /// - `azure_storage_master_key`
    /// - `access_key`
    /// - `account_key`
    /// - `master_key`
    AccessKey,

    /// Service principal client id for authorizing requests
    ///
    /// Supported keys:
    /// - `azure_storage_client_id`
    /// - `azure_client_id`
    /// - `client_id`
    ClientId,

    /// Service principal client secret for authorizing requests
    ///
    /// Supported keys:
    /// - `azure_storage_client_secret`
    /// - `azure_client_secret`
    /// - `client_secret`
    ClientSecret,

    /// Tenant id used in oauth flows
    ///
    /// Supported keys:
    /// - `azure_storage_tenant_id`
    /// - `azure_storage_authority_id`
    /// - `azure_tenant_id`
    /// - `azure_authority_id`
    /// - `tenant_id`
    /// - `authority_id`
    AuthorityId,

    /// Shared access signature.
    ///
    /// The signature is expected to be percent-encoded, much like they are provided
    /// in the azure storage explorer or azure portal.
    ///
    /// Supported keys:
    /// - `azure_storage_sas_key`
    /// - `azure_storage_sas_token`
    /// - `sas_key`
    /// - `sas_token`
    SasKey,

    /// Bearer token
    ///
    /// Supported keys:
    /// - `azure_storage_token`
    /// - `bearer_token`
    /// - `token`
    Token,

    /// Use object store with azurite storage emulator
    ///
    /// Supported keys:
    /// - `azure_storage_use_emulator`
    /// - `object_store_use_emulator`
    /// - `use_emulator`
    UseEmulator,
}

impl AsRef<str> for AzureConfigKey {
    fn as_ref(&self) -> &str {
        match self {
            Self::AccountName => "azure_storage_account_name",
            Self::AccessKey => "azure_storage_account_key",
            Self::ClientId => "azure_storage_client_id",
            Self::ClientSecret => "azure_storage_client_secret",
            Self::AuthorityId => "azure_storage_tenant_id",
            Self::SasKey => "azure_storage_sas_key",
            Self::Token => "azure_storage_token",
            Self::UseEmulator => "azure_storage_use_emulator",
        }
    }
}

impl FromStr for AzureConfigKey {
    type Err = super::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "azure_storage_account_key"
            | "azure_storage_access_key"
            | "azure_storage_master_key"
            | "master_key"
            | "account_key"
            | "access_key" => Ok(Self::AccessKey),
            "azure_storage_account_name" | "account_name" => Ok(Self::AccountName),
            "azure_storage_client_id" | "azure_client_id" | "client_id" => {
                Ok(Self::ClientId)
            }
            "azure_storage_client_secret" | "azure_client_secret" | "client_secret" => {
                Ok(Self::ClientSecret)
            }
            "azure_storage_tenant_id"
            | "azure_storage_authority_id"
            | "azure_tenant_id"
            | "azure_authority_id"
            | "tenant_id"
            | "authority_id" => Ok(Self::AuthorityId),
            "azure_storage_sas_key"
            | "azure_storage_sas_token"
            | "sas_key"
            | "sas_token" => Ok(Self::SasKey),
            "azure_storage_token" | "bearer_token" | "token" => Ok(Self::Token),
            "azure_storage_use_emulator" | "use_emulator" => Ok(Self::UseEmulator),
            _ => Err(Error::UnknownConfigurationKey { key: s.into() }.into()),
        }
    }
}

impl Debug for MicrosoftAzureBuilder {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "MicrosoftAzureBuilder {{ account: {:?}, container_name: {:?} }}",
            self.account_name, self.container_name
        )
    }
}

impl MicrosoftAzureBuilder {
    /// Create a new [`MicrosoftAzureBuilder`] with default values.
    pub fn new() -> Self {
        Default::default()
    }

    /// Create an instance of [`MicrosoftAzureBuilder`] with values pre-populated from environment variables.
    ///
    /// Variables extracted from environment:
    /// * AZURE_STORAGE_ACCOUNT_NAME: storage account name
    /// * AZURE_STORAGE_ACCOUNT_KEY: storage account master key
    /// * AZURE_STORAGE_ACCESS_KEY: alias for AZURE_STORAGE_ACCOUNT_KEY
    /// * AZURE_STORAGE_CLIENT_ID -> client id for service principal authorization
    /// * AZURE_STORAGE_CLIENT_SECRET -> client secret for service principal authorization
    /// * AZURE_STORAGE_TENANT_ID -> tenant id used in oauth flows
    /// # Example
    /// ```
    /// use object_store::azure::MicrosoftAzureBuilder;
    ///
    /// let azure = MicrosoftAzureBuilder::from_env()
    ///     .with_container_name("foo")
    ///     .build();
    /// ```
    pub fn from_env() -> Self {
        let mut builder = Self::default();
        for (os_key, os_value) in std::env::vars_os() {
            if let (Some(key), Some(value)) = (os_key.to_str(), os_value.to_str()) {
                if key.starts_with("AZURE_") {
                    if let Ok(config_key) =
                        AzureConfigKey::from_str(&key.to_ascii_lowercase())
                    {
                        builder = builder.try_with_option(config_key, value).unwrap();
                    }
                }
            }
        }

        if let Ok(text) = std::env::var("AZURE_ALLOW_HTTP") {
            builder.client_options =
                builder.client_options.with_allow_http(str_is_truthy(&text));
        }

        builder
    }

    /// Parse available connection info form a well-known storage URL.
    ///
    /// The supported url schemes are:
    ///
    /// - `abfs[s]://<container>/<path>` (according to [fsspec](https://github.com/fsspec/adlfs))
    /// - `abfs[s]://<file_system>@<account_name>.dfs.core.windows.net/<path>`
    /// - `az://<container>/<path>` (according to [fsspec](https://github.com/fsspec/adlfs))
    /// - `adl://<container>/<path>` (according to [fsspec](https://github.com/fsspec/adlfs))
    /// - `azure://<container>/<path>` (custom)
    /// - `https://<account>.dfs.core.windows.net`
    /// - `https://<account>.blob.core.windows.net`
    ///
    /// Note: Settings derived from the URL will override any others set on this builder
    ///
    /// # Example
    /// ```
    /// use object_store::azure::MicrosoftAzureBuilder;
    ///
    /// let azure = MicrosoftAzureBuilder::from_env()
    ///     .with_url("abfss://file_system@account.dfs.core.windows.net/")
    ///     .build();
    /// ```
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    /// Set an option on the builder via a key - value pair.
    pub fn try_with_option(
        mut self,
        key: impl AsRef<str>,
        value: impl Into<String>,
    ) -> Result<Self> {
        match AzureConfigKey::from_str(key.as_ref())? {
            AzureConfigKey::AccessKey => self.access_key = Some(value.into()),
            AzureConfigKey::AccountName => self.account_name = Some(value.into()),
            AzureConfigKey::ClientId => self.client_id = Some(value.into()),
            AzureConfigKey::ClientSecret => self.client_secret = Some(value.into()),
            AzureConfigKey::AuthorityId => self.tenant_id = Some(value.into()),
            AzureConfigKey::SasKey => self.sas_key = Some(value.into()),
            AzureConfigKey::Token => self.bearer_token = Some(value.into()),
            AzureConfigKey::UseEmulator => {
                self.use_emulator = str_is_truthy(&value.into())
            }
        };
        Ok(self)
    }

    /// Hydrate builder from key value pairs
    pub fn try_with_options<
        I: IntoIterator<Item = (impl AsRef<str>, impl Into<String>)>,
    >(
        mut self,
        options: I,
    ) -> Result<Self> {
        for (key, value) in options {
            self = self.try_with_option(key, value)?;
        }
        Ok(self)
    }

    /// Sets properties on this builder based on a URL
    ///
    /// This is a separate member function to allow fallible computation to
    /// be deferred until [`Self::build`] which in turn allows deriving [`Clone`]
    fn parse_url(&mut self, url: &str) -> Result<()> {
        let parsed = Url::parse(url).context(UnableToParseUrlSnafu { url })?;
        let host = parsed.host_str().context(UrlNotRecognisedSnafu { url })?;

        let validate = |s: &str| match s.contains('.') {
            true => Err(UrlNotRecognisedSnafu { url }.build()),
            false => Ok(s.to_string()),
        };

        match parsed.scheme() {
            "az" | "adl" | "azure" => self.container_name = Some(validate(host)?),
            "abfs" | "abfss" => {
                // abfs(s) might refer to the fsspec convention abfs://<container>/<path>
                // or the convention for the hadoop driver abfs[s]://<file_system>@<account_name>.dfs.core.windows.net/<path>
                if parsed.username().is_empty() {
                    self.container_name = Some(validate(host)?);
                } else if let Some(a) = host.strip_suffix(".dfs.core.windows.net") {
                    self.container_name = Some(validate(parsed.username())?);
                    self.account_name = Some(validate(a)?);
                } else {
                    return Err(UrlNotRecognisedSnafu { url }.build().into());
                }
            }
            "https" => match host.split_once('.') {
                Some((a, "dfs.core.windows.net"))
                | Some((a, "blob.core.windows.net")) => {
                    self.account_name = Some(validate(a)?);
                }
                _ => return Err(UrlNotRecognisedSnafu { url }.build().into()),
            },
            scheme => return Err(UnknownUrlSchemeSnafu { scheme }.build().into()),
        }
        Ok(())
    }

    /// Set the Azure Account (required)
    pub fn with_account(mut self, account: impl Into<String>) -> Self {
        self.account_name = Some(account.into());
        self
    }

    /// Set the Azure Container Name (required)
    pub fn with_container_name(mut self, container_name: impl Into<String>) -> Self {
        self.container_name = Some(container_name.into());
        self
    }

    /// Set the Azure Access Key (required - one of access key, bearer token, or client credentials)
    pub fn with_access_key(mut self, access_key: impl Into<String>) -> Self {
        self.access_key = Some(access_key.into());
        self
    }

    /// Set a static bearer token to be used for authorizing requests
    pub fn with_bearer_token_authorization(
        mut self,
        bearer_token: impl Into<String>,
    ) -> Self {
        self.bearer_token = Some(bearer_token.into());
        self
    }

    /// Set a client secret used for client secret authorization
    pub fn with_client_secret_authorization(
        mut self,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        tenant_id: impl Into<String>,
    ) -> Self {
        self.client_id = Some(client_id.into());
        self.client_secret = Some(client_secret.into());
        self.tenant_id = Some(tenant_id.into());
        self
    }

    /// Set query pairs appended to the url for shared access signature authorization
    pub fn with_sas_authorization(
        mut self,
        query_pairs: impl Into<Vec<(String, String)>>,
    ) -> Self {
        self.sas_query_pairs = Some(query_pairs.into());
        self
    }

    /// Set if the Azure emulator should be used (defaults to false)
    pub fn with_use_emulator(mut self, use_emulator: bool) -> Self {
        self.use_emulator = use_emulator;
        self
    }

    /// Sets what protocol is allowed. If `allow_http` is :
    /// * false (default):  Only HTTPS are allowed
    /// * true:  HTTP and HTTPS are allowed
    pub fn with_allow_http(mut self, allow_http: bool) -> Self {
        self.client_options = self.client_options.with_allow_http(allow_http);
        self
    }

    /// Sets an alternative authority host for OAuth based authorization
    /// common hosts for azure clouds are defined in [authority_hosts].
    /// Defaults to <https://login.microsoftonline.com>
    pub fn with_authority_host(mut self, authority_host: String) -> Self {
        self.authority_host = Some(authority_host);
        self
    }

    /// Set the retry configuration
    pub fn with_retry(mut self, retry_config: RetryConfig) -> Self {
        self.retry_config = retry_config;
        self
    }

    /// Set the proxy_url to be used by the underlying client
    pub fn with_proxy_url(mut self, proxy_url: impl Into<String>) -> Self {
        self.client_options = self.client_options.with_proxy_url(proxy_url);
        self
    }

    /// Sets the client options, overriding any already set
    pub fn with_client_options(mut self, options: ClientOptions) -> Self {
        self.client_options = options;
        self
    }

    /// Configure a connection to container with given name on Microsoft Azure
    /// Blob store.
    pub fn build(mut self) -> Result<MicrosoftAzure> {
        if let Some(url) = self.url.take() {
            self.parse_url(&url)?;
        }

        let container = self.container_name.ok_or(Error::MissingContainerName {})?;

        let (is_emulator, storage_url, auth, account) = if self.use_emulator {
            let account_name = self
                .account_name
                .unwrap_or_else(|| EMULATOR_ACCOUNT.to_string());
            // Allow overriding defaults. Values taken from
            // from https://docs.rs/azure_storage/0.2.0/src/azure_storage/core/clients/storage_account_client.rs.html#129-141
            let url = url_from_env("AZURITE_BLOB_STORAGE_URL", "http://127.0.0.1:10000")?;
            let account_key = self
                .access_key
                .unwrap_or_else(|| EMULATOR_ACCOUNT_KEY.to_string());
            let credential = credential::CredentialProvider::AccessKey(account_key);

            self.client_options = self.client_options.with_allow_http(true);
            (true, url, credential, account_name)
        } else {
            let account_name = self.account_name.ok_or(Error::MissingAccount {})?;
            let account_url = format!("https://{}.blob.core.windows.net", &account_name);
            let url = Url::parse(&account_url)
                .context(UnableToParseUrlSnafu { url: account_url })?;
            let credential = if let Some(bearer_token) = self.bearer_token {
                Ok(credential::CredentialProvider::AccessKey(bearer_token))
            } else if let Some(access_key) = self.access_key {
                Ok(credential::CredentialProvider::AccessKey(access_key))
            } else if let (Some(client_id), Some(client_secret), Some(tenant_id)) =
                (self.client_id, self.client_secret, self.tenant_id)
            {
                let client_credential = credential::ClientSecretOAuthProvider::new(
                    client_id,
                    client_secret,
                    tenant_id,
                    self.authority_host,
                );
                Ok(credential::CredentialProvider::ClientSecret(
                    client_credential,
                ))
            } else if let Some(query_pairs) = self.sas_query_pairs {
                Ok(credential::CredentialProvider::SASToken(query_pairs))
            } else if let Some(sas) = self.sas_key {
                Ok(credential::CredentialProvider::SASToken(split_sas(&sas)?))
            } else {
                Err(Error::MissingCredentials {})
            }?;
            (false, url, credential, account_name)
        };

        let config = client::AzureConfig {
            account,
            is_emulator,
            container,
            retry_config: self.retry_config,
            client_options: self.client_options,
            service: storage_url,
            credentials: auth,
        };

        let client = Arc::new(client::AzureClient::new(config)?);

        Ok(MicrosoftAzure { client })
    }
}

/// Parses the contents of the environment variable `env_name` as a URL
/// if present, otherwise falls back to default_url
fn url_from_env(env_name: &str, default_url: &str) -> Result<Url> {
    let url = match std::env::var(env_name) {
        Ok(env_value) => {
            Url::parse(&env_value).context(UnableToParseEmulatorUrlSnafu {
                env_name,
                env_value,
            })?
        }
        Err(_) => Url::parse(default_url).expect("Failed to parse default URL"),
    };
    Ok(url)
}

fn split_sas(sas: &str) -> Result<Vec<(String, String)>, Error> {
    let sas = percent_decode_str(sas)
        .decode_utf8()
        .context(DecodeSasKeySnafu {})?;
    let kv_str_pairs = sas
        .trim_start_matches('?')
        .split('&')
        .filter(|s| !s.chars().all(char::is_whitespace));
    let mut pairs = Vec::new();
    for kv_pair_str in kv_str_pairs {
        let (k, v) = kv_pair_str
            .trim()
            .split_once('=')
            .ok_or(Error::MissingSasComponent {})?;
        pairs.push((k.into(), v.into()))
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{
        copy_if_not_exists, list_uses_directories_correctly, list_with_delimiter,
        put_get_delete_list, put_get_delete_list_opts, rename_and_copy, stream_get,
    };
    use std::collections::HashMap;
    use std::env;

    // Helper macro to skip tests if TEST_INTEGRATION and the Azure environment
    // variables are not set.
    macro_rules! maybe_skip_integration {
        () => {{
            dotenv::dotenv().ok();

            let use_emulator = std::env::var("AZURE_USE_EMULATOR").is_ok();

            let mut required_vars = vec!["OBJECT_STORE_BUCKET"];
            if !use_emulator {
                required_vars.push("AZURE_STORAGE_ACCOUNT");
                required_vars.push("AZURE_STORAGE_ACCESS_KEY");
            }
            let unset_vars: Vec<_> = required_vars
                .iter()
                .filter_map(|&name| match env::var(name) {
                    Ok(_) => None,
                    Err(_) => Some(name),
                })
                .collect();
            let unset_var_names = unset_vars.join(", ");

            let force = std::env::var("TEST_INTEGRATION");

            if force.is_ok() && !unset_var_names.is_empty() {
                panic!(
                    "TEST_INTEGRATION is set, \
                        but variable(s) {} need to be set",
                    unset_var_names
                )
            } else if force.is_err() {
                eprintln!(
                    "skipping Azure integration test - set {}TEST_INTEGRATION to run",
                    if unset_var_names.is_empty() {
                        String::new()
                    } else {
                        format!("{} and ", unset_var_names)
                    }
                );
                return;
            } else {
                let builder = MicrosoftAzureBuilder::new()
                    .with_container_name(
                        env::var("OBJECT_STORE_BUCKET")
                            .expect("already checked OBJECT_STORE_BUCKET"),
                    )
                    .with_use_emulator(use_emulator);
                if !use_emulator {
                    builder
                        .with_account(
                            env::var("AZURE_STORAGE_ACCOUNT").unwrap_or_default(),
                        )
                        .with_access_key(
                            env::var("AZURE_STORAGE_ACCESS_KEY").unwrap_or_default(),
                        )
                } else {
                    builder
                }
            }
        }};
    }

    #[tokio::test]
    async fn azure_blob_test() {
        let use_emulator = env::var("AZURE_USE_EMULATOR").is_ok();
        let integration = maybe_skip_integration!().build().unwrap();
        // Azurite doesn't support listing with spaces - https://github.com/localstack/localstack/issues/6328
        put_get_delete_list_opts(&integration, use_emulator).await;
        list_uses_directories_correctly(&integration).await;
        list_with_delimiter(&integration).await;
        rename_and_copy(&integration).await;
        copy_if_not_exists(&integration).await;
        stream_get(&integration).await;
    }

    // test for running integration test against actual blob service with service principal
    // credentials. To run make sure all environment variables are set and remove the ignore
    #[tokio::test]
    #[ignore]
    async fn azure_blob_test_sp() {
        dotenv::dotenv().ok();
        let builder = MicrosoftAzureBuilder::new()
            .with_account(
                env::var("AZURE_STORAGE_ACCOUNT")
                    .expect("must be set AZURE_STORAGE_ACCOUNT"),
            )
            .with_container_name(
                env::var("OBJECT_STORE_BUCKET").expect("must be set OBJECT_STORE_BUCKET"),
            )
            .with_access_key(
                env::var("AZURE_STORAGE_ACCESS_KEY")
                    .expect("must be set AZURE_STORAGE_CLIENT_ID"),
            );
        let integration = builder.build().unwrap();

        put_get_delete_list(&integration).await;
        list_uses_directories_correctly(&integration).await;
        list_with_delimiter(&integration).await;
        rename_and_copy(&integration).await;
        copy_if_not_exists(&integration).await;
        stream_get(&integration).await;
    }

    #[test]
    fn azure_blob_test_urls() {
        let mut builder = MicrosoftAzureBuilder::new();
        builder
            .parse_url("abfss://file_system@account.dfs.core.windows.net/")
            .unwrap();
        assert_eq!(builder.account_name, Some("account".to_string()));
        assert_eq!(builder.container_name, Some("file_system".to_string()));

        let mut builder = MicrosoftAzureBuilder::new();
        builder.parse_url("abfs://container/path").unwrap();
        assert_eq!(builder.container_name, Some("container".to_string()));

        let mut builder = MicrosoftAzureBuilder::new();
        builder.parse_url("az://container").unwrap();
        assert_eq!(builder.container_name, Some("container".to_string()));

        let mut builder = MicrosoftAzureBuilder::new();
        builder.parse_url("az://container/path").unwrap();
        assert_eq!(builder.container_name, Some("container".to_string()));

        let mut builder = MicrosoftAzureBuilder::new();
        builder
            .parse_url("https://account.dfs.core.windows.net/")
            .unwrap();
        assert_eq!(builder.account_name, Some("account".to_string()));

        let mut builder = MicrosoftAzureBuilder::new();
        builder
            .parse_url("https://account.blob.core.windows.net/")
            .unwrap();
        assert_eq!(builder.account_name, Some("account".to_string()));

        let err_cases = [
            "mailto://account.blob.core.windows.net/",
            "az://blob.mydomain/",
            "abfs://container.foo/path",
            "abfss://file_system@account.foo.dfs.core.windows.net/",
            "abfss://file_system.bar@account.dfs.core.windows.net/",
            "https://blob.mydomain/",
            "https://blob.foo.dfs.core.windows.net/",
        ];
        let mut builder = MicrosoftAzureBuilder::new();
        for case in err_cases {
            builder.parse_url(case).unwrap_err();
        }
    }

    #[test]
    fn azure_test_config_from_map() {
        let azure_client_id = "object_store:fake_access_key_id";
        let azure_storage_account_name = "object_store:fake_secret_key";
        let azure_storage_token = "object_store:fake_default_region";
        let options = HashMap::from([
            ("azure_client_id", azure_client_id),
            ("azure_storage_account_name", azure_storage_account_name),
            ("azure_storage_token", azure_storage_token),
        ]);

        let builder = MicrosoftAzureBuilder::new()
            .try_with_options(options)
            .unwrap();
        assert_eq!(builder.client_id.unwrap(), azure_client_id);
        assert_eq!(builder.account_name.unwrap(), azure_storage_account_name);
        assert_eq!(builder.bearer_token.unwrap(), azure_storage_token);
    }

    #[test]
    fn azure_test_config_from_typed_map() {
        let azure_client_id = "object_store:fake_access_key_id".to_string();
        let azure_storage_account_name = "object_store:fake_secret_key".to_string();
        let azure_storage_token = "object_store:fake_default_region".to_string();
        let options = HashMap::from([
            (AzureConfigKey::ClientId, azure_client_id.clone()),
            (
                AzureConfigKey::AccountName,
                azure_storage_account_name.clone(),
            ),
            (AzureConfigKey::Token, azure_storage_token.clone()),
        ]);

        let builder = MicrosoftAzureBuilder::new()
            .try_with_options(&options)
            .unwrap();
        assert_eq!(builder.client_id.unwrap(), azure_client_id);
        assert_eq!(builder.account_name.unwrap(), azure_storage_account_name);
        assert_eq!(builder.bearer_token.unwrap(), azure_storage_token);
    }

    #[test]
    fn azure_test_config_fallible_options() {
        let azure_client_id = "object_store:fake_access_key_id".to_string();
        let azure_storage_token = "object_store:fake_default_region".to_string();
        let options = HashMap::from([
            ("azure_client_id", azure_client_id),
            ("invalid-key", azure_storage_token),
        ]);

        let builder = MicrosoftAzureBuilder::new().try_with_options(&options);
        assert!(builder.is_err());
    }

    #[test]
    fn azure_test_split_sas() {
        let raw_sas = "?sv=2021-10-04&st=2023-01-04T17%3A48%3A57Z&se=2023-01-04T18%3A15%3A00Z&sr=c&sp=rcwl&sig=C7%2BZeEOWbrxPA3R0Cw%2Fw1EZz0%2B4KBvQexeKZKe%2BB6h0%3D";
        let expected = vec![
            ("sv".to_string(), "2021-10-04".to_string()),
            ("st".to_string(), "2023-01-04T17:48:57Z".to_string()),
            ("se".to_string(), "2023-01-04T18:15:00Z".to_string()),
            ("sr".to_string(), "c".to_string()),
            ("sp".to_string(), "rcwl".to_string()),
            (
                "sig".to_string(),
                "C7+ZeEOWbrxPA3R0Cw/w1EZz0+4KBvQexeKZKe+B6h0=".to_string(),
            ),
        ];
        let pairs = split_sas(raw_sas).unwrap();
        assert_eq!(expected, pairs);
    }
}
