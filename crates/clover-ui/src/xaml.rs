//! XAML front-end (WPF / Avalonia / WinUI / UWP flavoured).
//!
//! XAML is a *front-end*, not a second UI model: parsing produces the same
//! [`Node`] tree the declarative builders produce, so a XAML page can host
//! declaratively-built children and vice versa.
//!
//! ## Supported today
//!
//! | Element | Notes |
//! |---|---|
//! | `StackPanel` | `Orientation`, `Spacing` |
//! | `Grid` | uniform grid via `Columns` (rows derived from child count) |
//! | `Canvas` | children positioned with `Canvas.Left` / `Canvas.Top` |
//! | `Border` | `Background`, `CornerRadius`, `BorderThickness`, `BorderBrush` |
//! | `Panel` | plain container |
//! | `TextBlock` | `Text` attribute or inner text |
//! | `Button` | `Content` |
//! | `Image` | `Source` resolved through an [`ImageResolver`] |
//! | `ScrollViewer` | vertical scrolling container |
//! | `CloverSurface` | `Id`, a direct-memory child surface |
//!
//! `Command="id"` on any element makes it actionable: clicking it, or pressing
//! Enter/Space while it has focus, reports `id` to the host.
//!
//! ## Not yet
//!
//! Styles, templates, `{Binding}`, property elements (`<Grid.RowDefinitions>`),
//! triggers, and attached properties other than `Canvas.*`. Those are the next
//! layers; the parser already keeps attributes verbatim so they can be added
//! without changing the node model.

use std::fmt;

use clover_core::{Align, Color, Edges, Orientation};

use crate::node::{self, ImageHandle, Node, SurfaceHandle};
use crate::style::{Fit, Length};

/// Errors produced while parsing XAML.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XamlError {
    pub message: String,
    pub offset: usize,
}

impl fmt::Display for XamlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "xaml error at {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for XamlError {}

fn err<T>(offset: usize, message: impl Into<String>) -> Result<T, XamlError> {
    Err(XamlError { message: message.into(), offset })
}

/// Resolves an `Image Source` name to a registered image handle.
pub trait ImageResolver {
    fn resolve(&self, source: &str) -> Option<ImageHandle>;
}

/// An [`ImageResolver`] that resolves nothing, for trees without images.
pub struct NoImages;

impl ImageResolver for NoImages {
    fn resolve(&self, _source: &str) -> Option<ImageHandle> {
        None
    }
}

impl<F> ImageResolver for F
where
    F: Fn(&str) -> Option<ImageHandle>,
{
    fn resolve(&self, source: &str) -> Option<ImageHandle> {
        self(source)
    }
}

/// Parses a XAML fragment into a node tree.
pub fn parse(source: &str, images: &dyn ImageResolver) -> Result<Node, XamlError> {
    parse_with_styles(source, images, &crate::style_sheet::StyleSheet::new())
}

/// Parses a XAML fragment applying `sheet` first; element attributes override it.
pub fn parse_with_styles(
    source: &str,
    images: &dyn ImageResolver,
    sheet: &crate::style_sheet::StyleSheet,
) -> Result<Node, XamlError> {
    parse_with_data(source, images, sheet, &crate::binding::DataContext::new())
}

/// Parses with both a stylesheet and a data context for `{Binding key}`.
pub fn parse_with_data(
    source: &str,
    images: &dyn ImageResolver,
    sheet: &crate::style_sheet::StyleSheet,
    data: &crate::binding::DataContext,
) -> Result<Node, XamlError> {
    let document = parse_xml(source)?;
    let context = BuildContext { images, sheet, data };
    build(&document, &context, InheritedStyle::default())
}

/// Everything a build pass needs that is not on the element itself.
#[derive(Clone, Copy)]
struct BuildContext<'a> {
    images: &'a dyn ImageResolver,
    sheet: &'a crate::style_sheet::StyleSheet,
    data: &'a crate::binding::DataContext,
}

/// Inherited properties: text styling cascades to children, everything else does
/// not. A child that sets `FontSize` overrides it for its own subtree.
#[derive(Clone, Copy, Debug)]
struct InheritedStyle {
    font_size: f32,
    text_color: clover_core::Color,
}

impl Default for InheritedStyle {
    fn default() -> Self {
        let style = crate::style::Style::default();
        Self { font_size: style.font_size, text_color: style.text_color }
    }
}

/// Parses a `<Styles>` document into a [`crate::style_sheet::StyleSheet`].
pub fn parse_stylesheet(source: &str) -> Result<crate::style_sheet::StyleSheet, XamlError> {
    let document = parse_xml(source)?;
    if !document.name.eq_ignore_ascii_case("Styles") {
        return err(document.offset, "a stylesheet document must start with <Styles>");
    }
    parse_styles_node(&document)
}

