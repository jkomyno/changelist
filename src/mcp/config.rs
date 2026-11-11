//! How to reach one MCP server, and what to trust about it.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;
use url::Url;

/// Largest server configuration file accepted.
pub const MAX_SERVER_CONFIG_BYTES: usize = 64 * 1024;

/// Longest identity a plan's scope can hold.
const MAX_IDENTITY_BYTES: usize = 128;

/// Shortest configured value removed from a server's text. A shorter value
/// is not a credential, and removing it would garble ordinary words.
const MIN_REDACTED_BYTES: usize = 8;

/// A server configuration that cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// The file a user writes: either `url` (Streamable HTTP) or `command` (stdio).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerFile {
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    trust_annotations: bool,
    #[serde(default)]
    read_tools: Vec<String>,
    write_tools: Option<Vec<String>>,
}

/// How to reach the server.
#[derive(Clone)]
pub enum Endpoint {
    /// Streamable HTTP: every message is a POST to `url`.
    Http {
        /// The MCP endpoint.
        url: Url,
        /// Extra request headers, with `${NAME}` references already replaced.
        /// They may hold credentials and are never printed.
        headers: Vec<(String, String)>,
    },
    /// stdio: a subprocess started by this client.
    Stdio {
        /// Program to start.
        command: String,
        /// Its arguments.
        args: Vec<String>,
        /// Extra environment variables, with `${NAME}` references already replaced.
        env: Vec<(String, String)>,
    },
}

impl fmt::Debug for Endpoint {
    // Header and environment values may be credentials; print names only.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = |pairs: &[(String, String)]| -> Vec<String> {
            pairs.iter().map(|(name, _)| name.clone()).collect()
        };
        match self {
            Self::Http { url, headers } => formatter
                .debug_struct("Http")
                .field(
                    "url",
                    &format_args!("{}{}", url.origin().ascii_serialization(), url.path()),
                )
                .field("headers", &names(headers))
                .finish(),
            Self::Stdio { command, args, env } => formatter
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", args)
                .field("env", &names(env))
                .finish(),
        }
    }
}

/// One MCP server and the trust decisions a person made about it.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// How to reach it.
    pub endpoint: Endpoint,
    /// Whether the server's tool annotations (`readOnlyHint`, `idempotentHint`)
    /// are believed. The specification calls them hints that a client must not
    /// rely on unless it trusts the server, so this is `false` unless stated.
    pub trust_annotations: bool,
    /// Tools a program may call while it investigates, named by a person. Used
    /// in addition to trusted `readOnlyHint` annotations.
    pub read_tools: Vec<String>,
    /// The only tools a plan may hold, named by a person. `None` allows every
    /// tool that is not a read. A tool named here that is a read stays a read.
    pub write_tools: Option<Vec<String>>,
}

impl ServerConfig {
    /// Parse a server file, taking `${NAME}` values from `lookup`.
    ///
    /// ```
    /// use codemode::mcp::{Endpoint, ServerConfig};
    ///
    /// let text = r#"{"url":"https://mcp.example.com/mcp","headers":{"x-api-key":"${KEY}"}}"#;
    /// let config = ServerConfig::parse(text, |name| (name == "KEY").then(|| "s3cret".to_string()))?;
    /// assert_eq!(config.identity(), "https://mcp.example.com/mcp");
    /// assert!(matches!(config.endpoint, Endpoint::Http { .. }));
    /// // The identity never includes a header value.
    /// assert!(!format!("{config:?}").contains("s3cret"));
    /// # Ok::<(), codemode::mcp::ConfigError>(())
    /// ```
    ///
    /// # Errors
    /// Returns a [`ConfigError`] for invalid JSON, an unknown field, both or
    /// neither of `url` and `command`, a URL that is not `https` (plain `http`
    /// is accepted for a loopback host only), a URL with a user name or
    /// password, or a `${NAME}` that `lookup` does not know.
    pub fn parse(text: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        if text.len() > MAX_SERVER_CONFIG_BYTES {
            return Err(ConfigError("the server file is too large".into()));
        }
        let file: ServerFile = serde_json::from_str(text)
            .map_err(|error| ConfigError(format!("not a server file: {error}")))?;
        let expand_all = |pairs: BTreeMap<String, String>, what: &str| {
            pairs
                .into_iter()
                .map(|(name, value)| {
                    let value = expand(&value, &lookup)
                        .map_err(|error| ConfigError(format!("{what} {name:?}: {error}")))?;
                    Ok((name, value))
                })
                .collect::<Result<Vec<_>, ConfigError>>()
        };
        let endpoint = match (file.url, file.command) {
            (Some(url), None) => {
                if !file.args.is_empty() || !file.env.is_empty() {
                    return Err(ConfigError(
                        "\"args\" and \"env\" belong to a \"command\" server".into(),
                    ));
                }
                let url = Url::parse(&url)
                    .map_err(|error| ConfigError(format!("\"url\" is not a URL: {error}")))?;
                check_url(&url)?;
                let headers = expand_all(file.headers, "header")?;
                for (name, value) in &headers {
                    check_header(name, value)?;
                }
                Endpoint::Http { url, headers }
            }
            (None, Some(command)) => {
                if !file.headers.is_empty() {
                    return Err(ConfigError("\"headers\" belong to a \"url\" server".into()));
                }
                if command.is_empty() {
                    return Err(ConfigError("\"command\" is empty".into()));
                }
                Endpoint::Stdio {
                    command,
                    args: file.args,
                    env: expand_all(file.env, "environment variable")?,
                }
            }
            _ => {
                return Err(ConfigError(
                    "a server file needs exactly one of \"url\" and \"command\"".into(),
                ));
            }
        };
        Ok(Self {
            endpoint,
            trust_annotations: file.trust_annotations,
            read_tools: file.read_tools,
            write_tools: file.write_tools,
        })
    }

