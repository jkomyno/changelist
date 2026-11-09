//! Finite, immutable operation plans.
//!
//! A plan lists the external operations an agent or a person proposes, each with a
//! stable identity, a tool, its arguments, the provider account it acts through,
//! and any provider-checked preconditions. [`Plan::parse`] validates a plan and
//! freezes it into a [`FrozenPlan`] whose canonical JSON and SHA-256 digest never
//! change; approvals and journals bind to that digest.
//!
//! A plan is data, not a program. There are no dependencies between operations,
//! no control flow, and no variables: recovery works from the list itself.

use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

/// The plan format version this build accepts.
pub const PLAN_FORMAT_VERSION: u32 = 1;
/// Largest number of operations in one plan.
pub const MAX_OPERATIONS: usize = 256;
/// Largest serialized arguments of one operation, matching the guest call frame.
pub const MAX_ARGUMENT_BYTES: usize = 64 * 1024;
/// Largest plan text accepted by [`Plan::parse`].
pub const MAX_PLAN_BYTES: usize = 1024 * 1024;
const MAX_NAME_BYTES: usize = 128;
const MAX_SUMMARY_BYTES: usize = 1024;

/// Argument keys that name credentials, compared after lowercasing and removing
/// `-` and `_`. A plan is persisted in its journal, so it must not carry secrets;
/// credentials belong to the trusted provider. This is a guard for obvious
/// mistakes, not a secret scanner.
const CREDENTIAL_KEYS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "clientsecret",
    "token",
    "accesstoken",
    "refreshtoken",
    "authtoken",
    "idtoken",
    "bearer",
    "apikey",
    "accesskey",
    "secretkey",
    "privatekey",
    "authorization",
    "credential",
    "credentials",
    "cookie",
];

/// A versioned list of proposed external operations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// Plan format version; must equal [`PLAN_FORMAT_VERSION`].
    pub version: u32,
    /// A short human description shown when the plan is reviewed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// The operations, in the order they are offered to the scheduler.
    pub operations: Vec<Operation>,
}

/// One proposed external operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    /// Stable identity, unique in the plan: 1 to 64 of `A-Z a-z 0-9 . _ -`.
    /// It survives every attempt, retry, and trace.
    pub id: String,
    /// Provider tool name.
    pub tool: String,
    /// The provider account the operation acts through, and its destination.
    pub scope: Scope,
    /// JSON object passed to the provider; at most [`MAX_ARGUMENT_BYTES`] serialized.
    pub arguments: Value,
    /// Provider-defined conditions the provider must check when it applies the
    /// operation, for example that an issue is still open. Each is a JSON object.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preconditions: Vec<Value>,
}

/// Where an operation acts: which provider, as which account, and on what.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    /// Provider name, which must match the executing provider's declaration.
    pub provider: String,
    /// Account the provider acts as. An identifier, never a credential.
    pub account: String,
    /// The resource the operation changes, such as a repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
}

impl fmt::Display for Scope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.provider, self.account)?;
        if let Some(destination) = &self.destination {
            write!(formatter, " on {destination}")?;
        }
        Ok(())
    }
}

/// A SHA-256 digest written as `sha256:` followed by 64 lowercase hex digits.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Digest(String);

impl Digest {
    /// Digest `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        let hash = Sha256::digest(bytes);
        let mut text = String::with_capacity(71);
        text.push_str("sha256:");
        for byte in hash {
            text.push_str(&format!("{byte:02x}"));
        }
        Self(text)
    }

    /// The digest text, `sha256:<hex>`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first 12 hex digits, for display.
    #[must_use]
    pub fn short(&self) -> &str {
        &self.0[7..19]
    }
}

impl TryFrom<String> for Digest {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        let valid = text.len() == 71
            && text.starts_with("sha256:")
            && text[7..]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if valid {
            Ok(Self(text))
        } else {
            Err("a digest is sha256: followed by 64 lowercase hex digits".into())
        }
    }
}