/// Parses the `<Style>` children of a `<Styles>` element.
fn parse_styles_node(node: &XmlNode) -> Result<crate::style_sheet::StyleSheet, XamlError> {
    use crate::style_sheet::{Selector, StyleProps, StyleRule, StyleSheet};

    let mut sheet = StyleSheet::new();
    for child in &node.children {
        if !child.name.eq_ignore_ascii_case("Style") {
            return err(child.offset, format!("unsupported element <{}> in <Styles>", child.name));
        }
        let selector = if let Some(name) = child.attr("x:Name").or_else(|| child.attr("Name")) {
            Selector::Name(name.to_string())
        } else if let Some(class) = child.attr("Class").or_else(|| child.attr("TargetClass")) {
            Selector::Class(class.trim().to_string())
        } else if let Some(target) = child.attr("TargetType") {
            Selector::Type(target.to_string())
        } else {
            Selector::All
        };

        let mut props = StyleProps::default();
        if let Some(v) = child.attr("Background") {
            props.background = Some(parse_color(v, child.offset)?);
        }
        if let Some(v) = child.attr("Foreground") {
            props.foreground = Some(parse_color(v, child.offset)?);
        }
        if let Some(v) = child.attr_f32("FontSize") {
            props.font_size = Some(v);
        }
        if let Some(v) = child.attr("Padding") {
            props.padding = Some(parse_edges(v, child.offset, "Padding")?);
        }
        if let Some(v) = child.attr("Margin") {
            props.margin = Some(parse_edges(v, child.offset, "Margin")?);
        }
        if let Some(v) = child.attr_f32("CornerRadius") {
            props.radius = Some(v);
        }
        if let Some(v) = child.attr_f32("BorderThickness") {
            props.border_width = Some(v);
        }
        if let Some(v) = child.attr("BorderBrush") {
            props.border_color = Some(parse_color(v, child.offset)?);
        }
        if let Some(v) = child.attr("Width") {
            props.width = Some(
                crate::style::Length::parse(v)
                    .ok_or_else(|| XamlError { message: format!("bad Width '{v}'"), offset: child.offset })?,
            );
        }
        if let Some(v) = child.attr("Height") {
            props.height = Some(
                crate::style::Length::parse(v)
                    .ok_or_else(|| XamlError { message: format!("bad Height '{v}'"), offset: child.offset })?,
            );
        }
        if let Some(v) = child.attr("TextWrapping") {
            props.wrap = Some(v.trim().eq_ignore_ascii_case("wrap"));
        }

        sheet.push(StyleRule { selector, props });
    }
    Ok(sheet)
}

/// A `.cui` UI document: data, styles and one page root.
///
/// ```xml
/// <Ui>
///   <Data>
///     <Value Key="title" Text="Settings"/>
///     <Value Key="accent" Color="#2F6FD0"/>
///   </Data>
///   <Styles>
///     <Style TargetType="Button" Background="#2E6FD0" CornerRadius="6"/>
///   </Styles>
///   <StackPanel><TextBlock Text="{Binding title}"/></StackPanel>
/// </Ui>
/// ```
///
/// The document is parsed once and rebuilt on demand, which is when bindings are
/// resolved. Nothing observes the data: the host rebuilds when it changes.
#[derive(Clone, Debug)]
pub struct UiDocument {
    pub data: crate::binding::DataContext,
    pub sheet: crate::style_sheet::StyleSheet,
    root: XmlNode,
    source: String,
}

impl UiDocument {
    /// Parses a `.cui` document.
    pub fn parse(source: &str) -> Result<Self, XamlError> {
        let document = parse_xml(source)?;
        if !document.name.eq_ignore_ascii_case("Ui") {
            return err(document.offset, "a .cui document must start with <Ui>");
        }

        let mut data = crate::binding::DataContext::new();
        let mut sheet = crate::style_sheet::StyleSheet::new();
        let mut root: Option<XmlNode> = None;

        for child in &document.children {
            let name = child.name.to_ascii_lowercase();
            if name == "data" {
                for entry in &child.children {
                    if !entry.name.eq_ignore_ascii_case("Value") {
                        return err(entry.offset, "expected <Value> inside <Data>");
                    }
                    let Some(key) = entry.attr("Key") else {
                        return err(entry.offset, "<Value> needs a Key");
                    };
                    let value = if let Some(text) = entry.attr("Text") {
                        crate::binding::Value::Text(text.to_string())
                    } else if let Some(number) = entry.attr("Number") {
                        let parsed = number.trim().parse::<f64>().map_err(|_| XamlError {
                            message: format!("bad Number '{number}'"),
                            offset: entry.offset,
                        })?;
                        crate::binding::Value::Number(parsed)
                    } else if let Some(flag) = entry.attr("Bool") {
                        crate::binding::Value::Bool(
                            flag.trim().eq_ignore_ascii_case("true") || flag.trim() == "1",
                        )
                    } else if let Some(color) = entry.attr("Color") {
                        crate::binding::Value::Color(parse_color(color, entry.offset)?)
                    } else {
                        return err(entry.offset, "<Value> needs Text, Number, Bool or Color");
                    };
                    data.insert(key, value);
                }
            } else if name == "styles" {
                sheet = parse_styles_node(child)?;
            } else {
                if root.is_some() {
                    return err(child.offset, "a .cui document has exactly one page root");
                }
                root = Some(child.clone());
            }
        }

        let Some(root) = root else {
            return err(document.offset, "a .cui document needs a page root");
        };
        Ok(Self { data, sheet, root, source: source.to_string() })
    }

    /// Reads and parses a `.cui` file.
    pub fn from_path(path: impl AsRef<std::path::Path>) -> Result<Self, String> {
        let source = std::fs::read_to_string(path.as_ref())
            .map_err(|error| format!("read .cui failed: {error}"))?;
        Self::parse(&source).map_err(|error| error.to_string())
    }

    /// The original source, so hosts can keep it around.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Builds the page using the document's own data.
    pub fn build(&self, images: &dyn ImageResolver) -> Result<Node, XamlError> {
        self.build_with(images, &crate::binding::DataContext::new())
    }

