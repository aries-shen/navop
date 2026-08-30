use gpui::{Div, ParentElement, Styled, div, prelude::FluentBuilder, px, rgb};
use gpui_component::{Icon, IconName, IconNamed as _, IconSize, Sizable};

use super::{
    CARD_BACKGROUND_RGB, CARD_BORDER_RGB, ICON_SIZE_MATRIX, MUTED_TEXT_RGB, PRIMARY_TEXT_RGB,
};

const CARD_WIDTH: f32 = 260.;
const CARD_PADDING: f32 = 12.;
const CARD_SECTION_GAP: f32 = 8.;
const ICON_MATRIX_PADDING: f32 = 8.;
const ICON_MATRIX_CELL_GAP: f32 = 4.;
const ICON_MATRIX_CELL_WIDTH: f32 = 52.;
const ICON_MATRIX_CELL_HEIGHT: f32 = 68.;
const ICON_MATRIX_LABEL_LINE_HEIGHT: f32 = 16.;
#[cfg(test)]
const ICON_MATRIX_CELL_BORDER_WIDTH: f32 = 1.;
const ICON_SURFACE_RGB: u32 = 0x27_27_2a;
pub(super) fn render_icon_card(icon: IconName, selected_size: IconSize) -> Div {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(CARD_WIDTH))
        .min_w_0()
        .gap(px(CARD_SECTION_GAP))
        .p(px(CARD_PADDING))
        .rounded_lg()
        .border_1()
        .border_color(rgb(CARD_BORDER_RGB))
        .bg(rgb(CARD_BACKGROUND_RGB))
        .child(render_icon_matrix(icon, selected_size))
        .child(metadata_line(format!("{icon:?}"), true))
        .child(metadata_line("Functional Outline", false))
        .child(metadata_line(icon.path(), true))
        .child(metadata_line("Source: gpui-component-assets", false))
}

fn render_icon_matrix(icon: IconName, selected_size: IconSize) -> Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_center()
        .gap(px(ICON_MATRIX_CELL_GAP))
        .p(px(ICON_MATRIX_PADDING))
        .rounded_lg()
        .bg(rgb(ICON_SURFACE_RGB))
        .text_color(rgb(PRIMARY_TEXT_RGB))
        .children(
            ICON_SIZE_MATRIX
                .into_iter()
                .map(|(size, label)| render_matrix_cell(icon, size, label, selected_size)),
        )
}

fn render_matrix_cell(icon: IconName, size: IconSize, label: u8, selected_size: IconSize) -> Div {
    let selected = size == selected_size;
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .w(px(ICON_MATRIX_CELL_WIDTH))
        .h(px(ICON_MATRIX_CELL_HEIGHT))
        .gap(px(ICON_MATRIX_CELL_GAP))
        .rounded_md()
        .border_1()
        .border_color(rgb(if selected {
            PRIMARY_TEXT_RGB
        } else {
            ICON_SURFACE_RGB
        }))
        .when(selected, |cell| cell.bg(rgb(CARD_BORDER_RGB)))
        .child(
            div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(Icon::new(icon).with_size(size)),
        )
        .child(
            div()
                .text_xs()
                .line_height(px(ICON_MATRIX_LABEL_LINE_HEIGHT))
                .text_color(rgb(if selected {
                    PRIMARY_TEXT_RGB
                } else {
                    MUTED_TEXT_RGB
                }))
                .child(format!("{label}")),
        )
}

fn metadata_line(content: impl Into<gpui::SharedString>, monospace: bool) -> Div {
    div()
        .min_w_0()
        .truncate()
        .when(monospace, |element| element.font_family("monospace"))
        .text_xs()
        .text_color(rgb(MUTED_TEXT_RGB))
        .child(content.into())
}

#[cfg(test)]
mod tests {
    use super::{
        CARD_PADDING, CARD_WIDTH, ICON_MATRIX_CELL_BORDER_WIDTH, ICON_MATRIX_CELL_GAP,
        ICON_MATRIX_CELL_HEIGHT, ICON_MATRIX_CELL_WIDTH, ICON_MATRIX_LABEL_LINE_HEIGHT,
        ICON_MATRIX_PADDING,
    };
    use crate::icon_gallery::ICON_SIZE_MATRIX;

    const MATRIX_COLUMNS: usize = 4;
    const HERO_ICON_SIZE: f32 = 40.;
    const MINIMUM_CELL_VERTICAL_BREATHING_ROOM: f32 = 4.;

    fn matrix_content_width() -> f32 {
        CARD_WIDTH - 2. * CARD_PADDING - 2. * ICON_MATRIX_PADDING
    }

    #[test]
    fn icon_matrix_fits_four_columns_but_not_five() {
        let available = matrix_content_width();
        let four_columns = MATRIX_COLUMNS as f32 * ICON_MATRIX_CELL_WIDTH
            + (MATRIX_COLUMNS - 1) as f32 * ICON_MATRIX_CELL_GAP;
        let five_columns = (MATRIX_COLUMNS + 1) as f32 * ICON_MATRIX_CELL_WIDTH
            + MATRIX_COLUMNS as f32 * ICON_MATRIX_CELL_GAP;

        assert_eq!(available, four_columns);
        assert!(five_columns > available);
    }

    #[test]
    fn seven_sizes_wrap_into_a_stable_four_plus_three_matrix() {
        let columns = ((matrix_content_width() + ICON_MATRIX_CELL_GAP)
            / (ICON_MATRIX_CELL_WIDTH + ICON_MATRIX_CELL_GAP))
            .floor() as usize;
        let rows = ICON_SIZE_MATRIX.len().div_ceil(columns);

        assert_eq!(columns, MATRIX_COLUMNS);
        assert_eq!(rows, 2);
        assert_eq!(ICON_SIZE_MATRIX.len() - columns, 3);
    }

    #[test]
    fn hero_icon_cell_keeps_label_and_breathing_room() {
        let required_height = HERO_ICON_SIZE
            + ICON_MATRIX_CELL_GAP
            + ICON_MATRIX_LABEL_LINE_HEIGHT
            + 2. * ICON_MATRIX_CELL_BORDER_WIDTH
            + MINIMUM_CELL_VERTICAL_BREATHING_ROOM;

        assert!(ICON_MATRIX_CELL_HEIGHT >= required_height);
    }
}