    /// Whether a plan may hold the write tool `name`: always when the file has
    /// no `write_tools`, otherwise only when `write_tools` names it.
    ///
    /// ```
    /// use codemode::mcp::ServerConfig;
    ///
    /// let open = ServerConfig::parse(r#"{"command":"srv"}"#, |_| None)?;
    /// assert!(open.allows_write("notes.delete"));
    /// let narrow = ServerConfig::parse(r#"{"command":"srv","write_tools":["notes.put"]}"#, |_| None)?;
    /// assert!(narrow.allows_write("notes.put") && !narrow.allows_write("notes.delete"));
    /// # Ok::<(), codemode::mcp::ConfigError>(())
    /// ```
    #[must_use]
    pub fn allows_write(&self, name: &str) -> bool {
        self.write_tools
            .as_ref()
            .is_none_or(|allowed| allowed.iter().any(|tool| tool == name))
    }

    /// The name an approval binds as the account. For a URL: the URL without
    /// its fragment, with a query replaced by `?sha256=` and 24 hexadecimal
    /// digits of its SHA-256. For a command: `stdio:` followed by the command
    /// and its arguments as a JSON array. It never includes a header or
    /// environment value. A name longer than 128 bytes, the most a
    /// plan's scope holds, is cut to its first 96 bytes followed by `#` and 24
    /// hexadecimal digits of the SHA-256 of the whole name.
    #[must_use]
    pub fn identity(&self) -> String {
        let full = self.full_identity();
        if full.len() <= MAX_IDENTITY_BYTES {
            return full;
        }
        let digest = crate::plan::Digest::of(full.as_bytes());
        let hex = digest.as_str().trim_start_matches("sha256:");
        format!("{}#{}", crate::mcp::wire::truncate(&full, 96), &hex[..24])
    }

    /// The configured values that a server's text must not carry into a
    /// journal or an output: every header value and `env` value, whole and
    /// split at white space (so the token of `Bearer <token>` counts by
    /// itself), and the URL's query, whole and as each of its values, both as
    /// written and decoded. Values under 8 bytes are left out. The longest
    /// comes first, so a value is removed before a part of it.
    pub(crate) fn redactions(&self) -> Vec<String> {
        let mut values: Vec<String> = Vec::new();
        let mut add = |value: &str| {
            values.push(value.to_string());
            values.extend(value.split_whitespace().map(String::from));
        };
        match &self.endpoint {
            Endpoint::Http { url, headers } => {
                for (_, value) in headers {
                    add(value);
                }
                if let Some(query) = url.query() {
                    add(query);
                    for pair in query.split('&') {
                        add(pair.split_once('=').map_or(pair, |(_, value)| value));
                    }
                    for (_, value) in url.query_pairs() {
                        add(&value);
                    }
                }
            }
            Endpoint::Stdio { env, .. } => {
                for (_, value) in env {
                    add(value);
                }
            }
        }
        values.retain(|value| value.len() >= MIN_REDACTED_BYTES);
        values.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        values.dedup();
        values
    }

