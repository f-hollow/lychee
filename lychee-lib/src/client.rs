//! Handler of link checking operations.
//!
//! This module defines two structs, [`Client`] and [`ClientBuilder`].
//! `Client` handles incoming requests and returns responses.
//! `ClientBuilder` exposes a finer level of granularity for building
//! a `Client`.
//!
//! For convenience, a free function [`check`] is provided for ad-hoc
//! link checks.
#![allow(
    clippy::module_name_repetitions,
    clippy::struct_excessive_bools,
    clippy::default_trait_access,
    clippy::used_underscore_binding
)]
use std::{collections::HashSet, time::Duration};

use check_if_email_exists::{check_email, CheckEmailInput, Reachable};
use http::{
    header::{HeaderMap, HeaderValue},
    StatusCode,
};
use octocrab::Octocrab;
use regex::RegexSet;
use reqwest::{header, Url};
use secrecy::{ExposeSecret, SecretString};
use tokio::time::sleep;
use typed_builder::TypedBuilder;

use crate::{
    filter::{Excludes, Filter, Includes},
    quirks::Quirks,
    remap::Remaps,
    types::{mail, uri::github::GithubUri},
    ErrorKind, Request, Response, Result, Status, Uri,
};

/// Default number of redirects before a request is deemed as failed, 5.
pub const DEFAULT_MAX_REDIRECTS: usize = 5;
/// Default number of retries before a request is deemed as failed, 3.
pub const DEFAULT_MAX_RETRIES: u64 = 3;
/// Default wait time in seconds between retries, 1.
pub const DEFAULT_RETRY_WAIT_TIME_SECS: usize = 1;
/// Default timeout in seconds before a request is deemed as failed, 20.
pub const DEFAULT_TIMEOUT_SECS: usize = 20;
/// Default user agent, `lychee-<PKG_VERSION>`.
pub const DEFAULT_USER_AGENT: &str = concat!("lychee/", env!("CARGO_PKG_VERSION"));

// Constants currently not configurable by the user.
/// A timeout for only the connect phase of a [`Client`].
const CONNECT_TIMEOUT: u64 = 10;
/// TCP keepalive.
///
/// See <https://tldp.org/HOWTO/TCP-Keepalive-HOWTO/overview.html> for more
/// infomation.
const TCP_KEEPALIVE: u64 = 60;

/// Builder for [`Client`].
///
/// See crate-level documentation for usage example.
#[derive(TypedBuilder, Debug, Clone)]
#[builder(field_defaults(default, setter(into)))]
pub struct ClientBuilder {
    /// Optional GitHub token used for GitHub links.
    ///
    /// This allows much more request before getting rate-limited.
    ///
    /// # Rate-limiting Defaults
    ///
    /// As of Feb 2022, it's 60 per hour without GitHub token v.s.
    /// 5000 per hour with token.
    github_token: Option<SecretString>,

    /// Remap URIs matching a pattern to a different URI.
    ///
    /// This makes it possible to remap any HTTP/HTTPS endpoint to a different
    /// HTTP/HTTPS one. This feature could also be used to proxy
    /// certain requests.
    ///
    /// # Usage Notes
    ///
    /// Use with caution because a large set of remapping rules may cause
    /// performance issues.
    ///
    /// Furthermore rules are executed sequentially and multiple mappings for
    /// the same URI are allowed, so it is up to the library user's discretion to
    /// make sure rules don't conflict with each other.
    remaps: Option<Remaps>,

    /// Links matching this set of regular expressions are **always** checked.
    ///
    /// This has higher precedence over [`ClientBuilder::excludes`], **but**
    /// has lower precedence compared to any other `exclude_` fields or
    /// [`ClientBuilder::schemes`] below.
    includes: Option<RegexSet>,

    /// Links matching this set of regular expressions are ignored, **except**
    /// when a link also matches against [`ClientBuilder::includes`].
    excludes: Option<RegexSet>,

    /// When `true`, exclude all private network addresses.
    ///
    /// This effectively turns on the following fields:
    /// - [`ClientBuilder::exclude_private_ips`]
    /// - [`ClientBuilder::exclude_link_local_ips`]
    /// - [`ClientBuilder::exclude_loopback_ips`]
    exclude_all_private: bool,

