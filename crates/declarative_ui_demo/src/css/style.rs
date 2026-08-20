use gpui::{
    AlignItems, BorderStyle, FlexDirection, FlexWrap, FontWeight, JustifyContent, Length, Overflow,
    Position, Styled, TextAlign,
};

use super::conversion::{
    absolute_length, color, definite_length, display, length, optional_length,
};
use super::{
    CssAlignItems, CssBorderStyle, CssFlexDirection, CssFlexWrap, CssJustifyContent, CssOverflow,
    CssPosition, CssProperty, CssRule, CssSelector, CssStylesheet, CssTextAlign, ResolvedCssStyle,
};
use crate::VElement;

pub fn resolve_style(stylesheet: &CssStylesheet, element: &VElement) -> ResolvedCssStyle {
    let mut resolved = ResolvedCssStyle::default();
    for rule in &stylesheet.rules {
        if selector_matches_any(&rule.selectors, element) {
            merge_rule(&mut resolved, rule);
        }
    }
    resolved
}

fn selector_matches_any(selectors: &[CssSelector], element: &VElement) -> bool {
    selectors
        .iter()
        .any(|selector| selector_matches(selector, element))
}

fn selector_matches(selector: &CssSelector, element: &VElement) -> bool {
    selector
        .tag
        .as_deref()
        .is_none_or(|tag| tag.eq_ignore_ascii_case(&element.tag))
        && selector
            .id
            .as_deref()
            .is_none_or(|id| element.attr("id") == Some(id))
        && selector
            .classes
            .iter()
            .all(|class| element.classes.contains(class))
}

fn merge_rule(resolved: &mut ResolvedCssStyle, rule: &CssRule) {
    for declaration in &rule.declarations {
        resolved.set(declaration.property.clone());
    }
}

pub fn apply_css<E: Styled>(element: E, style: &ResolvedCssStyle) -> E {
    style.properties().fold(element, apply_property)
}

