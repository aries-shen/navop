use gpui::{DefiniteLength, Hsla, Length, Pixels, px, rgb, transparent_black};
use palette::IntoColor;

use super::{ColorToken, CssColor, CssDisplay, CssLength};

pub(super) fn definite_length(value: CssLength) -> DefiniteLength {
    match value {
        CssLength::Px(value) => DefiniteLength::Absolute(px(value).into()),
        CssLength::Percent(value) => DefiniteLength::Fraction(value),
    }
}

pub(super) fn length(value: CssLength) -> gpui::Pixels {
    match value {
        CssLength::Px(value) => px(value),
        CssLength::Percent(value) => px(value * 100.0),
    }
}

// Use this helper for properties whose parser only accepts `px`. It keeps the
// conversion strict even if a future parser path accidentally allows `%`.
pub(super) fn absolute_length(value: CssLength) -> Pixels {
    match value {
        CssLength::Px(value) => px(value),
        CssLength::Percent(value) => {
            debug_assert!(
                false,
                "absolute-only CSS lengths are validated before style conversion: {value}"
            );
            px(0.0)
        }
    }
}

pub(super) fn optional_length(value: CssLength) -> Length {
    Length::Definite(definite_length(value))
}

pub(super) fn color(value: CssColor) -> Hsla {
    match value {
        CssColor::Token(token) => token_color(token),
        CssColor::Rgb { red, green, blue } => {
            rgb((u32::from(red) << 16) | (u32::from(green) << 8) | u32::from(blue)).into_color()
        }
        CssColor::Rgba {
            red,
            green,
            blue,
            alpha,
        } => {
            let mut color: Hsla =
                rgb((u32::from(red) << 16) | (u32::from(green) << 8) | u32::from(blue))
                    .into_color();
            color.alpha = alpha;
            color
        }
        CssColor::Transparent => transparent_black(),
    }
}

pub(super) fn display(value: CssDisplay) -> gpui::Display {
    match value {
        CssDisplay::Flex => gpui::Display::Flex,
        CssDisplay::Grid => gpui::Display::Grid,
        CssDisplay::Block => gpui::Display::Block,
        CssDisplay::None => gpui::Display::None,
    }
}

fn token_color(token: ColorToken) -> Hsla {
    let value = match token {
        ColorToken::Zinc950 => 0x09_09_0b,
        ColorToken::Zinc900 => 0x18_18_1b,
        ColorToken::Zinc800 => 0x27_27_2a,
        ColorToken::Zinc700 => 0x3f_3f_46,
        ColorToken::Zinc400 => 0xa1_a1_aa,
        ColorToken::Zinc100 => 0xf4_f4_f5,
        ColorToken::Blue600 => 0x25_63_eb,
        ColorToken::Emerald400 => 0x34_d3_99,
        ColorToken::White => 0xff_ff_ff,
    };
    rgb(value).into_color()
}