    /// When `true`, exclude private IP addresses.
    ///
    /// # IPv4
    ///
    /// The private address ranges are defined in [IETF RFC 1918] and include:
    ///
    ///  - `10.0.0.0/8`
    ///  - `172.16.0.0/12`
    ///  - `192.168.0.0/16`
    ///
    /// # IPv6
    ///
    /// The address is a unique local address (`fc00::/7`).
    ///
    /// This property is defined in [IETF RFC 4193].
    ///
    /// # Note
    ///
    /// Unicast site-local network was defined in [IETF RFC 4291], but was fully
    /// deprecated in [IETF RFC 3879]. So it is **NOT** considered as private on
    /// this purpose.
    ///
    /// [IETF RFC 1918]: https://tools.ietf.org/html/rfc1918
    /// [IETF RFC 4193]: https://tools.ietf.org/html/rfc4193
    /// [IETF RFC 4291]: https://tools.ietf.org/html/rfc4291
    /// [IETF RFC 3879]: https://tools.ietf.org/html/rfc3879
    exclude_private_ips: bool,

    /// When `true`, exclude link-local IPs.
    ///
    /// # IPv4
    ///
    /// The address is `169.254.0.0/16`.
    ///
    /// This property is defined by [IETF RFC 3927].
    ///
    /// # IPv6
    ///
    /// The address is a unicast address with link-local scope,  as defined in
    /// [RFC 4291].
    ///
    /// A unicast address has link-local scope if it has the prefix `fe80::/10`,
    /// as per [RFC 4291 section 2.4].
    ///
    /// [IETF RFC 3927]: https://tools.ietf.org/html/rfc3927
    /// [RFC 4291]: https://tools.ietf.org/html/rfc4291
    /// [RFC 4291 section 2.4]: https://tools.ietf.org/html/rfc4291#section-2.4
    exclude_link_local_ips: bool,

    /// When `true`, exclude loopback IP addresses.
    ///
    /// # IPv4
    ///
    /// This is a loopback address (`127.0.0.0/8`).
    ///
    /// This property is defined by [IETF RFC 1122].
    ///
    /// # IPv6
    ///
    /// This is the loopback address (`::1`), as defined in
    /// [IETF RFC 4291 section 2.5.3].
    ///
    /// [IETF RFC 1122]: https://tools.ietf.org/html/rfc1122
    /// [IETF RFC 4291 section 2.5.3]: https://tools.ietf.org/html/rfc4291#section-2.5.3
    exclude_loopback_ips: bool,

    /// When `true`, don't check mail addresses.
    exclude_mail: bool,

    /// Maximum number of redirects per request before returning an error.
    ///
    /// Defaults to [`DEFAULT_MAX_REDIRECTS`].
    #[builder(default = DEFAULT_MAX_REDIRECTS)]
    max_redirects: usize,

    /// Maximum number of retries per request before returning an error.
    ///
    /// Defaults to [`DEFAULT_MAX_RETRIES`].
    #[builder(default = DEFAULT_MAX_RETRIES)]
    max_retries: u64,

    /// User-agent used for checking links.
    ///
    /// Defaults to [`DEFAULT_USER_AGENT`].
    ///
    /// # Notes
    ///
    /// This may be helpful for bypassing certain firewalls.
    // Faking the user agent is necessary for some websites, unfortunately.
    // Otherwise we get a 403 from the firewall (e.g. Sucuri/Cloudproxy on ldra.com).
    #[builder(default_code = "String::from(DEFAULT_USER_AGENT)")]
    user_agent: String,

    /// When `true`, accept invalid SSL certificates.
    ///
    /// # Warning
    ///
    /// You should think very carefully before allowing invalid SSL
    /// certificates. It will accept any certificate for any site to be trusted
    /// including expired certificates. This introduces significant
    /// vulnerabilities, and should only be used as a last resort.
    // TODO: We should add a warning message in CLI. (Lucius, Jan 2023)
    allow_insecure: bool,

