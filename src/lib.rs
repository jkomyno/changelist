//! A small library shared by the binary and external callers.

/// Formats a greeting without taking ownership of the caller's name.
///
/// # Examples
///
/// ```
/// use rust_template::greeting;
///
/// assert_eq!(greeting("Rust"), "Hello, Rust!");
/// ```
pub fn greeting(name: &str) -> String {
    format!("Hello, {name}!")
}