    /// The whole command and arguments of a stdio server, as `stdio:` and a
    /// JSON array, when [`Self::identity`] had to shorten them; `None` for
    /// an identity that is whole, and for a URL. A review prints it, because
    /// the command will be executed.
    ///
    /// ```
    /// use codemode::mcp::ServerConfig;
    ///
    /// let short = ServerConfig::parse(r#"{"command":"srv","args":["--db","x"]}"#, |_| None)?;
    /// assert_eq!(short.full_command(), None);
    /// let long = format!(r#"{{"command":"srv","args":["{}","--last"]}}"#, "a".repeat(200));
    /// let long = ServerConfig::parse(&long, |_| None)?;
    /// assert!(long.identity().len() <= 128);
    /// assert!(long.full_command().expect("shortened").ends_with(r#"","--last"]"#));
    /// # Ok::<(), codemode::mcp::ConfigError>(())
    /// ```
    #[must_use]
    pub fn full_command(&self) -> Option<String> {
        let full = self.full_identity();
        (matches!(self.endpoint, Endpoint::Stdio { .. }) && full.len() > MAX_IDENTITY_BYTES)
            .then_some(full)
    }

    fn full_identity(&self) -> String {
        match &self.endpoint {
            Endpoint::Http { url, .. } => {
                let mut shown = url.clone();
                shown.set_query(None);
                shown.set_fragment(None);
                match url.query() {
                    // A query can select a tenant, so it is bound; it can also
                    // hold a key, so only its digest is shown.
                    Some(query) if !query.is_empty() => {
                        let digest = crate::plan::Digest::of(query.as_bytes());
                        let hex = digest.as_str().trim_start_matches("sha256:");
                        format!("{shown}?sha256={}", &hex[..24])
                    }
                    _ => shown.to_string(),
                }
            }
            Endpoint::Stdio { command, args, .. } => {
                // As JSON, so ["a b"] and ["a", "b"] are different servers.
                let mut argv = vec![command.as_str()];
                argv.extend(args.iter().map(String::as_str));
                format!("stdio:{}", serde_json::json!(argv))
            }
        }
    }
}

fn check_url(url: &Url) -> Result<(), ConfigError> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ConfigError(
            "\"url\" must not hold a user name or password; use \"headers\"".into(),
        ));
    }
    let loopback = match url.host() {
        Some(url::Host::Domain(name)) => name == "localhost",
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    match url.scheme() {
        "https" => Ok(()),
        "http" if loopback => Ok(()),
        "http" => Err(ConfigError(
            "\"url\" must use https; plain http is accepted for a loopback host only".into(),
        )),
        other => Err(ConfigError(format!(
            "\"url\" has scheme {other:?}; expected https"
        ))),
    }
}

/// `text` with every one of `values` replaced by `[redacted]`.
pub(crate) fn redact(text: &str, values: &[String]) -> String {
    let mut text = text.to_string();
    for value in values {
        if text.contains(value.as_str()) {
            text = text.replace(value.as_str(), "[redacted]");
        }
    }
    text
}

/// Headers this client sets itself; a server file must not override them.
const RESERVED_HEADERS: &[&str] = &[
    "accept",
    "content-type",
    "content-length",
    "host",
    "mcp-protocol-version",
    "mcp-method",
    "mcp-name",
    "mcp-session-id",
];

fn check_header(name: &str, value: &str) -> Result<(), ConfigError> {
    let lower = name.to_ascii_lowercase();
    if RESERVED_HEADERS.contains(&lower.as_str()) || lower.starts_with("mcp-param-") {
        return Err(ConfigError(format!(
            "header {name:?} is set by the protocol and cannot be configured"
        )));
    }
    let token = !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte));
    if !token {
        return Err(ConfigError(format!("header name {name:?} is not valid")));
    }
    let visible = value
        .bytes()
        .all(|byte| byte == b'\t' || (0x20..0x7f).contains(&byte));
    if !visible {
        return Err(ConfigError(format!(
            "the value of header {name:?} has characters a header cannot carry"
        )));
    }
    Ok(())
}