    /// Set of accepted URL schemes.
    ///
    /// Only links with matched URI schemes are checked. This has no effect when
    /// it's empty.
    schemes: HashSet<String>,

    /// Default [headers] for every request.
    ///
    /// This allows working around validation issues on some websites. See also
    /// [here] for usage examples.
    ///
    /// [headers]: https://docs.rs/http/latest/http/header/struct.HeaderName.html
    /// [here]: https://docs.rs/reqwest/latest/reqwest/struct.ClientBuilder.html#method.default_headers
    custom_headers: HeaderMap,

    /// HTTP method used for requests, e.g. `GET` or `HEAD`.
    #[builder(default = reqwest::Method::GET)]
    method: reqwest::Method,

    /// Set of accepted return codes / status codes.
    ///
    /// Unmatched return codes/ status codes are deemed as errors.
    accepted: Option<HashSet<StatusCode>>,

    /// Response timeout per request in seconds.
    timeout: Option<Duration>,

    /// Initial time between retries of failed requests.
    ///
    /// Defaults to [`DEFAULT_RETRY_WAIT_TIME_SECS`].
    ///
    /// # Notes
    ///
    /// For each request, the wait time increases using an exponential backoff
    /// mechanism. For example, if the value is 1 second, then it waits for
    /// 2 ^ (N-1) seconds before the N-th retry.
    ///
    /// This prevents spending too much system resources on slow responders and
    /// prioritizes other requests.
    #[builder(default_code = "Duration::from_secs(DEFAULT_RETRY_WAIT_TIME_SECS as u64)")]
    retry_wait_time: Duration,

    /// When `true`, requires using HTTPS when it's available.
    ///
    /// This would treat unencrypted links as errors when HTTPS is avaliable.
    /// It has no effect on non-HTTP schemes or if the URL doesn't support
    /// HTTPS.
    require_https: bool,
}

impl Default for ClientBuilder {
    #[must_use]
    #[inline]
    fn default() -> Self {
        Self::builder().build()
    }
}

impl ClientBuilder {
    /// Instantiates a [`Client`].
    ///
    /// # Errors
    ///
    /// Returns an `Err` if:
    /// - The user-agent contains characters other than ASCII 32-127.
    /// - The reqwest client cannot be instantiated. This occurs if a TLS
    ///   backend cannot be initialized or the resolver fails to load the system
    ///   configuration. See [here].
    /// - The Github client cannot be created. Since the implementation also
    ///   uses reqwest under the hood, this errors in the same circumstances as
    ///   the last one.
    ///
    /// [here]: https://docs.rs/reqwest/latest/reqwest/struct.ClientBuilder.html#errors
    pub fn client(self) -> Result<Client> {
        let Self {
            user_agent,
            custom_headers: mut headers,
            ..
        } = self;

        if let Some(prev_user_agent) =
            headers.insert(header::USER_AGENT, HeaderValue::try_from(&user_agent)?)
        {
            // TODO: make this configurable according to verbosity (Lucius, Jan 2023)
            println!(
                "Found user-agent in headers: {}. Overriding it with {user_agent}.",
                prev_user_agent.to_str().unwrap_or("�"),
            );
        };

        headers.insert(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );

        let builder = reqwest::ClientBuilder::new()
            .gzip(true)
            .default_headers(headers)
            .danger_accept_invalid_certs(self.allow_insecure)
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT))
            .tcp_keepalive(Duration::from_secs(TCP_KEEPALIVE))
            .redirect(reqwest::redirect::Policy::limited(self.max_redirects));

        let reqwest_client = (match self.timeout {
            Some(t) => builder.timeout(t),
            None => builder,
        })
        .build()
        .map_err(ErrorKind::NetworkRequest)?;

        let github_client = match self.github_token.as_ref().map(ExposeSecret::expose_secret) {
            Some(token) if !token.is_empty() => Some(
                Octocrab::builder()
                    .personal_token(token.clone())
                    .build()
                    // this is essentially the same reqwest::ClientBuilder::build error
                    // see https://docs.rs/octocrab/0.18.1/src/octocrab/lib.rs.html#360-364
                    .map_err(ErrorKind::BuildGithubClient)?,
            ),
            _ => None,
        };

        let filter = Filter {
            includes: self.includes.map(|regex| Includes { regex }),
            excludes: self.excludes.map(|regex| Excludes { regex }),
            schemes: self.schemes,
            // exclude_all_private option turns on all "private" excludes,
            // including private IPs, link-local IPs and loopback IPs
            exclude_private_ips: self.exclude_all_private || self.exclude_private_ips,
            exclude_link_local_ips: self.exclude_all_private || self.exclude_link_local_ips,
            exclude_loopback_ips: self.exclude_all_private || self.exclude_loopback_ips,
            exclude_mail: self.exclude_mail,
        };

        let quirks = Quirks::default();

        Ok(Client {
            reqwest_client,
            github_client,
            remaps: self.remaps,
            filter,
            max_retries: self.max_retries,
            retry_wait_time: self.retry_wait_time,
            method: self.method,
            accepted: self.accepted,
            require_https: self.require_https,
            quirks,
        })
    }
}