    /// Builds the page, with `overrides` layered over the document's data.
    pub fn build_with(
        &self,
        images: &dyn ImageResolver,
        overrides: &crate::binding::DataContext,
    ) -> Result<Node, XamlError> {
        let data = self.data.merged(overrides);
        let context = BuildContext { images, sheet: &self.sheet, data: &data };
        build(&self.root, &context, InheritedStyle::default())
    }
}

// ---- XML ------------------------------------------------------------------

#[derive(Clone, Debug)]
struct XmlNode {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<XmlNode>,
    text: String,
    offset: usize,
}

impl XmlNode {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn attr_f32(&self, name: &str) -> Option<f32> {
        self.attr(name).and_then(|v| v.trim().parse::<f32>().ok())
    }

    fn attr_bool(&self, name: &str) -> Option<bool> {
        self.attr(name).map(|v| {
            let v = v.trim();
            v.eq_ignore_ascii_case("true") || v == "1" || v.eq_ignore_ascii_case("yes")
        })
    }
}

fn parse_xml(source: &str) -> Result<XmlNode, XamlError> {
    let bytes = source.as_bytes();
    let mut index = 0usize;
    let mut roots: Vec<XmlNode> = Vec::new();
    let mut stack: Vec<XmlNode> = Vec::new();

    while index < bytes.len() {
        if bytes[index] != b'<' {
            let start = index;
            while index < bytes.len() && bytes[index] != b'<' {
                index += 1;
            }
            let text = decode_entities(source[start..index].trim());
            if !text.is_empty() {
                if let Some(current) = stack.last_mut() {
                    current.text.push_str(&text);
                }
            }
            continue;
        }

        // Comments, declarations, doctypes.
        if source[index..].starts_with("<!--") {
            match source[index..].find("-->") {
                Some(rel) => index += rel + 3,
                None => return err(index, "unterminated comment"),
            }
            continue;
        }
        if source[index..].starts_with("<?") {
            match source[index..].find("?>") {
                Some(rel) => index += rel + 2,
                None => return err(index, "unterminated declaration"),
            }
            continue;
        }
        if source[index..].starts_with("<!") {
            match source[index..].find('>') {
                Some(rel) => index += rel + 1,
                None => return err(index, "unterminated directive"),
            }
            continue;
        }

        if source[index..].starts_with("</") {
            let close_start = index;
            match source[index..].find('>') {
                Some(rel) => index += rel + 1,
                None => return err(close_start, "unterminated closing tag"),
            }
            let name = source[close_start + 2..index - 1].trim().to_string();
            let Some(open) = stack.pop() else {
                return err(close_start, format!("unexpected closing tag </{name}>"));
            };
            if !open.name.eq_ignore_ascii_case(&name) {
                return err(close_start, format!("</{name}> closes <{}>", open.name));
            }
            push_node(&mut stack, &mut roots, open);
            continue;
        }

        // Opening tag.
        let tag_start = index;
        index += 1;
        let name_start = index;
        while index < bytes.len() && !is_name_end(bytes[index]) {
            index += 1;
        }
        if index == name_start {
            return err(tag_start, "empty element name");
        }
        let name = source[name_start..index].trim().to_string();
        let mut attrs = Vec::new();
        let self_closing;
        loop {
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            if index >= bytes.len() {
                return err(tag_start, "unterminated tag");
            }
            if source[index..].starts_with("/>") {
                index += 2;
                self_closing = true;
                break;
            }
            if bytes[index] == b'>' {
                index += 1;
                self_closing = false;
                break;
            }
            let attr_start = index;
            while index < bytes.len() && !is_attr_name_end(bytes[index]) {
                index += 1;
            }
            let attr_name = source[attr_start..index].trim().to_string();
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            if index >= bytes.len() || bytes[index] != b'=' {
                return err(attr_start, format!("attribute {attr_name} is missing '='"));
            }
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            if index >= bytes.len() || (bytes[index] != b'"' && bytes[index] != b'\'') {
                return err(attr_start, format!("attribute {attr_name} value must be quoted"));
            }
            let quote = bytes[index];
            index += 1;
            let value_start = index;
            while index < bytes.len() && bytes[index] != quote {
                index += 1;
            }
            if index >= bytes.len() {
                return err(attr_start, format!("unterminated value for {attr_name}"));
            }
            let value = decode_entities(&source[value_start..index]);
            index += 1;
            attrs.push((attr_name, value));
        }

        let element = XmlNode { name, attrs, children: Vec::new(), text: String::new(), offset: tag_start };
        if self_closing {
            push_node(&mut stack, &mut roots, element);
        } else {
            stack.push(element);
        }
    }

    if let Some(open) = stack.pop() {
        return err(open.offset, format!("unclosed <{}>", open.name));
    }
    match roots.len() {
        0 => err(0, "document has no root element"),
        1 => Ok(roots.remove(0)),
        _ => err(0, "document has multiple root elements"),
    }
}

fn push_node(stack: &mut Vec<XmlNode>, roots: &mut Vec<XmlNode>, node: XmlNode) {
    match stack.last_mut() {
        Some(parent) => parent.children.push(node),
        None => roots.push(node),
    }
}

fn is_name_end(byte: u8) -> bool {
    byte.is_ascii_whitespace() || byte == b'>' || byte == b'/'
}