impl From<Digest> for String {
    fn from(digest: Digest) -> Self {
        digest.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Why a plan was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// The text is not a plan: invalid JSON, a missing field, or an unknown field.
    Format(String),
    /// The plan text exceeds [`MAX_PLAN_BYTES`].
    TooLarge,
    /// The plan declares a format version this build does not accept.
    UnsupportedVersion(u32),
    /// The plan has no operations, or more than [`MAX_OPERATIONS`].
    OperationCount(usize),
    /// An operation field is invalid; `operation` is its index or identity.
    Invalid {
        /// The operation's identity, or `#<index>` when the identity itself is invalid.
        operation: String,
        /// What is wrong.
        reason: String,
    },
    /// Two operations share an identity.
    DuplicateId(String),
    /// An argument or precondition key names a credential.
    CredentialLike {
        /// The operation's identity.
        operation: String,
        /// The offending key.
        key: String,
    },
}

impl fmt::Display for PlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format(message) => write!(formatter, "not a valid plan: {message}"),
            Self::TooLarge => write!(formatter, "plan exceeds {MAX_PLAN_BYTES} bytes"),
            Self::UnsupportedVersion(version) => write!(
                formatter,
                "plan version {version} is not supported; expected {PLAN_FORMAT_VERSION}"
            ),
            Self::OperationCount(count) => write!(
                formatter,
                "a plan needs 1 to {MAX_OPERATIONS} operations, found {count}"
            ),
            Self::Invalid { operation, reason } => {
                write!(formatter, "operation {operation}: {reason}")
            }
            Self::DuplicateId(id) => write!(formatter, "operation id '{id}' appears twice"),
            Self::CredentialLike { operation, key } => write!(
                formatter,
                "operation {operation}: key '{key}' looks like a credential; credentials \
                 stay with the provider and must not appear in a plan"
            ),
        }
    }
}

impl std::error::Error for PlanError {}

/// A validated plan with its canonical JSON and digest; it cannot be modified.
#[derive(Debug, Clone, PartialEq)]
pub struct FrozenPlan {
    plan: Plan,
    canonical: String,
    digest: Digest,
}

impl FrozenPlan {
    /// The validated plan.
    #[must_use]
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// Canonical JSON: object keys sorted, no insignificant whitespace.
    #[must_use]
    pub fn canonical(&self) -> &str {
        &self.canonical
    }

    /// SHA-256 of the canonical JSON.
    #[must_use]
    pub fn digest(&self) -> &Digest {
        &self.digest
    }

    /// The operation with identity `id`.
    #[must_use]
    pub fn operation(&self, id: &str) -> Option<&Operation> {
        self.plan
            .operations
            .iter()
            .find(|operation| operation.id == id)
    }
}

impl Plan {
    /// Parse, validate, and freeze a plan from JSON text.
    ///
    /// # Examples
    ///
    /// ```
    /// let plan = codemode::Plan::parse(r#"{"version": 1, "operations": [
    ///   {"id": "label-101", "tool": "issues.add_label",
    ///    "scope": {"provider": "fixture.issues", "account": "triage-bot"},
    ///    "arguments": {"issue": 101, "label": "triage"}}]}"#)?;
    /// assert!(plan.digest().as_str().starts_with("sha256:"));
    /// # Ok::<(), codemode::PlanError>(())
    /// ```
    ///
    /// # Errors
    /// Returns [`PlanError`] for malformed JSON, unknown fields, an unsupported
    /// version, invalid identities or names, non-object arguments, oversized
    /// arguments, duplicate identities, or credential-like keys.
    pub fn parse(text: &str) -> Result<FrozenPlan, PlanError> {
        if text.len() > MAX_PLAN_BYTES {
            return Err(PlanError::TooLarge);
        }
        let plan: Self =
            serde_json::from_str(text).map_err(|error| PlanError::Format(error.to_string()))?;
        plan.freeze()
    }