/// Handles incoming requests and returns responses.
///
/// See [`ClientBuilder`] which contains sane defaults for all configuration
/// options.
#[derive(Debug, Clone)]
pub struct Client {
    /// Underlying `reqwest` client instance that handles the HTTP requests.
    reqwest_client: reqwest::Client,

    /// Optional GitHub client that handles communications with GitHub.
    github_client: Option<Octocrab>,

    /// Optional remapping rules for URIs matching pattern.
    remaps: Option<Remaps>,

    /// Rules to decided whether each link would be checked or ignored.
    filter: Filter,

    /// Maximum number of retries per request before returning an error.
    max_retries: u64,

    /// Initial wait time between retries of failed requests. This doubles after
    /// each failure.
    retry_wait_time: Duration,

    /// HTTP method used for requests, e.g. `GET` or `HEAD`.
    ///
    /// The same method will be used for all links.
    method: reqwest::Method,

    /// Set of accepted return codes / status codes.
    ///
    /// Unmatched return codes/ status codes are deemed as errors.
    accepted: Option<HashSet<StatusCode>>,

    /// Requires using HTTPS when it's available.
    ///
    /// This would treat unencrypted links as errors when HTTPS is avaliable.
    require_https: bool,

    /// Override behaviors for certain known issues with special URIs.
    quirks: Quirks,
}

impl Client {
    /// Check a single request.
    ///
    /// `request` can be either a [`Request`] or a type that can be converted
    /// into it. In any case, it must represent a valid URI.
    ///
    /// # Errors
    ///
    /// Returns an `Err` if:
    /// - `request` does not represent a valid URI.
    /// - Encrypted connection for a HTTP URL is available but unused.  (Only
    ///   checked when `Client::require_https` is `true`.)
    #[allow(clippy::missing_panics_doc)]
    pub async fn check<T, E>(&self, request: T) -> Result<Response>
    where
        Request: TryFrom<T, Error = E>,
        ErrorKind: From<E>,
    {
        let Request {
            ref mut uri,
            source,
            ..
        } = request.try_into()?;

        self.remap(uri);

        // TODO: Allow filtering based on element and attribute
        let status = if self.is_excluded(uri) {
            Status::Excluded
        } else if uri.is_file() {
            self.check_file(uri)
        } else if uri.is_mail() {
            self.check_mail(uri).await
        } else {
            match self.check_website(uri).await {
                Status::Ok(code) if self.require_https && uri.scheme() == "http" => {
                    let mut https_uri = uri.clone();
                    {
                        // here `uri` must be valid, otherwise `check_website` won't
                        // return `Ok`, thus `set_scheme` won't fail
                        debug_assert!(!https_uri.url.cannot_be_a_base());
                        https_uri.set_scheme("https").unwrap();
                    }
                    if self.check_website(&https_uri).await.is_success() {
                        Status::Error(ErrorKind::InsecureURL(https_uri))
                    } else {
                        Status::Ok(code)
                    }
                }
                s => s,
            }
        };

        Ok(Response::new(uri.clone(), status, source))
    }