fn is_attr_name_end(byte: u8) -> bool {
    byte.is_ascii_whitespace() || byte == b'=' || byte == b'>' || byte == b'/'
}

fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        let (decoded, consumed) = match tail.get(1..) {
            Some(after) if after.starts_with("amp;") => ("&", 5),
            Some(after) if after.starts_with("lt;") => ("<", 4),
            Some(after) if after.starts_with("gt;") => (">", 4),
            Some(after) if after.starts_with("quot;") => ("\"", 6),
            Some(after) if after.starts_with("apos;") => ("'", 6),
            _ => ("&", 1),
        };
        out.push_str(decoded);
        rest = &tail[consumed..];
    }
    out.push_str(rest);
    out
}

fn build(
    xml: &XmlNode,
    context: &BuildContext<'_>,
    inherited: InheritedStyle,
) -> Result<Node, XamlError> {
    // Containers that synthesise their own children (Grid) must not also have the
    // raw XML children appended a second time.
    let mut consumes_children = false;
    let mut node = match xml.name.to_ascii_lowercase().as_str() {
        "stackpanel" => {
            let orientation = match xml.attr("Orientation").map(|v| v.trim().to_ascii_lowercase()) {
                Some(v) if v == "horizontal" || v == "h" => Orientation::Horizontal,
                _ => Orientation::Vertical,
            };
            node::stack(orientation)
        }
        "grid" => {
            consumes_children = true;
            build_grid(xml, context, inherited)?
        }
        "canvas" => node::stack(Orientation::Vertical).align(Align::Start).justify(Align::Start),
        "border" => node::panel(),
        "scrollviewer" => node::column().scrollable(),
        "panel" | "stack" => node::panel(),
        "textblock" | "text" | "label" => {
            let raw = xml.attr("Text").map(str::to_string).unwrap_or_else(|| xml.text.clone());
            node::text(context.data.resolve_text(&raw))
        }
        "button" => {
            let raw = xml.attr("Content").map(str::to_string).unwrap_or_else(|| xml.text.clone());
            node::button(context.data.resolve_text(&raw))
        }
        "image" => {
            let source = xml.attr("Source").unwrap_or_default();
            match context.images.resolve(&context.data.resolve_text(source)) {
                Some(handle) => {
                    let fit = match xml.attr("Stretch").map(|v| v.trim().to_ascii_lowercase()).as_deref() {
                        Some("uniform") => Fit::Contain,
                        Some("uniformtofill") => Fit::Cover,
                        _ => Fit::Fill,
                    };
                    let mut n = node::image(handle);
                    n.view = node::View::Image { handle, fit };
                    n
                }
                None => return err(xml.offset, format!("image source '{source}' was not registered")),
            }
        }
        "cloversurface" | "surface" => {
            let id = xml
                .attr("Id")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(0);
            node::direct_surface(SurfaceHandle(id))
        }
        other => return err(xml.offset, format!("unsupported element <{other}>")),
    };

    // Styles are defaults; element attributes override them; text properties
    // cascade into children.
    let mut ctx = inherited;
    let classes: Vec<String> = xml
        .attr("Class")
        .map(|raw| raw.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    node.classes = classes.clone();
    let element_name = xml.attr("x:Name").or_else(|| xml.attr("Name"));
    for props in context.sheet.resolve(&xml.name, element_name, &classes) {
        apply_style_props(&mut node, props);
        if let Some(v) = props.font_size {
            ctx.font_size = v;
        }
        if let Some(v) = props.foreground {
            ctx.text_color = v;
        }
    }

    apply_common_attributes(&mut node, xml, context)?;
    if let Some(v) = xml.attr_f32("FontSize") {
        ctx.font_size = v;
    }
    if let Some(v) = xml.attr("Foreground") {
        ctx.text_color = parse_color(v, xml.offset)?;
    }
    node.style.font_size = ctx.font_size;
    node.style.text_color = ctx.text_color;

    if !consumes_children {
        for child in &xml.children {
            let child_node = build(child, context, ctx)?;
            node.children.push(child_node);
        }
    }

    Ok(node)
}

fn apply_style_props(node: &mut Node, props: &crate::style_sheet::StyleProps) {
    if let Some(v) = props.background {
        node.style.background = v;
    }
    if let Some(v) = props.foreground {
        node.style.text_color = v;
    }
    if let Some(v) = props.font_size {
        node.style.font_size = v;
    }
    if let Some(v) = props.padding {
        node.style.padding = v;
    }
    if let Some(v) = props.margin {
        node.style.margin = v;
    }
    if let Some(v) = props.radius {
        node.style.radius = v;
    }
    if let Some(v) = props.border_width {
        node.style.border_width = v;
    }
    if let Some(v) = props.border_color {
        node.style.border_color = v;
    }
    if let Some(v) = props.width {
        node.style.width = v;
    }
    if let Some(v) = props.height {
        node.style.height = v;
    }
    if let Some(v) = props.wrap {
        node.style.wrap = v;
    }
}

fn build_grid(
    xml: &XmlNode,
    context: &BuildContext<'_>,
    inherited: InheritedStyle,
) -> Result<Node, CloverGridError> {
    // A uniform grid expressed with the existing stack layout: one row per
    // `Columns` children, each cell taking an equal percentage of the width.
    let columns = xml.attr("Columns").and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(1).max(1);
    let mut grid = node::column().align(Align::Stretch);
    let gap = xml.attr("Spacing").and_then(|v| v.trim().parse::<f32>().ok()).unwrap_or(0.0);
    grid.style.gap = gap;

    let mut row = node::row().align(Align::Stretch).gap(gap);
    for (index, child) in xml.children.iter().enumerate() {
        let mut cell = build(child, context, inherited)?;
        if cell.style.width == Length::Auto {
            cell.style.width = Length::Percent(1.0 / columns as f32);
        }
        row.children.push(cell);
        if (index + 1) % columns == 0 {
            grid.children.push(std::mem::replace(
                &mut row,
                node::row().align(Align::Stretch).gap(gap),
            ));
        }
    }
    if !row.children.is_empty() {
        grid.children.push(row);
    }
    Ok(grid)
}

// `build_grid` is called from `build`, which returns `Result<Node, XamlError>`;
// keeping the alias makes the intent obvious at the call site.
type CloverGridError = XamlError;

fn apply_common_attributes(
    node: &mut Node,
    xml: &XmlNode,
    context: &BuildContext<'_>,
) -> Result<(), XamlError> {
    if let Some(name) = xml.attr("x:Name").or_else(|| xml.attr("Name")) {
        node.name = Some(name.to_string());
    }
    if let Some(key) = xml.attr("Key") {
        node.key = Some(context.data.resolve_text(key));
    }
    if let Some(command) = xml.attr("Command") {
        node.action = Some(context.data.resolve_text(command));
    }

    for (attr, target) in [
        ("Width", AttrTarget::Width),
        ("Height", AttrTarget::Height),
    ] {
        if let Some(value) = xml.attr(attr) {
            let resolved = context.data.resolve_text(value);
            let length = Length::parse(&resolved)
                .ok_or_else(|| XamlError { message: format!("bad {attr} '{value}'"), offset: xml.offset })?;
            match target {
                AttrTarget::Width => node.style.width = length,
                AttrTarget::Height => node.style.height = length,
            }
        }
    }

    if let Some(v) = xml.attr_f32("MinWidth") {
        node.style.min_width = Some(v);
    }
    if let Some(v) = xml.attr_f32("MaxWidth") {
        node.style.max_width = Some(v);
    }
    if let Some(v) = xml.attr_f32("MinHeight") {
        node.style.min_height = Some(v);
    }
    if let Some(v) = xml.attr_f32("MaxHeight") {
        node.style.max_height = Some(v);
    }
    if let Some(v) = xml.attr("Margin") {
        node.style.margin = parse_edges(v, xml.offset, "Margin")?;
    }
    if let Some(v) = xml.attr("Padding") {
        node.style.padding = parse_edges(v, xml.offset, "Padding")?;
    }
    if let Some(v) = xml.attr("Spacing").and_then(|v| v.trim().parse::<f32>().ok()) {
        node.style.gap = v;
    }
    if let Some(v) = xml.attr("Background") {
        node.style.background = context
            .data
            .resolve_color(v)
            .ok_or_else(|| XamlError { message: format!("bad Background '{v}'"), offset: xml.offset })?;
    }
    if let Some(v) = xml.attr("Foreground") {
        node.style.text_color = context
            .data
            .resolve_color(v)
            .ok_or_else(|| XamlError { message: format!("bad Foreground '{v}'"), offset: xml.offset })?;
    }
    if let Some(v) = xml.attr("FontSize").and_then(|v| context.data.resolve_number(v)) {
        node.style.font_size = v as f32;
    }
    if let Some(v) = xml.attr_f32("CornerRadius") {
        node.style.radius = v;
    }
    if let Some(v) = xml.attr_f32("BorderThickness") {
        node.style.border_width = v;
    }
    if let Some(v) = xml.attr("BorderBrush") {
        node.style.border_color = context
            .data
            .resolve_color(v)
            .ok_or_else(|| XamlError { message: format!("bad BorderBrush '{v}'"), offset: xml.offset })?;
    }
    if let Some(v) = xml.attr_f32("Opacity") {
        node.style.opacity = v.clamp(0.0, 1.0);
    }
    if let Some(v) = xml.attr_bool("Clip") {
        node.style.clip = v;
    }
    if let Some(v) = xml.attr("TextWrapping") {
        node.style.wrap = v.trim().eq_ignore_ascii_case("wrap")
            || v.trim().eq_ignore_ascii_case("wrapwithoverflow");
    }
    if let Some(v) = xml.attr_bool("Focusable") {
        node.focusable = v;
    }
    if let Some(v) = xml.attr("Visible").and_then(|v| context.data.resolve_bool(v)) {
        node.visible = v;
    }
    if let Some(v) = xml.attr_bool("IsEnabled") {
        node.state.disabled = !v;
    }
    if let Some(v) = xml.attr("HorizontalAlignment") {
        node.style.align = parse_align(v);
    }
    if let Some(v) = xml.attr("VerticalAlignment") {
        node.style.justify = parse_align(v);
    }
    if let (Some(left), Some(top)) = (xml.attr_f32("Canvas.Left"), xml.attr_f32("Canvas.Top")) {
        node.style.margin = Edges::new(left, top, 0.0, 0.0);
    }

    Ok(())
}

enum AttrTarget {
    Width,
    Height,
}

fn parse_align(value: &str) -> Align {
    match value.trim().to_ascii_lowercase().as_str() {
        "center" => Align::Center,
        "right" | "end" | "bottom" => Align::End,
        "stretch" => Align::Stretch,
        _ => Align::Start,
    }
}

fn parse_edges(value: &str, offset: usize, name: &str) -> Result<Edges, XamlError> {
    let parts: Vec<f32> = value
        .split(',')
        .filter_map(|p| p.trim().parse::<f32>().ok())
        .collect();
    match parts.as_slice() {
        [all] => Ok(Edges::all(*all)),
        [horizontal, vertical] => Ok(Edges::symmetric(*horizontal, *vertical)),
        [left, top, right, bottom] => Ok(Edges::new(*left, *top, *right, *bottom)),
        _ => err(offset, format!("{name} needs 1, 2 or 4 numbers, got '{value}'")),
    }
}

/// Parses WPF-style colours: `#RGB`, `#RRGGBB`, `#AARRGGBB`, or a name.
pub fn parse_color(value: &str, offset: usize) -> Result<Color, XamlError> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        let parse = |s: &str| u8::from_str_radix(s, 16).ok();
        return match hex.len() {
            3 => {
                let (r, g, b) = match (parse(&hex[0..1]), parse(&hex[1..2]), parse(&hex[2..3])) {
                    (Some(r), Some(g), Some(b)) => (r, g, b),
                    _ => return err(offset, format!("bad colour '{value}'")),
                };
                Ok(Color::rgba(r * 17, g * 17, b * 17, 255))
            }
            6 => {
                let (r, g, b) = match (parse(&hex[0..2]), parse(&hex[2..4]), parse(&hex[4..6])) {
                    (Some(r), Some(g), Some(b)) => (r, g, b),
                    _ => return err(offset, format!("bad colour '{value}'")),
                };
                Ok(Color::rgb(r, g, b))
            }
            8 => {
                let (a, r, g, b) = match (
                    parse(&hex[0..2]),
                    parse(&hex[2..4]),
                    parse(&hex[4..6]),
                    parse(&hex[6..8]),
                ) {
                    (Some(a), Some(r), Some(g), Some(b)) => (a, r, g, b),
                    _ => return err(offset, format!("bad colour '{value}'")),
                };
                Ok(Color::rgba(r, g, b, a))
            }
            _ => err(offset, format!("bad colour '{value}'")),
        };
    }
    match value.to_ascii_lowercase().as_str() {
        "transparent" => Ok(Color::TRANSPARENT),
        "black" => Ok(Color::BLACK),
        "white" => Ok(Color::WHITE),
        _ => err(offset, format!("unsupported colour '{value}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stack_panel_with_children() {
        let xaml = r#"<StackPanel Orientation="Horizontal" Spacing="8" Padding="4">
            <TextBlock Text="A" FontSize="18"/>
            <Button Content="B"/>
        </StackPanel>"#;
        let root = parse(xaml, &NoImages).unwrap();
        assert_eq!(root.children.len(), 2);
        assert!(root.style.direction.is_horizontal());
        assert_eq!(root.style.gap, 8.0);
        assert_eq!(root.style.padding, Edges::all(4.0));
        assert_eq!(root.children[0].view, node::View::Text("A".into()));
        assert_eq!(root.children[0].style.font_size, 18.0);
        assert_eq!(root.children[1].view, node::View::Text("B".into()));
    }

    #[test]
    fn parses_inner_text_when_attribute_missing() {
        let root = parse("<TextBlock>Hello &amp; bye</TextBlock>", &NoImages).unwrap();
        assert_eq!(root.view, node::View::Text("Hello & bye".into()));
    }

    #[test]
    fn parses_command_actions() {
        let root = parse(r#"<Button Content="OK" Command="new-canvas"/>"#, &NoImages).unwrap();
        assert_eq!(root.action.as_deref(), Some("new-canvas"));
        let plain = parse(r#"<Button Content="OK"/>"#, &NoImages).unwrap();
        assert!(plain.action.is_none());
    }

    #[test]
    fn parses_names_and_keys() {
        let root = parse(r#"<Panel x:Name="shell" Key="k1"/>"#, &NoImages).unwrap();
        assert_eq!(root.name.as_deref(), Some("shell"));
        assert_eq!(root.key.as_deref(), Some("k1"));
    }

    #[test]
    fn parses_grid_into_rows() {
        let xaml = r#"<Grid Columns="2">
            <TextBlock Text="a"/><TextBlock Text="b"/><TextBlock Text="c"/>
        </Grid>"#;
        let root = parse(xaml, &NoImages).unwrap();
        assert_eq!(root.children.len(), 2, "two rows for three cells");
        assert_eq!(root.children[0].children.len(), 2);
        assert_eq!(root.children[1].children.len(), 1);
    }

    #[test]
    fn parses_canvas_position_as_margin() {
        let root = parse(r#"<Canvas><Panel Canvas.Left="12" Canvas.Top="7"/></Canvas>"#, &NoImages).unwrap();
        assert_eq!(root.children[0].style.margin, Edges::new(12.0, 7.0, 0.0, 0.0));
    }

    #[test]
    fn rejects_unknown_elements() {
        let error = parse("<Frobnicator/>", &NoImages).unwrap_err();
        assert!(error.message.contains("unsupported element"));
    }

    #[test]
    fn rejects_unclosed_tags() {
        let error = parse("<StackPanel><TextBlock/></StackPanel", &NoImages).unwrap_err();
        assert!(error.message.contains("unterminated closing tag") || error.message.contains("unclosed"));
    }

    #[test]
    fn rejects_mismatched_close() {
        let error = parse("<StackPanel></Border>", &NoImages).unwrap_err();
        assert!(error.message.contains("closes"));
    }

    const THEME: &str = r##"
<Styles>
  <Style TargetType="Button" Background="#303840" Foreground="#F0F2F5" Padding="14,8" CornerRadius="6"/>
  <Style TargetType="TextBlock" FontSize="18" Foreground="#C8CDD3"/>
  <Style x:Name="title" FontSize="24" Foreground="#FFFFFF"/>
</Styles>"##;

    #[test]
    fn styles_apply_as_defaults() {
        let sheet = parse_stylesheet(THEME).unwrap();
        let page = parse_with_styles(
            r#"<StackPanel><Button Content="OK"/><TextBlock Text="hi"/></StackPanel>"#,
            &NoImages,
            &sheet,
        )
        .unwrap();
        assert_eq!(page.children[0].style.radius, 6.0);
        assert_eq!(page.children[0].style.padding, Edges::symmetric(14.0, 8.0));
        assert_eq!(page.children[1].style.font_size, 18.0);
    }

    #[test]
    fn attributes_override_styles() {
        let sheet = parse_stylesheet(THEME).unwrap();
        let page = parse_with_styles(
            r#"<StackPanel><Button Content="OK" FontSize="11" CornerRadius="0"/></StackPanel>"#,
            &NoImages,
            &sheet,
        )
        .unwrap();
        assert_eq!(page.children[0].style.font_size, 11.0);
        assert_eq!(page.children[0].style.radius, 0.0);
    }

    #[test]
    fn name_selector_beats_type_selector() {
        let sheet = parse_stylesheet(THEME).unwrap();
        let page = parse_with_styles(
            r#"<StackPanel><TextBlock Text="t" x:Name="title"/></StackPanel>"#,
            &NoImages,
            &sheet,
        )
        .unwrap();
        assert_eq!(page.children[0].style.font_size, 24.0);
        assert_eq!(page.children[0].style.text_color, Color::WHITE);
    }

    #[test]
    fn text_properties_inherit_into_children() {
        let page = parse(
            r##"<StackPanel FontSize="22" Foreground="#112233">
                 <Panel><TextBlock Text="deep"/></Panel>
               </StackPanel>"##,
            &NoImages,
        )
        .unwrap();
        let deep = &page.children[0].children[0];
        assert_eq!(deep.style.font_size, 22.0);
        assert_eq!(deep.style.text_color, Color::rgb(0x11, 0x22, 0x33));
    }

    #[test]
    fn child_can_break_inheritance() {
        let page = parse(
            r#"<StackPanel FontSize="22"><TextBlock Text="a" FontSize="9"/></StackPanel>"#,
            &NoImages,
        )
        .unwrap();
        assert_eq!(page.children[0].style.font_size, 9.0);
    }

    #[test]
    fn wrapped_text_still_honours_inherited_size() {
        let sheet = parse_stylesheet(THEME).unwrap();
        let page = parse_with_styles(
            r#"<StackPanel><TextBlock Text="a b c" TextWrapping="Wrap"/></StackPanel>"#,
            &NoImages,
            &sheet,
        )
        .unwrap();
        assert!(page.children[0].style.wrap);
        assert_eq!(page.children[0].style.font_size, 18.0);
    }

    #[test]
    fn stylesheet_requires_styles_root() {
        assert!(parse_stylesheet("<Style TargetType=\"Button\"/>").is_err());
    }

    #[test]
    fn stylesheet_rejects_unknown_children() {
        assert!(parse_stylesheet(r#"<Styles><Nope/></Styles>"#).is_err());
    }

    #[test]
    fn empty_stylesheet_is_a_no_op() {
        let sheet = parse_stylesheet(r#"<Styles></Styles>"#).unwrap();
        assert!(sheet.is_empty());
        let a = parse(r#"<TextBlock Text="x" FontSize="13"/>"#, &NoImages).unwrap();
        let b = parse_with_styles(r#"<TextBlock Text="x" FontSize="13"/>"#, &NoImages, &sheet).unwrap();
        assert_eq!(a.style.font_size, b.style.font_size);
    }

    const CUI: &str = r##"
<Ui>
  <Data>
    <Value Key="title" Text="Settings"/>
    <Value Key="accent" Color="#2F6FD0"/>
    <Value Key="wide" Number="240"/>
    <Value Key="enabled" Bool="True"/>
  </Data>
  <Styles>
    <Style TargetType="Button" Background="#333A42" CornerRadius="6"/>
    <Style Class="accent" Background="#2F6FD0" Foreground="#FFFFFF"/>
    <Style x:Name="heading" FontSize="20"/>
  </Styles>
  <StackPanel>
    <TextBlock Text="{Binding title}" x:Name="heading"/>
    <Button Content="Save" Width="{Binding wide}" Class="accent"/>
    <Panel Background="{Binding accent}" Height="10"/>
    <Panel Height="10" Visible="{Binding enabled}"/>
  </StackPanel>
</Ui>"##;

    #[test]
    fn cui_document_parses_data_styles_and_root() {
        let document = UiDocument::parse(CUI).unwrap();
        assert_eq!(document.data.len(), 4);
        assert_eq!(document.sheet.len(), 3);
        assert_eq!(document.data.get("title").and_then(|v| v.text()), Some("Settings"));
    }

    #[test]
    fn cui_build_resolves_bindings_and_classes() {
        let document = UiDocument::parse(CUI).unwrap();
        let page = document.build(&NoImages).unwrap();
        assert_eq!(page.children.len(), 4);
        assert_eq!(page.children[0].view, node::View::Text("Settings".into()));
        assert_eq!(page.children[0].style.font_size, 20.0, "name selector applied");
        // Button: type style radius, class style background, bound width.
        assert_eq!(page.children[1].style.radius, 6.0);
        assert_eq!(page.children[1].style.background, Color::rgb(0x2F, 0x6F, 0xD0));
        assert_eq!(page.children[1].style.width, Length::Px(240.0));
        // Bound colour and visibility.
        assert_eq!(page.children[2].style.background, Color::rgb(0x2F, 0x6F, 0xD0));
        assert!(page.children[3].visible);
    }

    #[test]
    fn cui_overrides_win_over_document_data() {
        let document = UiDocument::parse(CUI).unwrap();
        let overrides = crate::binding::DataContext::new()
            .set("title", crate::binding::Value::Text("Overridden".into()))
            .set("enabled", crate::binding::Value::Bool(false));
        let page = document.build_with(&NoImages, &overrides).unwrap();
        assert_eq!(page.children[0].view, node::View::Text("Overridden".into()));
        assert!(!page.children[3].visible);
    }

    #[test]
    fn cui_requires_a_ui_root() {
        assert!(UiDocument::parse("<StackPanel/>").is_err());
    }

    #[test]
    fn cui_requires_a_page_root() {
        assert!(UiDocument::parse("<Ui><Styles/></Ui>").is_err());
    }

    #[test]
    fn cui_rejects_two_page_roots() {
        assert!(UiDocument::parse("<Ui><Panel/><Panel/></Ui>").is_err());
    }

    #[test]
    fn cui_rejects_malformed_values() {
        assert!(UiDocument::parse(r#"<Ui><Data><Value Key="x"/></Data><Panel/></Ui>"#).is_err());
        assert!(UiDocument::parse(r#"<Ui><Data><Value Number="x"/></Data><Panel/></Ui>"#).is_err());
    }

    #[test]
    fn parses_scroll_viewer() {
        let root = parse(
            r#"<ScrollViewer Height="120"><TextBlock Text="a"/></ScrollViewer>"#,
            &NoImages,
        )
        .unwrap();
        assert!(root.scroll.enabled);
        assert!(root.style.clip, "scroll containers clip their content");
    }

    #[test]
    fn parses_wrapping_and_focus() {
        let root = parse(
            r#"<TextBlock Text="a b c" TextWrapping="Wrap" Focusable="True" Width="Fill"/>"#,
            &NoImages,
        )
        .unwrap();
        assert!(root.style.wrap);
        assert!(root.focusable);
        assert_eq!(root.style.width, crate::style::Length::Fill);
    }

    #[test]
    fn text_wrapping_is_off_by_default() {
        let root = parse(r#"<TextBlock Text="a"/>"#, &NoImages).unwrap();
        assert!(!root.style.wrap);
        assert!(!root.focusable);
    }

    #[test]
    fn parses_wpf_colours() {
        assert_eq!(parse_color("#FFF", 0).unwrap(), Color::rgb(255, 255, 255));
        assert_eq!(parse_color("#112233", 0).unwrap(), Color::rgb(0x11, 0x22, 0x33));
        assert_eq!(parse_color("#80112233", 0).unwrap(), Color::rgba(0x11, 0x22, 0x33, 0x80));
        assert_eq!(parse_color("Transparent", 0).unwrap(), Color::TRANSPARENT);
        assert!(parse_color("mauve", 0).is_err());
    }

    #[test]
    fn parses_lengths_and_edges() {
        assert_eq!(parse_edges("4", 0, "Padding").unwrap(), Edges::all(4.0));
        assert_eq!(parse_edges("4,8", 0, "Padding").unwrap(), Edges::symmetric(4.0, 8.0));
        assert_eq!(parse_edges("1,2,3,4", 0, "Padding").unwrap(), Edges::new(1.0, 2.0, 3.0, 4.0));
        assert!(parse_edges("1,2,3", 0, "Padding").is_err());
    }

    #[test]
    fn resolves_images_through_the_resolver() {
        let resolver = |source: &str| {
            if source == "logo" {
                Some(ImageHandle(7))
            } else {
                None
            }
        };
        let root = parse(r#"<Image Source="logo"/>"#, &resolver).unwrap();
        assert_eq!(root.view, node::View::Image { handle: ImageHandle(7), fit: Fit::Fill });
        assert!(parse(r#"<Image Source="missing"/>"#, &resolver).is_err());
    }

    #[test]
    fn ignores_comments_and_prolog() {
        let xaml = r#"<?xml version="1.0"?>
            <!-- a comment -->
            <Panel/>"#;
        assert!(parse(xaml, &NoImages).is_ok());
    }

    #[test]
    fn parses_nested_structures() {
        let xaml = r##"<Border Background="#202020" CornerRadius="6">
            <StackPanel Spacing="4">
                <TextBlock Text="one"/>
                <TextBlock Text="two"/>
            </StackPanel>
        </Border>"##;
        let root = parse(xaml, &NoImages).unwrap();
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].children.len(), 2);
    }
}
