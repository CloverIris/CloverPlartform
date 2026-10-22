//! Stylesheets: reusable property sets matched by element type or name.
//!
//! This is the smallest layer that makes theming possible. It is deliberately not
//! a full CSS engine: three selector forms, ordered application, and attribute
//! override. Styles provide *defaults*; anything written on the element wins.
//!
//! ```ignore
//! let sheet = xaml::parse_stylesheet(r#"
//! <Styles>
//!   <Style TargetType="Button" Background="#383E46" Foreground="#F2F5F7" Padding="14,8"/>
//!   <Style x:Name="title" FontSize="20" Foreground="#FFFFFF"/>
//! </Styles>"#)?;
//! let page = xaml::parse_with_styles(page_xaml, &NoImages, &sheet)?;
//! ```

use clover_core::{Color, Edges};

use crate::style::Length;

/// Which nodes a rule applies to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selector {
    /// Every element.
    All,
    /// Every element with this XAML name, e.g. `Button`.
    Type(String),
    /// The element carrying this `x:Name`.
    Name(String),
}

impl Selector {
    /// Higher specificity wins when several rules match.
    pub fn specificity(&self) -> u8 {
        match self {
            Selector::All => 0,
            Selector::Type(_) => 1,
            Selector::Name(_) => 2,
        }
    }

    pub fn matches(&self, type_name: &str, name: Option<&str>) -> bool {
        match self {
            Selector::All => true,
            Selector::Type(expected) => expected.eq_ignore_ascii_case(type_name),
            Selector::Name(expected) => name.map(|n| n == expected).unwrap_or(false),
        }
    }
}

/// Properties a rule can set. `None` means "leave it alone".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StyleProps {
    pub background: Option<Color>,
    pub foreground: Option<Color>,
    pub font_size: Option<f32>,
    pub padding: Option<Edges>,
    pub margin: Option<Edges>,
    pub radius: Option<f32>,
    pub border_width: Option<f32>,
    pub border_color: Option<Color>,
    pub width: Option<Length>,
    pub height: Option<Length>,
    pub wrap: Option<bool>,
}

/// One selector plus the properties it assigns.
#[derive(Clone, Debug)]
pub struct StyleRule {
    pub selector: Selector,
    pub props: StyleProps,
}

/// An ordered collection of rules.
#[derive(Clone, Debug, Default)]
pub struct StyleSheet {
    rules: Vec<StyleRule>,
}

impl StyleSheet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, rule: StyleRule) {
        self.rules.push(rule);
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Matching rules, lowest specificity first, so later applications win.
    pub fn resolve(&self, type_name: &str, name: Option<&str>) -> Vec<&StyleProps> {
        let mut matched: Vec<(u8, usize, &StyleProps)> = self
            .rules
            .iter()
            .enumerate()
            .filter(|(_, rule)| rule.selector.matches(type_name, name))
            .map(|(index, rule)| (rule.selector.specificity(), index, &rule.props))
            .collect();
        matched.sort_by_key(|(specificity, index, _)| (*specificity, *index));
        matched.into_iter().map(|(_, _, props)| props).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(selector: Selector, props: StyleProps) -> StyleRule {
        StyleRule { selector, props }
    }

    #[test]
    fn type_selector_matches_case_insensitively() {
        assert!(Selector::Type("Button".into()).matches("button", None));
        assert!(!Selector::Type("Button".into()).matches("TextBlock", None));
    }

    #[test]
    fn name_selector_needs_the_name() {
        let selector = Selector::Name("title".into());
        assert!(selector.matches("TextBlock", Some("title")));
        assert!(!selector.matches("TextBlock", Some("other")));
        assert!(!selector.matches("TextBlock", None));
    }

    #[test]
    fn name_beats_type_regardless_of_order() {
        let mut sheet = StyleSheet::new();
        sheet.push(rule(Selector::Name("btn".into()), StyleProps { font_size: Some(20.0), ..Default::default() }));
        sheet.push(rule(Selector::Type("Button".into()), StyleProps { font_size: Some(12.0), ..Default::default() }));

        let resolved = sheet.resolve("Button", Some("btn"));
        assert_eq!(resolved.len(), 2);
        // Type first (specificity 1), name last (specificity 2).
        assert_eq!(resolved[0].font_size, Some(12.0));
        assert_eq!(resolved[1].font_size, Some(20.0));
    }

    #[test]
    fn equal_specificity_keeps_document_order() {
        let mut sheet = StyleSheet::new();
        sheet.push(rule(Selector::Type("Panel".into()), StyleProps { radius: Some(2.0), ..Default::default() }));
        sheet.push(rule(Selector::Type("Panel".into()), StyleProps { radius: Some(8.0), ..Default::default() }));
        let resolved = sheet.resolve("Panel", None);
        assert_eq!(resolved[0].radius, Some(2.0));
        assert_eq!(resolved[1].radius, Some(8.0), "later rule wins");
    }

    #[test]
    fn all_matches_everything() {
        let mut sheet = StyleSheet::new();
        sheet.push(rule(Selector::All, StyleProps { foreground: Some(Color::BLACK), ..Default::default() }));
        assert_eq!(sheet.resolve("Anything", None).len(), 1);
    }

    #[test]
    fn unmatched_yields_nothing() {
        let mut sheet = StyleSheet::new();
        sheet.push(rule(Selector::Type("Button".into()), StyleProps::default()));
        assert!(sheet.resolve("Panel", None).is_empty());
    }
}
