use super::{
    CssAlignItems, CssBorderStyle, CssColor, CssDisplay, CssFlexDirection, CssFlexWrap,
    CssJustifyContent, CssLength, CssOverflow, CssPosition, CssProperty, CssTextAlign,
};
use crate::ColorToken;

pub(super) fn parse_properties(name: &str, value: &str) -> Option<Vec<CssProperty>> {
    reject_unsafe(value)?;
    if let Some(properties) = match name {
        "border" => parse_border(value),
        "overflow" => parse_overflow_shorthand(value),
        _ => None,
    } {
        return Some(properties);
    }
    match name {
        "display" => parse_keyword(
            value,
            &[
                ("flex", CssProperty::Display(CssDisplay::Flex)),
                ("grid", CssProperty::Display(CssDisplay::Grid)),
                ("block", CssProperty::Display(CssDisplay::Block)),
                ("none", CssProperty::Display(CssDisplay::None)),
            ],
        ),
        "flex-direction" => parse_keyword(
            value,
            &[
                ("row", CssProperty::FlexDirection(CssFlexDirection::Row)),
                (
                    "column",
                    CssProperty::FlexDirection(CssFlexDirection::Column),
                ),
            ],
        ),
        "flex-grow" => parse_float(value).map(CssProperty::FlexGrow),
        "flex-shrink" => parse_float(value).map(CssProperty::FlexShrink),
        "align-items" => parse_keyword(
            value,
            &[
                ("start", CssProperty::AlignItems(CssAlignItems::Start)),
                ("center", CssProperty::AlignItems(CssAlignItems::Center)),
                ("end", CssProperty::AlignItems(CssAlignItems::End)),
                ("stretch", CssProperty::AlignItems(CssAlignItems::Stretch)),
            ],
        ),
        "justify-content" => parse_keyword(
            value,
            &[
                (
                    "start",
                    CssProperty::JustifyContent(CssJustifyContent::Start),
                ),
                (
                    "center",
                    CssProperty::JustifyContent(CssJustifyContent::Center),
                ),
                ("end", CssProperty::JustifyContent(CssJustifyContent::End)),
                (
                    "space-between",
                    CssProperty::JustifyContent(CssJustifyContent::Between),
                ),
            ],
        ),
        "gap" => parse_absolute_length(value).map(CssProperty::Gap),
        "padding" => parse_absolute_length(value).map(CssProperty::Padding),
        "padding-x" => parse_absolute_length(value).map(CssProperty::PaddingX),
        "padding-y" => parse_absolute_length(value).map(CssProperty::PaddingY),
        "margin" => parse_absolute_length(value).map(CssProperty::Margin),
        "margin-x" => parse_absolute_length(value).map(CssProperty::MarginX),
        "margin-y" => parse_absolute_length(value).map(CssProperty::MarginY),
        "width" => parse_length(value).map(CssProperty::Width),
        "height" => parse_length(value).map(CssProperty::Height),
        "min-width" => parse_length(value).map(CssProperty::MinWidth),
        "min-height" => parse_length(value).map(CssProperty::MinHeight),
        "max-width" => parse_length(value).map(CssProperty::MaxWidth),
        "max-height" => parse_length(value).map(CssProperty::MaxHeight),
        "flex-basis" => parse_length(value).map(CssProperty::FlexBasis),
        "align-self" => parse_keyword(
            value,
            &[
                ("start", CssProperty::AlignSelf(CssAlignItems::Start)),
                ("center", CssProperty::AlignSelf(CssAlignItems::Center)),
                ("end", CssProperty::AlignSelf(CssAlignItems::End)),
                ("stretch", CssProperty::AlignSelf(CssAlignItems::Stretch)),
            ],
        ),
        "flex-wrap" => parse_keyword(
            value,
            &[
                ("nowrap", CssProperty::FlexWrap(CssFlexWrap::NoWrap)),
                ("wrap", CssProperty::FlexWrap(CssFlexWrap::Wrap)),
                (
                    "wrap-reverse",
                    CssProperty::FlexWrap(CssFlexWrap::WrapReverse),
                ),
            ],
        ),
        "background" => parse_color(value).map(CssProperty::Background),
        "color" => parse_color(value).map(CssProperty::Color),
        "border-width" => parse_absolute_length(value).map(CssProperty::BorderWidth),
        "border-style" => parse_keyword(
            value,
            &[
                ("solid", CssProperty::BorderStyle(CssBorderStyle::Solid)),
                ("dashed", CssProperty::BorderStyle(CssBorderStyle::Dashed)),
            ],
        ),
        "border-color" => parse_color(value).map(CssProperty::BorderColor),
        "border-radius" => parse_absolute_length(value).map(CssProperty::BorderRadius),
        "opacity" => parse_opacity(value).map(CssProperty::Opacity),
        "font-size" => parse_absolute_length(value).map(CssProperty::FontSize),
        "font-weight" => parse_font_weight(value).map(CssProperty::FontWeight),
        "line-height" => parse_length(value).map(CssProperty::LineHeight),
        "text-align" => parse_keyword(
            value,
            &[
                ("left", CssProperty::TextAlign(CssTextAlign::Left)),
                ("center", CssProperty::TextAlign(CssTextAlign::Center)),
                ("right", CssProperty::TextAlign(CssTextAlign::Right)),
            ],
        ),
        "overflow-x" => parse_overflow(value).map(CssProperty::OverflowX),
        "overflow-y" => parse_overflow(value).map(CssProperty::OverflowY),
        "position" => parse_keyword(
            value,
            &[
                ("relative", CssProperty::Position(CssPosition::Relative)),
                ("absolute", CssProperty::Position(CssPosition::Absolute)),
            ],
        ),
        "top" => parse_length(value).map(CssProperty::Top),
        "right" => parse_length(value).map(CssProperty::Right),
        "bottom" => parse_length(value).map(CssProperty::Bottom),
        "left" => parse_length(value).map(CssProperty::Left),
        "grid-template-columns" => parse_grid(value).map(CssProperty::GridTemplateColumns),
        "grid-template-rows" => parse_grid(value).map(CssProperty::GridTemplateRows),
        _ => None,
    }
    .map(|property| vec![property])
}

