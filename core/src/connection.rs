//! Server addresses and the rules for reaching them.
//!
//! A server is an https base URL. Plain http is allowed only as an explicit,
//! warned opt-in, and then only to a private network address. [`ServerUrl`]
//! holds an address that has passed these rules, so every client built from
//! it starts from a checked address.

use std::fmt;

use url::{Host, Url};

/// Whether plain http may be used at all. Set only by the user's explicit,
/// warned opt-in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HttpPolicy {
    allow_private_http: bool,
}

impl HttpPolicy {
    /// https only. The default.
    pub const HTTPS_ONLY: Self = Self {
        allow_private_http: false,
    };

    /// The user ticked the warned opt-in: http is allowed to a private
    /// address, and nowhere else.
    pub const ALLOW_PRIVATE_HTTP: Self = Self {
        allow_private_http: true,
    };

    /// Whether this policy allows plain http at all.
    pub fn allows_plain_http(self) -> bool {
        self.allow_private_http
    }
}

/// Why a server address was refused. Carries no part of the address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UrlError {
    #[error("the server address is empty")]
    Empty,
    #[error("the server address is not a valid URL")]
    Invalid,
    #[error("only https:// addresses are accepted")]
    NotHttps,
    #[error("plain http needs the opt-in in the settings")]
    PlainHttpNeedsOptIn,
    #[error("plain http is only allowed to a private network address")]
    PlainHttpNotPrivate,
    #[error("the server address must not contain a query, fragment or user name")]
    NotABaseAddress,
}

/// A server base URL that has passed the address rules.
///
/// The stored form has no trailing slash ([`ServerUrl::key`]), which is also
/// the account name of the server's secrets.
#[derive(Clone, PartialEq, Eq)]
pub struct ServerUrl {
    /// Always ends with `/`, so endpoints can be joined onto it.
    base: Url,
}

impl ServerUrl {
    /// Parses and checks a server address typed or pasted by the user.
    pub fn parse(input: &str, policy: HttpPolicy) -> Result<Self, UrlError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(UrlError::Empty);
        }
        let mut url = Url::parse(trimmed).map_err(|_| UrlError::Invalid)?;
        match url.scheme() {
            "https" => {}
            "http" => {
                if !policy.allows_plain_http() {
                    return Err(UrlError::PlainHttpNeedsOptIn);
                }
                let private = url.host().is_some_and(|h| is_private_host(&h));
                if !private {
                    return Err(UrlError::PlainHttpNotPrivate);
                }
            }
            _ => return Err(UrlError::NotHttps),
        }
        if url.host().is_none() {
            return Err(UrlError::Invalid);
        }
        if url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(UrlError::NotABaseAddress);
        }
        let path = url.path().trim_end_matches('/').to_owned();
        url.set_path(&format!("{path}/"));
        Ok(Self { base: url })
    }

    /// The address without its trailing slash. This is the stored form and
    /// the account name of the server's secrets.
    pub fn key(&self) -> String {
        self.base.as_str().trim_end_matches('/').to_owned()
    }

    /// The base URL, ending with `/`.
    pub fn base(&self) -> &Url {
        &self.base
    }

    /// The URL of an endpoint relative to the base, such as `rest/ping.view`.
    pub fn endpoint(&self, relative: &str) -> Result<Url, UrlError> {
        self.base.join(relative).map_err(|_| UrlError::Invalid)
    }

    /// Whether the address is plain http.
    pub fn is_plain_http(&self) -> bool {
        self.base.scheme() == "http"
    }
}

impl fmt::Debug for ServerUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The address itself is not secret, but it may carry a private host
        // name; keep it out of debug output anyway.
        f.debug_struct("ServerUrl")
            .field("scheme", &self.base.scheme())
            .finish_non_exhaustive()
    }
}

/// Whether a host is on the local network: loopback, private, link-local,
/// a single-label name, or a `.local` name.
///
/// This checks the address as written. It does not resolve names, so a name
/// that points at a public address still passes; the opt-in is the user's
/// statement that the server is on their own network.
pub fn is_private_host(host: &Host<&str>) -> bool {
    match host {
        Host::Ipv4(ip) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        Host::Ipv6(ip) => {
            let first = ip.segments()[0];
            ip.is_loopback()
                || (first & 0xfe00) == 0xfc00 // unique local fc00::/7
                || (first & 0xffc0) == 0xfe80 // link-local fe80::/10
        }
        Host::Domain(name) => {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            name == "localhost" || name.ends_with(".local") || !name.contains('.')
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &str) -> Result<ServerUrl, UrlError> {
        ServerUrl::parse(input, HttpPolicy::HTTPS_ONLY)
    }

    #[test]
    fn https_addresses_are_normalised() {
        let url = parse("  https://music.example/folksonic  ").unwrap();
        assert_eq!(url.key(), "https://music.example/folksonic");
        assert_eq!(url.base().as_str(), "https://music.example/folksonic/");
        assert_eq!(
            url.endpoint("rest/ping.view").unwrap().as_str(),
            "https://music.example/folksonic/rest/ping.view"
        );
        assert_eq!(
            parse("https://music.example/").unwrap().key(),
            "https://music.example"
        );
    }

    #[test]
    fn empty_and_malformed_addresses_are_refused() {
        assert_eq!(parse("   "), Err(UrlError::Empty));
        assert_eq!(parse("music example"), Err(UrlError::Invalid));
        assert_eq!(parse("https://"), Err(UrlError::Invalid));
    }

    #[test]
    fn other_schemes_are_refused() {
        assert_eq!(parse("ftp://music.example"), Err(UrlError::NotHttps));
        assert_eq!(parse("file:///etc/passwd"), Err(UrlError::NotHttps));
    }

    #[test]
    fn http_needs_the_opt_in() {
        assert_eq!(
            parse("http://192.168.1.10:4533"),
            Err(UrlError::PlainHttpNeedsOptIn)
        );
    }

    #[test]
    fn http_opt_in_is_limited_to_private_addresses() {
        let policy = HttpPolicy::ALLOW_PRIVATE_HTTP;
        for ok in [
            "http://192.168.1.10:4533",
            "http://10.0.0.5",
            "http://172.16.4.4",
            "http://127.0.0.1:4533",
            "http://[::1]:4533",
            "http://[fd00::1]",
            "http://nas.local:4533",
            "http://nas:4533",
            "http://localhost:4533",
        ] {
            assert!(ServerUrl::parse(ok, policy).is_ok(), "{ok} should pass");
        }
        for refused in [
            "http://music.example",
            "http://8.8.8.8",
            "http://172.32.0.1",
            "http://[2001:db8::1]",
        ] {
            assert_eq!(
                ServerUrl::parse(refused, policy),
                Err(UrlError::PlainHttpNotPrivate),
                "{refused}"
            );
        }
    }

    #[test]
    fn queries_fragments_and_credentials_are_refused() {
        assert_eq!(
            parse("https://music.example/?apiKey=x"),
            Err(UrlError::NotABaseAddress)
        );
        assert_eq!(
            parse("https://music.example/#x"),
            Err(UrlError::NotABaseAddress)
        );
        assert_eq!(
            parse("https://user:pw@music.example"),
            Err(UrlError::NotABaseAddress)
        );
    }

    #[test]
    fn debug_output_hides_the_address() {
        let url = parse("https://secret-host.example/x").unwrap();
        let text = format!("{url:?}");
        assert!(!text.contains("secret-host"), "{text}");
    }
}
