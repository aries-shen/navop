mod conversion;
mod parser;
mod properties;
mod style;

pub use parser::parse_css;
pub use style::{apply_css, resolve_style};

use std::collections::BTreeMap;
use std::fmt;
use thiserror::Error;

use crate::{ColorToken, CompileLimits};

pub const DEFAULT_MAX_CSS_SOURCE_BYTES: usize = 128 * 1024;
pub const DEFAULT_MAX_CSS_RULES: usize = 2_048;
pub const DEFAULT_MAX_CSS_SELECTORS: usize = 4_096;
pub const DEFAULT_MAX_CSS_DECLARATIONS: usize = 8_192;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CssStylesheet {
    pub rules: Vec<CssRule>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CssRule {
    pub selectors: Vec<CssSelector>,
    pub declarations: Vec<CssDeclaration>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CssSelector {
    pub tag: Option<String>,
    pub classes: Vec<String>,
    pub id: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CssDeclaration {
    pub property: CssProperty,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CssProperty {
    Display(CssDisplay),
    FlexDirection(CssFlexDirection),
    FlexGrow(f32),
    FlexShrink(f32),
    AlignItems(CssAlignItems),
    JustifyContent(CssJustifyContent),
    Gap(CssLength),
    Padding(CssLength),
    PaddingX(CssLength),
    PaddingY(CssLength),
    Margin(CssLength),
    MarginX(CssLength),
    MarginY(CssLength),
    Width(CssLength),
    Height(CssLength),
    MinWidth(CssLength),
    MinHeight(CssLength),
    MaxWidth(CssLength),
    MaxHeight(CssLength),
    FlexBasis(CssLength),
    AlignSelf(CssAlignItems),
    FlexWrap(CssFlexWrap),
    Background(CssColor),
    Color(CssColor),
    BorderWidth(CssLength),
    BorderStyle(CssBorderStyle),
    BorderColor(CssColor),
    BorderRadius(CssLength),
    Opacity(f32),
    FontSize(CssLength),
    FontWeight(u16),
    LineHeight(CssLength),
    TextAlign(CssTextAlign),
    OverflowX(CssOverflow),
    OverflowY(CssOverflow),
    Position(CssPosition),
    Top(CssLength),
    Right(CssLength),
    Bottom(CssLength),
    Left(CssLength),
    GridTemplateColumns(u16),
    GridTemplateRows(u16),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CssLength {
    Px(f32),
    Percent(f32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CssColor {
    Token(ColorToken),
    Rgb {
        red: u8,
        green: u8,
        blue: u8,
    },
    Rgba {
        red: u8,
        green: u8,
        blue: u8,
        alpha: f32,
    },
    Transparent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CssDisplay {
    Flex,
    Grid,
    Block,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CssFlexDirection {
    Row,
    Column,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CssAlignItems {
    Start,
    Center,
    End,
    Stretch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CssJustifyContent {
    Start,
    Center,
    End,
    Between,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CssTextAlign {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CssOverflow {
    Visible,
    Hidden,
    Scroll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CssBorderStyle {
    Solid,
    Dashed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CssFlexWrap {
    NoWrap,
    Wrap,
    WrapReverse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CssPosition {
    Relative,
    Absolute,
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum CssError {
    #[error("CSS {resource} limit exceeded: limit {limit}, observed {actual}")]
    ResourceLimitExceeded {
        resource: CssResource,
        limit: usize,
        actual: usize,
    },
    #[error("invalid CSS at {position}: {message}")]
    Syntax { position: usize, message: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CssResource {
    SourceBytes,
    Rules,
    Selectors,
    Declarations,
}

impl fmt::Display for CssResource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::SourceBytes => "source bytes",
            Self::Rules => "rules",
            Self::Selectors => "selectors",
            Self::Declarations => "declarations",
        };
        formatter.write_str(name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CssLimits {
    pub max_source_bytes: usize,
    pub max_rules: usize,
    pub max_selectors: usize,
    pub max_declarations: usize,
}

impl From<CompileLimits> for CssLimits {
    fn from(limits: CompileLimits) -> Self {
        Self {
            max_source_bytes: limits.max_css_source_bytes,
            max_rules: limits.max_css_rules,
            max_selectors: limits.max_css_selectors,
            max_declarations: limits.max_css_declarations,
        }
    }
}

impl CssLimits {
    pub const DEFAULT: Self = Self {
        max_source_bytes: DEFAULT_MAX_CSS_SOURCE_BYTES,
        max_rules: DEFAULT_MAX_CSS_RULES,
        max_selectors: DEFAULT_MAX_CSS_SELECTORS,
        max_declarations: DEFAULT_MAX_CSS_DECLARATIONS,
    };
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResolvedCssStyle {
    properties: BTreeMap<CssPropertyKey, CssProperty>,
}

impl ResolvedCssStyle {
    pub fn get(&self, key: CssPropertyKey) -> Option<&CssProperty> {
        self.properties.get(&key)
    }

    pub fn properties(&self) -> impl Iterator<Item = &CssProperty> {
        self.properties.values()
    }

    pub(crate) fn set(&mut self, property: CssProperty) {
        self.properties
            .insert(css_property_key(&property), property);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CssPropertyKey {
    Display,
    FlexDirection,
    FlexGrow,
    FlexShrink,
    AlignItems,
    JustifyContent,
    Gap,
    Padding,
    PaddingX,
    PaddingY,
    Margin,
    MarginX,
    MarginY,
    Width,
    Height,
    MinWidth,
    MinHeight,
    MaxWidth,
    MaxHeight,
    FlexBasis,
    AlignSelf,
    FlexWrap,
    Background,
    Color,
    BorderWidth,
    BorderStyle,
    BorderColor,
    BorderRadius,
    Opacity,
    FontSize,
    FontWeight,
    LineHeight,
    TextAlign,
    OverflowX,
    OverflowY,
    Position,
    Top,
    Right,
    Bottom,
    Left,
    GridTemplateColumns,
    GridTemplateRows,
}

pub fn css_property_key(property: &CssProperty) -> CssPropertyKey {
    match property {
        CssProperty::Display(_) => CssPropertyKey::Display,
        CssProperty::FlexDirection(_) => CssPropertyKey::FlexDirection,
        CssProperty::FlexGrow(_) => CssPropertyKey::FlexGrow,
        CssProperty::FlexShrink(_) => CssPropertyKey::FlexShrink,
        CssProperty::AlignItems(_) => CssPropertyKey::AlignItems,
        CssProperty::JustifyContent(_) => CssPropertyKey::JustifyContent,
        CssProperty::Gap(_) => CssPropertyKey::Gap,
        CssProperty::Padding(_) => CssPropertyKey::Padding,
        CssProperty::PaddingX(_) => CssPropertyKey::PaddingX,
        CssProperty::PaddingY(_) => CssPropertyKey::PaddingY,
        CssProperty::Margin(_) => CssPropertyKey::Margin,
        CssProperty::MarginX(_) => CssPropertyKey::MarginX,
        CssProperty::MarginY(_) => CssPropertyKey::MarginY,
        CssProperty::Width(_) => CssPropertyKey::Width,
        CssProperty::Height(_) => CssPropertyKey::Height,
        CssProperty::MinWidth(_) => CssPropertyKey::MinWidth,
        CssProperty::MinHeight(_) => CssPropertyKey::MinHeight,
        CssProperty::MaxWidth(_) => CssPropertyKey::MaxWidth,
        CssProperty::MaxHeight(_) => CssPropertyKey::MaxHeight,
        CssProperty::FlexBasis(_) => CssPropertyKey::FlexBasis,
        CssProperty::AlignSelf(_) => CssPropertyKey::AlignSelf,
        CssProperty::FlexWrap(_) => CssPropertyKey::FlexWrap,
        CssProperty::Background(_) => CssPropertyKey::Background,
        CssProperty::Color(_) => CssPropertyKey::Color,
        CssProperty::BorderWidth(_) => CssPropertyKey::BorderWidth,
        CssProperty::BorderStyle(_) => CssPropertyKey::BorderStyle,
        CssProperty::BorderColor(_) => CssPropertyKey::BorderColor,
        CssProperty::BorderRadius(_) => CssPropertyKey::BorderRadius,
        CssProperty::Opacity(_) => CssPropertyKey::Opacity,
        CssProperty::FontSize(_) => CssPropertyKey::FontSize,
        CssProperty::FontWeight(_) => CssPropertyKey::FontWeight,
        CssProperty::LineHeight(_) => CssPropertyKey::LineHeight,
        CssProperty::TextAlign(_) => CssPropertyKey::TextAlign,
        CssProperty::OverflowX(_) => CssPropertyKey::OverflowX,
        CssProperty::OverflowY(_) => CssPropertyKey::OverflowY,
        CssProperty::Position(_) => CssPropertyKey::Position,
        CssProperty::Top(_) => CssPropertyKey::Top,
        CssProperty::Right(_) => CssPropertyKey::Right,
        CssProperty::Bottom(_) => CssPropertyKey::Bottom,
        CssProperty::Left(_) => CssPropertyKey::Left,
        CssProperty::GridTemplateColumns(_) => CssPropertyKey::GridTemplateColumns,
        CssProperty::GridTemplateRows(_) => CssPropertyKey::GridTemplateRows,
    }
}