fn reject_unsafe(value: &str) -> Option<()> {
    let normalized = value.to_ascii_lowercase();
    let unsafe_token = normalized
        .split(|char: char| char.is_ascii_whitespace() || matches!(char, ',' | '(' | ')' | ';'))
        .any(|token| matches!(token, "url" | "var" | "expression"));
    (!unsafe_token && !normalized.contains("!important")).then_some(())
}

fn parse_keyword(value: &str, options: &[(&str, CssProperty)]) -> Option<CssProperty> {
    options
        .iter()
        .find(|(keyword, _)| *keyword == value)
        .map(|(_, property)| property.clone())
}

fn parse_float(value: &str) -> Option<f32> {
    let parsed = value.parse::<f32>().ok()?;
    (parsed.is_finite() && parsed >= 0.0).then_some(parsed)
}

fn parse_opacity(value: &str) -> Option<f32> {
    let parsed = parse_float(value)?;
    (0.0..=1.0).contains(&parsed).then_some(parsed)
}

fn parse_font_weight(value: &str) -> Option<u16> {
    let parsed = value.parse::<u16>().ok()?;
    (100..=900).contains(&parsed).then_some(parsed)
}

fn parse_length(value: &str) -> Option<CssLength> {
    if let Some(number) = value.strip_suffix("px") {
        let parsed = parse_float(number)?;
        return (parsed <= 4_096.0).then_some(CssLength::Px(parsed));
    }
    if let Some(number) = value.strip_suffix('%') {
        let parsed = parse_float(number)?;
        return (parsed <= 100.0).then_some(CssLength::Percent(parsed / 100.0));
    }
    None
}

fn parse_absolute_length(value: &str) -> Option<CssLength> {
    match parse_length(value)? {
        length @ CssLength::Px(_) => Some(length),
        CssLength::Percent(_) => None,
    }
}

fn parse_overflow(value: &str) -> Option<CssOverflow> {
    match value {
        "visible" => Some(CssOverflow::Visible),
        "hidden" => Some(CssOverflow::Hidden),
        "scroll" => Some(CssOverflow::Scroll),
        _ => None,
    }
}

fn parse_overflow_shorthand(value: &str) -> Option<Vec<CssProperty>> {
    let mut parts = value.split_whitespace();
    let first = parts.next()?;
    let second = parts.next();
    if parts.next().is_some() {
        return None;
    }
    let x = parse_overflow(first)?;
    let y = match second {
        Some(second) => parse_overflow(second)?,
        None => x,
    };
    Some(vec![CssProperty::OverflowX(x), CssProperty::OverflowY(y)])
}