    /// Remap `uri` using the client-defined remapping rules.
    pub fn remap(&self, uri: &mut Uri) {
        // TODO: this should be logged (Lucius, Jan 2023)
        if let Some(ref remaps) = self.remaps {
            remaps.remap(&mut uri.url);
        }
    }

    /// Returns whether the given `uri` should be ignored from checking.
    #[must_use]
    pub fn is_excluded(&self, uri: &Uri) -> bool {
        self.filter.is_excluded(uri)
    }

    /// Checks the given URI of a website.
    ///
    /// Unsupported schemes will be ignored
    pub async fn check_website(&self, uri: &Uri) -> Status {
        // Workaround for upstream reqwest panic
        if validate_url(&uri.url) {
            if matches!(uri.scheme(), "http" | "https") {
                // This is a truly invalid URI with a known scheme.
                // If we pass that to reqwest it would panic.
                return Status::Error(ErrorKind::InvalidURI(uri.clone()));
            }
            // This is merely a URI with a scheme that is not supported by
            // reqwest yet. It would be safe to pass that to reqwest and it
            // wouldn't panic, but it's also unnecessary, because it would
            // simply return an error.
            return Status::Unsupported(ErrorKind::InvalidURI(uri.clone()));
        }

        let mut retries: u64 = 0;
        let mut wait_time = self.retry_wait_time;

        let mut status = self.check_default(uri).await;
        while retries < self.max_retries {
            if status.is_success() {
                return status;
            }
            sleep(wait_time).await;
            retries += 1;
            wait_time = wait_time.saturating_mul(2);
            status = self.check_default(uri).await;
        }

        // Pull out the heavy machinery in case of a failed normal request.
        // This could be a GitHub URL and we ran into the rate limiter.
        // TODO: We should first try to parse the URI as GitHub URI first (Lucius, Jan 2023)
        if let Ok(github_uri) = GithubUri::try_from(uri) {
            let status = self.check_github(github_uri).await;
            // Only return Github status in case of success
            // Otherwise return the original error, which has more information
            if status.is_success() {
                return status;
            }
        }

        status
    }

    /// Check a `uri` hosted on `GitHub` via the GitHub API.
    ///
    /// # Caveats
    ///
    /// Files inside private repositories won't get checked and instead would
    /// be reported as valid if the repository itself is reachable through the
    /// API.
    ///
    /// A better approach would be to download the file through the API or
    /// clone the repo, but we chose the pragmatic approach.
    async fn check_github(&self, uri: GithubUri) -> Status {
        let Some(client) = &self.github_client else { return ErrorKind::MissingGitHubToken.into() };
        let repo = match client.repos(&uri.owner, &uri.repo).get().await {
            Ok(repo) => repo,
            Err(e) => return ErrorKind::GithubRequest(e).into(),
        };
        if let Some(true) = repo.private {
            // The private repo exists. Assume a given endpoint exists as well
            // (e.g. `issues` in `github.com/org/private/issues`). This is not
            // always the case but simplifies the check.
            return Status::Ok(StatusCode::OK);
        } else if let Some(endpoint) = uri.endpoint {
            // The URI returned a non-200 status code from a normal request and
            // now we find that this public repo is reachable through the API,
            // so that must mean the full URI (which includes the additional
            // endpoint) must be invalid.
            return ErrorKind::InvalidGithubUrl(format!("{}/{}/{endpoint}", uri.owner, uri.repo))
                .into();
        }
        // Found public repo without endpoint
        Status::Ok(StatusCode::OK)
    }

    /// Check a URI using [reqwest](https://github.com/seanmonstar/reqwest).
    async fn check_default(&self, uri: &Uri) -> Status {
        let request = match self
            .reqwest_client
            .request(self.method.clone(), uri.as_str())
            .build()
        {
            Ok(r) => r,
            Err(e) => return e.into(),
        };

        let request = self.quirks.apply(request);

        match self.reqwest_client.execute(request).await {
            Ok(ref response) => Status::new(response, self.accepted.clone()),
            Err(e) => e.into(),
        }
    }