fn apply_property<E: Styled>(element: E, property: &CssProperty) -> E {
    match property {
        CssProperty::Display(value) => {
            let mut element = element;
            element.style().display = Some(display(*value));
            element
        }
        CssProperty::FlexDirection(value) => match value {
            CssFlexDirection::Row => {
                let mut element = element;
                element.style().flex_direction = Some(FlexDirection::Row);
                element
            }
            CssFlexDirection::Column => {
                let mut element = element;
                element.style().flex_direction = Some(FlexDirection::Column);
                element
            }
        },
        CssProperty::FlexGrow(value) => element.flex_grow(*value),
        CssProperty::FlexShrink(value) => element.flex_shrink(*value),
        CssProperty::AlignItems(value) => match value {
            CssAlignItems::Start => {
                let mut element = element;
                element.style().align_items = Some(AlignItems::FlexStart);
                element
            }
            CssAlignItems::Center => {
                let mut element = element;
                element.style().align_items = Some(AlignItems::Center);
                element
            }
            CssAlignItems::End => {
                let mut element = element;
                element.style().align_items = Some(AlignItems::FlexEnd);
                element
            }
            CssAlignItems::Stretch => {
                let mut element = element;
                element.style().align_items = Some(AlignItems::Stretch);
                element
            }
        },
        CssProperty::JustifyContent(value) => match value {
            CssJustifyContent::Start => {
                let mut element = element;
                element.style().justify_content = Some(JustifyContent::Start);
                element
            }
            CssJustifyContent::Center => {
                let mut element = element;
                element.style().justify_content = Some(JustifyContent::Center);
                element
            }
            CssJustifyContent::End => {
                let mut element = element;
                element.style().justify_content = Some(JustifyContent::End);
                element
            }
            CssJustifyContent::Between => {
                let mut element = element;
                element.style().justify_content = Some(JustifyContent::SpaceBetween);
                element
            }
        },
        CssProperty::Gap(value) => element.gap(length(*value)),
        CssProperty::Padding(value) => element.p(length(*value)),
        CssProperty::PaddingX(value) => element.px(length(*value)),
        CssProperty::PaddingY(value) => element.py(length(*value)),
        CssProperty::Margin(value) => element.m(optional_length(*value)),
        CssProperty::MarginX(value) => element.mx(optional_length(*value)),
        CssProperty::MarginY(value) => element.my(optional_length(*value)),
        CssProperty::Width(value) => element.w(definite_length(*value)),
        CssProperty::Height(value) => element.h(definite_length(*value)),
        CssProperty::MinWidth(value) => element.min_w(definite_length(*value)),
        CssProperty::MinHeight(value) => element.min_h(definite_length(*value)),
        CssProperty::MaxWidth(value) => element.max_w(definite_length(*value)),
        CssProperty::MaxHeight(value) => element.max_h(definite_length(*value)),
        CssProperty::FlexBasis(value) => element.flex_basis(optional_length(*value)),
        CssProperty::AlignSelf(value) => {
            let alignment = match value {
                CssAlignItems::Start => AlignItems::FlexStart,
                CssAlignItems::Center => AlignItems::Center,
                CssAlignItems::End => AlignItems::FlexEnd,
                CssAlignItems::Stretch => AlignItems::Stretch,
            };
            let mut element = element;
            element.style().align_self = Some(alignment);
            element
        }
        CssProperty::FlexWrap(value) => {
            let wrap = match value {
                CssFlexWrap::NoWrap => FlexWrap::NoWrap,
                CssFlexWrap::Wrap => FlexWrap::Wrap,
                CssFlexWrap::WrapReverse => FlexWrap::WrapReverse,
            };
            let mut element = element;
            element.style().flex_wrap = Some(wrap);
            element
        }
        CssProperty::Background(value) => element.bg(color(*value)),
        CssProperty::Color(value) => element.text_color(color(*value)),
        CssProperty::BorderWidth(value) => element.border(absolute_length(*value)),
        CssProperty::BorderStyle(value) => {
            let style = match value {
                CssBorderStyle::Solid => BorderStyle::Solid,
                CssBorderStyle::Dashed => BorderStyle::Dashed,
            };
            let mut element = element;
            element.style().border_style = Some(style);
            element
        }
        CssProperty::BorderColor(value) => element.border_color(color(*value)),
        CssProperty::BorderRadius(value) => element.rounded(length(*value)),
        CssProperty::Opacity(value) => element.opacity(*value),
        CssProperty::FontSize(value) => element.text_size(length(*value)),
        CssProperty::FontWeight(value) => element.font_weight(FontWeight(f32::from(*value))),
        CssProperty::LineHeight(value) => element.line_height(definite_length(*value)),
        CssProperty::TextAlign(value) => match value {
            CssTextAlign::Left => element.text_align(TextAlign::Left),
            CssTextAlign::Center => element.text_align(TextAlign::Center),
            CssTextAlign::Right => element.text_align(TextAlign::Right),
        },
        CssProperty::OverflowX(value) => {
            let overflow = overflow(*value);
            let mut element = element;
            element.style().overflow.x = Some(overflow);
            element
        }
        CssProperty::OverflowY(value) => {
            let overflow = overflow(*value);
            let mut element = element;
            element.style().overflow.y = Some(overflow);
            element
        }
        CssProperty::Position(value) => {
            let position = match value {
                CssPosition::Relative => Position::Relative,
                CssPosition::Absolute => Position::Absolute,
            };
            let mut element = element;
            element.style().position = Some(position);
            element
        }
        CssProperty::Top(value) => {
            let inset: Length = optional_length(*value);
            let mut element = element;
            element.style().inset.top = Some(inset);
            element
        }
        CssProperty::Right(value) => {
            let inset: Length = optional_length(*value);
            let mut element = element;
            element.style().inset.right = Some(inset);
            element
        }
        CssProperty::Bottom(value) => {
            let inset: Length = optional_length(*value);
            let mut element = element;
            element.style().inset.bottom = Some(inset);
            element
        }
        CssProperty::Left(value) => {
            let inset: Length = optional_length(*value);
            let mut element = element;
            element.style().inset.left = Some(inset);
            element
        }
        CssProperty::GridTemplateColumns(value) => element.grid_cols(*value),
        CssProperty::GridTemplateRows(value) => element.grid_rows(*value),
    }
}

fn overflow(value: CssOverflow) -> Overflow {
    match value {
        CssOverflow::Visible => Overflow::Visible,
        CssOverflow::Hidden => Overflow::Hidden,
        CssOverflow::Scroll => Overflow::Scroll,
    }
}