/// Replace every `${NAME}` in `value` with `lookup(NAME)`.
fn expand(value: &str, lookup: &impl Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| "\"${\" without a closing \"}\"".to_string())?;
        let name = &after[..end];
        let valid = !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
        if !valid {
            return Err(format!("\"${{{name}}}\" is not a variable name"));
        }
        let found =
            lookup(name).ok_or_else(|| format!("environment variable {name} is not set"))?;
        out.push_str(&found);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn expands_references_and_reports_a_missing_variable() {
        let lookup = |name: &str| (name == "TOKEN").then(|| "abc".to_string());
        assert_eq!(expand("Bearer ${TOKEN}", &lookup).unwrap(), "Bearer abc");
        assert_eq!(expand("plain", &lookup).unwrap(), "plain");
        assert_eq!(
            expand("${OTHER}", &lookup).unwrap_err(),
            "environment variable OTHER is not set"
        );
        assert!(expand("${TOKEN", &lookup).is_err());
        assert!(expand("${a-b}", &lookup).is_err());
    }

    #[test]
    fn identity_drops_the_query_and_never_holds_a_secret() {
        let config = ServerConfig::parse(
            r#"{"url":"https://h.example/mcp/s1?api_key=zzz#f","headers":{"x-api-key":"k"}}"#,
            none,
        )
        .unwrap();
        let identity = config.identity();
        assert!(
            identity.starts_with("https://h.example/mcp/s1?sha256="),
            "{identity}"
        );
        assert!(!identity.contains("zzz") && !format!("{config:?}").contains("zzz"));
        let other = ServerConfig::parse(r#"{"url":"https://h.example/mcp/s1?api_key=yyy"}"#, none);
        assert_ne!(identity, other.unwrap().identity());
        let bare = ServerConfig::parse(r#"{"url":"https://h.example/mcp/s1"}"#, none).unwrap();
        assert_eq!(bare.identity(), "https://h.example/mcp/s1");
        let stdio = ServerConfig::parse(
            r#"{"command":"srv","args":["--db","x"],"env":{"T":"v"}}"#,
            none,
        )
        .unwrap();
        assert_eq!(stdio.identity(), r#"stdio:["srv","--db","x"]"#);
        let joined = ServerConfig::parse(r#"{"command":"srv","args":["--db x"]}"#, none).unwrap();
        assert_ne!(stdio.identity(), joined.identity());
        let long = format!(r#"{{"command":"srv","args":["{}"]}}"#, "é".repeat(100));
        let long = ServerConfig::parse(&long, none).unwrap().identity();
        assert!(
            long.len() <= 128 && long.starts_with(r#"stdio:["srv","é"#),
            "{long}"
        );
        let other = format!(r#"{{"command":"srv","args":["{}x"]}}"#, "é".repeat(100));
        assert_ne!(long, ServerConfig::parse(&other, none).unwrap().identity());
        assert!(!format!("{stdio:?}").contains("\"v\""));
    }

    #[test]
    fn configured_values_are_removed_from_a_servers_text() {
        let http = ServerConfig::parse(
            r#"{"url":"https://h.example/mcp?tenant=acme-co-42&k=a%20long%20key&n=7",
                "headers":{"authorization":"Bearer tok-0123456789","x-short":"abc"}}"#,
            none,
        )
        .unwrap();
        let values = http.redactions();
        let echoed = "bad token tok-0123456789 (Bearer tok-0123456789) for acme-co-42 \
                      at /mcp?tenant=acme-co-42&k=a%20long%20key&n=7, key 'a long key', abc 7";
        assert_eq!(
            redact(echoed, &values),
            "bad token [redacted] ([redacted]) for [redacted] at /mcp?[redacted], \
             key '[redacted]', abc 7"
        );
        assert_eq!(redact("upstream said no", &values), "upstream said no");

        let stdio = ServerConfig::parse(
            r#"{"command":"srv","args":["--verbose"],"env":{"TOKEN":"env-secret-value","MODE":"fast"}}"#,
            none,
        )
        .unwrap();
        assert_eq!(
            redact("fast mode refused env-secret-value", &stdio.redactions()),
            "fast mode refused [redacted]"
        );
    }

    #[test]
    fn refuses_unsafe_or_ambiguous_files() {
        let refused = |text: &str| ServerConfig::parse(text, none).unwrap_err().0;
        assert!(refused(r#"{"url":"http://example.com/mcp"}"#).contains("https"));
        assert!(refused(r#"{"url":"https://u:p@example.com/mcp"}"#).contains("user name"));
        assert!(refused(r#"{"url":"https://e.com","command":"x"}"#).contains("exactly one"));
        assert!(refused(r#"{}"#).contains("exactly one"));
        assert!(
            refused(r#"{"url":"https://e.com","headers":{"Mcp-Name":"x"}}"#)
                .contains("set by the protocol")
        );
        assert!(
            refused(r#"{"url":"https://e.com","headers":{"a":"b\nc"}}"#).contains("cannot carry")
        );
        assert!(refused(r#"{"url":"https://e.com","surprise":1}"#).contains("not a server file"));
        ServerConfig::parse(r#"{"url":"http://127.0.0.1:9/mcp"}"#, none).unwrap();
        ServerConfig::parse(r#"{"url":"http://localhost:9/mcp"}"#, none).unwrap();
    }
}