    /// Check a `file` URI.
    pub fn check_file(&self, uri: &Uri) -> Status {
        if let Ok(path) = uri.url.to_file_path() {
            if path.exists() {
                return Status::Ok(StatusCode::OK);
            }
        }
        ErrorKind::InvalidFilePath(uri.clone()).into()
    }

    /// Check a mail address, or equivalently a `mailto` URI.
    ///
    /// URIs may contain query parameters (e.g. `contact@example.com?subject="Hello"`),
    /// which are ignored by this check. The are not part of the mail address
    /// and instead passed to a mail client.
    pub async fn check_mail(&self, uri: &Uri) -> Status {
        let address = uri.url.path().to_string();
        let input = CheckEmailInput::new(address);
        let result = &(check_email(&input).await);

        if let Reachable::Invalid = result.is_reachable {
            ErrorKind::UnreachableEmailAddress(uri.clone(), mail::error_from_output(result)).into()
        } else {
            Status::Ok(StatusCode::OK)
        }
    }
}

// Check if the given `Url` would cause `reqwest` to panic.
// This is a workaround for https://github.com/lycheeverse/lychee/issues/539
// and can be removed once https://github.com/seanmonstar/reqwest/pull/1399
// got merged.
// It is exactly the same check that reqwest runs internally, but unfortunately
// it `unwrap`s (and panics!) instead of returning an error, which we could handle.
fn validate_url(url: &Url) -> bool {
    http::Uri::try_from(url.as_str()).is_err()
}

/// A convenience function to check a single URI.
///
/// This provides the simplest link check utility without having to create a
/// [`Client`]. For more complex scenarios, see documentation of
/// [`ClientBuilder`] instead.
///
/// # Errors
///
/// Returns an `Err` if:
/// - The request client cannot be built (see [`ClientBuilder::client`] for
///   failure cases).
/// - The request cannot be checked (see [`Client::check`] for failure cases).
pub async fn check<T, E>(request: T) -> Result<Response>
where
    Request: TryFrom<T, Error = E>,
    ErrorKind: From<E>,
{
    let client = ClientBuilder::builder().build().client()?;
    client.check(request).await
}

#[cfg(test)]
mod tests {
    use std::{
        fs::File,
        time::{Duration, Instant},
    };

    use http::{header::HeaderMap, StatusCode};
    use reqwest::header;
    use tempfile::tempdir;

    use super::ClientBuilder;
    use crate::{mock_server, test_utils::get_mock_client_response, Uri};

    #[tokio::test]
    async fn test_nonexistent() {
        let mock_server = mock_server!(StatusCode::NOT_FOUND);
        let res = get_mock_client_response(mock_server.uri()).await;

        assert!(res.status().is_failure());
    }

    #[tokio::test]
    async fn test_nonexistent_with_path() {
        let res = get_mock_client_response("http://127.0.0.1/invalid").await;
        assert!(res.status().is_failure());
    }

    #[tokio::test]
    async fn test_exponential_backoff() {
        let mock_server = mock_server!(StatusCode::NOT_FOUND);

        let start = Instant::now();
        let res = get_mock_client_response(mock_server.uri()).await;
        let end = start.elapsed();

        assert!(res.status().is_failure());

        // on slow connections, this might take a bit longer than nominal
        // backed-off timeout (7 secs)
        assert!((7..=8).contains(&end.as_secs()));
    }

    #[tokio::test]
    async fn test_github() {
        let res = get_mock_client_response("https://github.com/lycheeverse/lychee").await;
        assert!(res.status().is_success());
    }

    #[tokio::test]
    async fn test_github_nonexistent_repo() {
        let res = get_mock_client_response("https://github.com/lycheeverse/not-lychee").await;
        assert!(res.status().is_failure());
    }

    #[tokio::test]
    async fn test_github_nonexistent_file() {
        let res = get_mock_client_response(
            "https://github.com/lycheeverse/lychee/blob/master/NON_EXISTENT_FILE.md",
        )
        .await;
        assert!(res.status().is_failure());
    }