fn parse_border(value: &str) -> Option<Vec<CssProperty>> {
    let mut width = None;
    let mut style = None;
    let mut border_color = None;
    for token in split_function_values(value) {
        if let Some(parsed_width) = parse_absolute_length(&token) {
            width = Some(parsed_width);
        } else if let Some(parsed_style) = match token.as_str() {
            "solid" => Some(CssBorderStyle::Solid),
            "dashed" => Some(CssBorderStyle::Dashed),
            _ => None,
        } {
            style = Some(parsed_style);
        } else if let Some(parsed_color) = parse_color(&token) {
            border_color = Some(parsed_color);
        } else {
            return None;
        }
    }
    let width = width?;
    let style = style?;
    let mut properties = vec![
        CssProperty::BorderWidth(width),
        CssProperty::BorderStyle(style),
    ];
    if let Some(border_color) = border_color {
        properties.push(CssProperty::BorderColor(border_color));
    }
    Some(properties)
}

fn split_function_values(value: &str) -> impl Iterator<Item = String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut depth = 0_usize;
    for char in value.chars() {
        match char {
            '(' => {
                depth = depth.saturating_add(1);
                token.push(char);
            }
            ')' if depth > 0 => {
                depth -= 1;
                token.push(char);
            }
            char if depth == 0 && char.is_whitespace() => {
                if !token.is_empty() {
                    tokens.push(std::mem::take(&mut token));
                }
            }
            char => token.push(char),
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens.into_iter()
}

fn parse_color(value: &str) -> Option<CssColor> {
    if value.eq_ignore_ascii_case("transparent") {
        return Some(CssColor::Transparent);
    }
    if let Some(token) = parse_color_token(value) {
        return Some(CssColor::Token(token));
    }
    if let Some(color) = parse_rgb_color(value) {
        return Some(color);
    }
    let hex = value.strip_prefix('#')?;
    if hex.len() == 3 && hex.chars().all(|char| char.is_ascii_hexdigit()) {
        let digits = [hex.as_bytes()[0], hex.as_bytes()[1], hex.as_bytes()[2]];
        let channel =
            |digit: u8| u32::from_str_radix((digit as char).to_string().as_str(), 16).ok();
        let red = u8::try_from(channel(digits[0])? * 17).ok()?;
        let green = u8::try_from(channel(digits[1])? * 17).ok()?;
        let blue = u8::try_from(channel(digits[2])? * 17).ok()?;
        return Some(CssColor::Rgb { red, green, blue });
    }
    if hex.len() == 6 && hex.chars().all(|char| char.is_ascii_hexdigit()) {
        let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
        return Some(CssColor::Rgb { red, green, blue });
    }
    None
}

fn parse_rgb_color(value: &str) -> Option<CssColor> {
    let (function, body) = value.split_once('(')?;
    if !matches!(function, "rgb" | "rgba") {
        return None;
    }
    if !body.ends_with(')') {
        return None;
    }
    let body = &body[..body.len() - 1];
    let channels = body.split(',').map(str::trim).collect::<Vec<_>>();
    if channels.len() != 3 && channels.len() != 4 {
        return None;
    }
    let parse_channel = |value: &str| value.parse::<u8>().ok();
    let alpha = if channels.len() == 4 {
        channels[3]
            .parse::<f32>()
            .ok()
            .filter(|alpha| alpha.is_finite() && (0.0..=1.0).contains(alpha))?
    } else {
        1.0
    };
    let red = parse_channel(channels[0])?;
    let green = parse_channel(channels[1])?;
    let blue = parse_channel(channels[2])?;
    Some(if channels.len() == 4 {
        CssColor::Rgba {
            red,
            green,
            blue,
            alpha,
        }
    } else {
        CssColor::Rgb { red, green, blue }
    })
}

fn parse_color_token(value: &str) -> Option<ColorToken> {
    match value {
        "zinc-950" => Some(ColorToken::Zinc950),
        "zinc-900" => Some(ColorToken::Zinc900),
        "zinc-800" => Some(ColorToken::Zinc800),
        "zinc-700" => Some(ColorToken::Zinc700),
        "zinc-400" => Some(ColorToken::Zinc400),
        "zinc-100" => Some(ColorToken::Zinc100),
        "blue-600" => Some(ColorToken::Blue600),
        "emerald-400" => Some(ColorToken::Emerald400),
        "white" => Some(ColorToken::White),
        _ => None,
    }
}

fn parse_grid(value: &str) -> Option<u16> {
    let count = value.split_whitespace().count();
    if count != 1 {
        return None;
    }
    let parsed = value.parse::<u16>().ok()?;
    (1..=16).contains(&parsed).then_some(parsed)
}
