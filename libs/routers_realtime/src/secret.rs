//! Secret-bearing connection configuration.
//!
//! [`SecretUrl`] validates a URL at process configuration time, redacts
//! credentials in diagnostics, and only reconstructs a [`url::Url`] at the
//! connection boundary. It deliberately does not implement `Display`, which
//! keeps an endpoint from being interpolated into an error or log message by
//! accident.

use core::fmt;
use core::str::FromStr;

use secrecy::{ExposeSecret, SecretString};
use url::{ParseError, Url};

/// A validated URL whose complete representation, including credentials and
/// query parameters, is secret.
///
/// Its [`Debug`] implementation reports only the scheme, host, and explicit
/// port. Use [`SecretUrl::connection_url`] immediately before creating a NATS
/// or Valkey client; keep the returned [`Url`] out of diagnostic structures.
#[derive(Clone)]
pub struct SecretUrl(SecretString);

impl PartialEq for SecretUrl {
    fn eq(&self, other: &Self) -> bool {
        self.0.expose_secret() == other.0.expose_secret()
    }
}

impl Eq for SecretUrl {}

impl SecretUrl {
    /// Recreate the validated URL for immediate use by a network client.
    ///
    /// This is the intentional secret-exposure boundary. Callers must not log
    /// or retain the returned value in a diagnostic configuration type.
    #[must_use]
    pub fn connection_url(&self) -> Url {
        Url::parse(self.0.expose_secret()).expect("SecretUrl validates its URL during parsing")
    }

    fn sanitized_endpoint(&self) -> String {
        let url = self.connection_url();
        let Some(host) = url.host_str() else {
            return format!("{}:<no-host>", url.scheme());
        };
        match url.port() {
            Some(port) => format!("{}://{host}:{port}", url.scheme()),
            None => format!("{}://{host}", url.scheme()),
        }
    }
}

impl FromStr for SecretUrl {
    type Err = ParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Url::parse(value)?;
        Ok(Self(SecretString::from(value.to_owned())))
    }
}

impl fmt::Debug for SecretUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretUrl")
            .field("endpoint", &self.sanitized_endpoint())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_credentials_and_query_parameters() {
        let url: SecretUrl = "nats://operator:password@nats.example:4222/?token=abc"
            .parse()
            .expect("valid URL");

        assert_eq!(
            format!("{url:?}"),
            "SecretUrl { endpoint: \"nats://nats.example:4222\" }"
        );
    }
}