    /// Validate this plan and freeze it.
    ///
    /// # Errors
    /// The same as [`Plan::parse`], except for JSON syntax.
    pub fn freeze(self) -> Result<FrozenPlan, PlanError> {
        self.validate()?;
        // Through `Value`, whose maps are sorted, so key order never depends on input.
        let value = serde_json::to_value(&self).map_err(|e| PlanError::Format(e.to_string()))?;
        let canonical =
            serde_json::to_string(&value).map_err(|e| PlanError::Format(e.to_string()))?;
        // A journal stores this text and reads it back to recover: refuse a plan it
        // could not read, whether too large or nested too deeply for the parser.
        if canonical.len() > MAX_PLAN_BYTES {
            return Err(PlanError::TooLarge);
        }
        serde_json::from_str::<Value>(&canonical).map_err(|error| {
            PlanError::Format(format!("the canonical plan cannot be read back: {error}"))
        })?;
        let digest = Digest::of(canonical.as_bytes());
        Ok(FrozenPlan {
            plan: self,
            canonical,
            digest,
        })
    }

    fn validate(&self) -> Result<(), PlanError> {
        if self.version != PLAN_FORMAT_VERSION {
            return Err(PlanError::UnsupportedVersion(self.version));
        }
        if self
            .summary
            .as_ref()
            .is_some_and(|summary| summary.len() > MAX_SUMMARY_BYTES)
        {
            return Err(PlanError::Format(format!(
                "summary exceeds {MAX_SUMMARY_BYTES} bytes"
            )));
        }
        let count = self.operations.len();
        if count == 0 || count > MAX_OPERATIONS {
            return Err(PlanError::OperationCount(count));
        }
        let mut seen = HashSet::new();
        for (index, operation) in self.operations.iter().enumerate() {
            if !valid_id(&operation.id) {
                return Err(PlanError::Invalid {
                    operation: format!("#{index}"),
                    reason: "id must be 1 to 64 of A-Z a-z 0-9 . _ -".into(),
                });
            }
            if !seen.insert(operation.id.as_str()) {
                return Err(PlanError::DuplicateId(operation.id.clone()));
            }
            operation.validate()?;
        }
        Ok(())
    }
}

