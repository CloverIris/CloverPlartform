//! Data binding.
//!
//! Deliberately tiny: a flat key → value map, resolved while the tree is built.
//! That is enough to drive a real app UI without inventing a dependency-tracking
//! runtime, and it keeps the "no work on idle" property: no observers, no
//! invalidation graph, just a value read when the page is rebuilt.
//!
//! Markup uses `{Binding key}` in any text-ish attribute:
//!
//! ```ignore
//! <TextBlock Text="{Binding title}"/>
//! <Button Content="{Binding action}" Command="{Binding command}"/>
//! ```
//!
//! Typed attributes resolve through the typed accessor, so a colour binding never
//! round-trips through a string.

use std::collections::HashMap;

use clover_core::Color;

/// A bound value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Text(String),
    Number(f64),
    Bool(bool),
    Color(Color),
}

impl Value {
    pub fn text(&self) -> Option<&str> {
        match self {
            Value::Text(text) => Some(text),
            _ => None,
        }
    }

    pub fn number(&self) -> Option<f64> {
        match self {
            Value::Number(value) => Some(*value),
            _ => None,
        }
    }

    pub fn boolean(&self) -> Option<bool> {
        match self {
            Value::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn color(&self) -> Option<Color> {
        match self {
            Value::Color(color) => Some(*color),
            _ => None,
        }
    }

    /// Best-effort textual form, used when a text attribute binds a non-text value.
    pub fn to_display_text(&self) -> String {
        match self {
            Value::Text(text) => text.clone(),
            Value::Number(value) => {
                if (value.fract()).abs() < f64::EPSILON {
                    format!("{}", *value as i64)
                } else {
                    format!("{value}")
                }
            }
            Value::Bool(value) => (if *value { "true" } else { "false" }).to_string(),
            Value::Color(color) => format!("#{:02X}{:02X}{:02X}", color.r, color.g, color.b),
        }
    }
}

/// Flat key → value store for one page build.
#[derive(Clone, Debug, Default)]
pub struct DataContext {
    values: HashMap<String, Value>,
}

impl DataContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(mut self, key: impl Into<String>, value: Value) -> Self {
        self.values.insert(key.into(), value);
        self
    }

    pub fn insert(&mut self, key: impl Into<String>, value: Value) {
        self.values.insert(key.into(), value);
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }

    /// A copy of `self` with `overrides` applied on top.
    pub fn merged(&self, overrides: &DataContext) -> DataContext {
        let mut merged = self.clone();
        for (key, value) in &overrides.values {
            merged.values.insert(key.clone(), value.clone());
        }
        merged
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Resolves `{Binding key}` to its display text.
    ///
    /// Both whole-value bindings (`"{Binding title}"`) and inline interpolation
    /// (`"3 of {Binding total}"`) work, so labels can mix text and data.
    pub fn resolve_text(&self, raw: &str) -> String {
        if !raw.contains('{') {
            return raw.to_string();
        }
        let mut out = String::with_capacity(raw.len());
        let mut rest = raw;
        while let Some(start) = rest.find('{') {
            out.push_str(&rest[..start]);
            let tail = &rest[start..];
            let Some(end) = tail.find('}') else {
                out.push_str(tail);
                return out;
            };
            let token = &tail[..=end];
            match binding_key(token) {
                Some(key) => out.push_str(
                    self.get(key).map(Value::to_display_text).unwrap_or_default().as_str(),
                ),
                None => out.push_str(token),
            }
            rest = &tail[end + 1..];
        }
        out.push_str(rest);
        out
    }

    pub fn resolve_number(&self, raw: &str) -> Option<f64> {
        match binding_key(raw) {
            Some(key) => self.get(key).and_then(Value::number),
            None => raw.trim().parse::<f64>().ok(),
        }
    }

    pub fn resolve_bool(&self, raw: &str) -> Option<bool> {
        match binding_key(raw) {
            Some(key) => self.get(key).and_then(Value::boolean),
            None => {
                let value = raw.trim();
                Some(value.eq_ignore_ascii_case("true") || value == "1")
            }
        }
    }

    pub fn resolve_color(&self, raw: &str) -> Option<Color> {
        match binding_key(raw) {
            Some(key) => self.get(key).and_then(Value::color),
            None => crate::xaml::parse_color(raw, 0).ok(),
        }
    }
}

/// Extracts `key` from `"{Binding key}"`.
pub fn binding_key(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    let inner = trimmed.strip_prefix('{')?.strip_suffix('}')?.trim();
    let inner = inner.strip_prefix("Binding")?.trim();
    if inner.is_empty() {
        None
    } else {
        Some(inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> DataContext {
        DataContext::new()
            .set("title", Value::Text("Hello".into()))
            .set("width", Value::Number(120.0))
            .set("flag", Value::Bool(true))
            .set("accent", Value::Color(Color::rgb(1, 2, 3)))
    }

    #[test]
    fn detects_binding_syntax() {
        assert_eq!(binding_key("{Binding title}"), Some("title"));
        assert_eq!(binding_key("{ Binding title }"), Some("title"));
        assert_eq!(binding_key("plain"), None);
        assert_eq!(binding_key("{}"), None);
    }

    #[test]
    fn resolves_text_bindings_and_leaves_literals() {
        let ctx = context();
        assert_eq!(ctx.resolve_text("{Binding title}"), "Hello");
        assert_eq!(ctx.resolve_text("literal"), "literal");
        assert_eq!(ctx.resolve_text("{Binding missing}"), "");
    }

    #[test]
    fn resolves_inline_bindings_inside_a_sentence() {
        let ctx = context();
        assert_eq!(ctx.resolve_text("Title: {Binding title}"), "Title: Hello");
        assert_eq!(ctx.resolve_text("{Binding title} / {Binding width}"), "Hello / 120");
        assert_eq!(ctx.resolve_text("no bindings here"), "no bindings here");
        assert_eq!(ctx.resolve_text("brace { not a binding }"), "brace { not a binding }");
        assert_eq!(ctx.resolve_text("unterminated {Binding title"), "unterminated {Binding title");
    }

    #[test]
    fn resolves_typed_values_without_string_round_trip() {
        let ctx = context();
        assert_eq!(ctx.resolve_number("{Binding width}"), Some(120.0));
        assert_eq!(ctx.resolve_number("42"), Some(42.0));
        assert_eq!(ctx.resolve_bool("{Binding flag}"), Some(true));
        assert_eq!(ctx.resolve_color("{Binding accent}"), Some(Color::rgb(1, 2, 3)));
        assert_eq!(ctx.resolve_color("#0A0B0C"), Some(Color::rgb(10, 11, 12)));
    }

    #[test]
    fn numbers_render_without_trailing_zero() {
        assert_eq!(Value::Number(12.0).to_display_text(), "12");
        assert_eq!(Value::Number(12.5).to_display_text(), "12.5");
    }

    #[test]
    fn colours_render_as_hex() {
        assert_eq!(Value::Color(Color::rgb(0x2F, 0x6F, 0xD0)).to_display_text(), "#2F6FD0");
    }
}