    #[tokio::test]
    async fn test_youtube() {
        // This is applying a quirk. See the quirks module.
        let res = get_mock_client_response("https://www.youtube.com/watch?v=NlKuICiT470&list=PLbWDhxwM_45mPVToqaIZNbZeIzFchsKKQ&index=7").await;
        assert!(res.status().is_success());

        let res = get_mock_client_response("https://www.youtube.com/watch?v=invalidNlKuICiT470&list=PLbWDhxwM_45mPVToqaIZNbZeIzFchsKKQ&index=7").await;
        assert!(res.status().is_failure());
    }

    #[tokio::test]
    async fn test_non_github() {
        let mock_server = mock_server!(StatusCode::OK);
        let res = get_mock_client_response(mock_server.uri()).await;

        assert!(res.status().is_success());
    }

    #[tokio::test]
    async fn test_invalid_ssl() {
        let res = get_mock_client_response("https://expired.badssl.com/").await;

        assert!(res.status().is_failure());

        // Same, but ignore certificate error
        let res = ClientBuilder::builder()
            .allow_insecure(true)
            .build()
            .client()
            .unwrap()
            .check("https://expired.badssl.com/")
            .await
            .unwrap();
        assert!(res.status().is_success());
    }

    #[tokio::test]
    async fn test_file() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("temp");
        File::create(file).unwrap();
        let uri = format!("file://{}", dir.path().join("temp").to_str().unwrap());

        let res = get_mock_client_response(uri).await;
        assert!(res.status().is_success());
    }

    #[tokio::test]
    async fn test_custom_headers() {
        // See https://github.com/rust-lang/crates.io/issues/788
        let mut custom = HeaderMap::new();
        custom.insert(header::ACCEPT, "text/html".parse().unwrap());
        let res = ClientBuilder::builder()
            .custom_headers(custom)
            .build()
            .client()
            .unwrap()
            .check("https://crates.io/crates/lychee")
            .await
            .unwrap();
        assert!(res.status().is_success());
    }

    #[tokio::test]
    async fn test_exclude_mail() {
        let client = ClientBuilder::builder()
            .exclude_mail(false)
            .exclude_all_private(true)
            .build()
            .client()
            .unwrap();
        assert!(!client.is_excluded(&Uri {
            url: "mailto://mail@example.com".try_into().unwrap()
        }));

        let client = ClientBuilder::builder()
            .exclude_mail(true)
            .exclude_all_private(true)
            .build()
            .client()
            .unwrap();
        assert!(client.is_excluded(&Uri {
            url: "mailto://mail@example.com".try_into().unwrap()
        }));
    }

    #[tokio::test]
    async fn test_require_https() {
        let client = ClientBuilder::builder().build().client().unwrap();
        let res = client.check("http://example.com").await.unwrap();
        assert!(res.status().is_success());

        // Same request will fail if HTTPS is required
        let client = ClientBuilder::builder()
            .require_https(true)
            .build()
            .client()
            .unwrap();
        let res = client.check("http://example.com").await.unwrap();
        assert!(res.status().is_failure());
    }

    #[tokio::test]
    async fn test_timeout() {
        // Note: this checks response timeout, not connect timeout.
        // To check connect timeout, we'd have to do something more involved,
        // see: https://github.com/LukeMathWalker/wiremock-rs/issues/19
        let mock_delay = Duration::from_millis(20);
        let checker_timeout = Duration::from_millis(10);
        assert!(mock_delay > checker_timeout);

        let mock_server = mock_server!(StatusCode::OK, set_delay(mock_delay));

        let client = ClientBuilder::builder()
            .timeout(checker_timeout)
            .build()
            .client()
            .unwrap();

        let res = client.check(mock_server.uri()).await.unwrap();
        assert!(res.status().is_timeout());
    }

    #[tokio::test]
    async fn test_avoid_reqwest_panic() {
        let client = ClientBuilder::builder().build().client().unwrap();
        // This request will fail, but it won't panic
        let res = client.check("http://\"").await.unwrap();
        assert!(res.status().is_failure());
    }
}