impl Operation {
    fn validate(&self) -> Result<(), PlanError> {
        let invalid = |reason: &str| PlanError::Invalid {
            operation: self.id.clone(),
            reason: reason.into(),
        };
        let names = [
            ("tool", Some(&self.tool)),
            ("scope.provider", Some(&self.scope.provider)),
            ("scope.account", Some(&self.scope.account)),
            ("scope.destination", self.scope.destination.as_ref()),
        ];
        for (field, value) in names {
            if let Some(value) = value {
                if value.is_empty()
                    || value.len() > MAX_NAME_BYTES
                    || value.chars().any(char::is_control)
                {
                    return Err(invalid(&format!(
                        "{field} must be 1 to {MAX_NAME_BYTES} bytes without control characters"
                    )));
                }
            }
        }
        if !self.arguments.is_object() {
            return Err(invalid("arguments must be a JSON object"));
        }
        let bytes = serde_json::to_vec(&self.arguments).map_or(usize::MAX, |bytes| bytes.len());
        if bytes > MAX_ARGUMENT_BYTES {
            return Err(invalid(&format!(
                "arguments exceed {MAX_ARGUMENT_BYTES} serialized bytes"
            )));
        }
        if self
            .preconditions
            .iter()
            .any(|condition| !condition.is_object())
        {
            return Err(invalid("each precondition must be a JSON object"));
        }
        let values = std::iter::once(&self.arguments).chain(&self.preconditions);
        for value in values {
            if let Some(key) = credential_key(value) {
                return Err(PlanError::CredentialLike {
                    operation: self.id.clone(),
                    key,
                });
            }
            if has_inexact_integer(value) {
                return Err(invalid(
                    "an integer is outside -(2^53-1)..=2^53-1, which a JavaScript host would round",
                ));
            }
        }
        Ok(())
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// Largest integer every JSON host carries exactly (`Number.MAX_SAFE_INTEGER`).
const MAX_EXACT_INTEGER: u64 = (1 << 53) - 1;

/// Whether `value` holds an integer a JavaScript host, such as a bridge client,
/// would round: the host would then act on a value nobody approved.
fn has_inexact_integer(value: &Value) -> bool {
    match value {
        Value::Number(number) => {
            number.as_u64().is_some_and(|n| n > MAX_EXACT_INTEGER)
                || number
                    .as_i64()
                    .is_some_and(|n| n.unsigned_abs() > MAX_EXACT_INTEGER)
        }
        Value::Array(items) => items.iter().any(has_inexact_integer),
        Value::Object(map) => map.values().any(has_inexact_integer),
        _ => false,
    }
}

/// The first object key, at any depth, that names a credential.
fn credential_key(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => map.iter().find_map(|(key, value)| {
            let normalized: String = key
                .chars()
                .filter(|c| !matches!(c, '-' | '_'))
                .flat_map(char::to_lowercase)
                .collect();
            if CREDENTIAL_KEYS.contains(&normalized.as_str()) {
                Some(key.clone())
            } else {
                credential_key(value)
            }
        }),
        Value::Array(items) => items.iter().find_map(credential_key),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(operations: &str) -> String {
        format!(r#"{{"version":1,"operations":[{operations}]}}"#)
    }

    const ADD: &str = r#"{"id":"a","tool":"t","scope":{"provider":"p","account":"x"},"arguments":{"label":"bug","issue":1}}"#;

    #[test]
    fn digest_ignores_key_order_and_whitespace_but_not_values() {
        let reordered = r#"{"arguments":{"issue":1,"label":"bug"},"scope":{"account":"x","provider":"p"},"tool":"t","id":"a"}"#;
        let first = Plan::parse(&plan(ADD)).expect("valid");
        let second = Plan::parse(&format!(
            "{{ \"operations\": [ {reordered} ], \"version\": 1 }}"
        ))
        .expect("valid");
        assert_eq!(first.digest(), second.digest());
        let changed = Plan::parse(&plan(&ADD.replace("bug", "feature"))).expect("valid");
        assert_ne!(first.digest(), changed.digest());
        let account = Plan::parse(&plan(&ADD.replace(r#""x""#, r#""y""#))).expect("valid");
        assert_ne!(first.digest(), account.digest());
    }

    #[test]
    fn invalid_plans_are_refused_with_the_operation_named() {
        let cases = [
            (r#"{"version":2,"operations":[]}"#.to_string(), "version 2"),
            (plan(""), "1 to 256"),
            (plan(&format!("{ADD},{ADD}")), "appears twice"),
            (plan(&ADD.replace(r#""id":"a""#, r#""id":"a b""#)), "#0"),
            (
                plan(&ADD.replace(r#"{"label":"bug","issue":1}"#, "[1]")),
                "JSON object",
            ),
            (
                plan(&ADD.replace(r#""label":"bug""#, r#""auth":{"api_key":"k"}"#)),
                "api_key",
            ),
            (
                plan(&ADD.replace(r#""tool":"t""#, r#""tool":"t","extra":1"#)),
                "extra",
            ),
        ];
        for (text, expected) in cases {
            let error = Plan::parse(&text).expect_err(&text).to_string();
            assert!(
                error.contains(expected),
                "{error} should mention {expected}"
            );
        }
    }

    #[test]
    fn a_plan_frozen_in_code_must_read_back() {
        let mut nested = Value::from(1);
        for _ in 0..130 {
            nested = Value::Array(vec![nested]);
        }
        let plan = Plan {
            version: PLAN_FORMAT_VERSION,
            summary: None,
            operations: vec![Operation {
                id: "a".into(),
                tool: "t".into(),
                scope: Scope {
                    provider: "p".into(),
                    account: "x".into(),
                    destination: None,
                },
                arguments: serde_json::json!({ "deep": nested }),
                preconditions: Vec::new(),
            }],
        };
        let error = plan.freeze().expect_err("too deep to read back");
        assert!(error.to_string().contains("cannot be read back"), "{error}");
    }
}
