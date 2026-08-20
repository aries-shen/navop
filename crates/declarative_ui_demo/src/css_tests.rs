use crate::{
    ColorToken, CompileLimits, CompileOptions, ComponentRegistry, CssAlignItems, CssBorderStyle,
    CssColor, CssError, CssFlexWrap, CssLength, CssOverflow, CssPosition, CssProperty, CssSelector,
    compile_template_with_style, css_property_key, parse_css,
};

#[test]
fn css_parser_accepts_scoped_simple_and_compound_selectors() {
    let stylesheet = parse_css(
        r#"
        /* provider surface */
        .panel, button.primary {
            display: flex;
            flex-direction: column;
            padding: 16px;
            background: zinc-900;
            color: #ffffff;
        }
        #refresh {
            width: 100%;
            opacity: 0.9;
        }
        "#,
        CompileLimits::DEFAULT,
    )
    .unwrap();

    assert_eq!(2, stylesheet.rules.len());
    assert_eq!(
        vec![
            CssSelector {
                tag: None,
                classes: vec!["panel".into()],
                id: None,
            },
            CssSelector {
                tag: Some("button".into()),
                classes: vec!["primary".into()],
                id: None,
            }
        ],
        stylesheet.rules[0].selectors
    );
    assert_eq!(
        Some(CssProperty::Background(CssColor::Token(
            ColorToken::Zinc900
        ))),
        stylesheet.rules[0]
            .declarations
            .get(3)
            .map(|item| item.property.clone())
    );
    assert_eq!(
        Some(CssProperty::Width(CssLength::Percent(1.0))),
        stylesheet.rules[1]
            .declarations
            .first()
            .map(|item| item.property.clone())
    );
}

#[test]
fn css_parser_rejects_unsafe_or_unsupported_syntax() {
    for source in [
        "@import url(secret.css);",
        ".panel { background: url(secret.png); }",
        ".panel { color: var(--text); }",
        ".panel { color: red !important; }",
        ".panel:hover { color: white; }",
        ".panel .child { color: white; }",
        ".panel { unknown: yes; }",
        ".panel { color: expression(alert(1)); }",
    ] {
        assert!(
            parse_css(source, CompileLimits::DEFAULT).is_err(),
            "expected rejection: {source}"
        );
    }
}

#[test]
fn css_compile_merges_external_css_before_tailwind_utilities() {
    let registry = ComponentRegistry::with_defaults();
    let template = compile_template_with_style(
        r#"<div id="root" class="p-2"><button id="refresh">Refresh</button></div>"#,
        Some("#root { padding: 16px; background: zinc-800; } button { color: white; }"),
        &registry,
        CompileOptions::strict(),
    )
    .unwrap();

    assert_eq!(2, template.stylesheet().rules.len());
    assert_eq!(
        Some(CssProperty::Padding(CssLength::Px(16.0))),
        template
            .resolved_style(template.root().element().unwrap())
            .get(css_property_key(&CssProperty::Padding(CssLength::Px(0.0))))
            .cloned()
    );
    assert_eq!(
        Some(CssProperty::Background(CssColor::Token(
            ColorToken::Zinc800
        ))),
        template
            .resolved_style(template.root().element().unwrap())
            .get(css_property_key(&CssProperty::Background(CssColor::Token(
                ColorToken::White,
            ))))
            .cloned()
    );
}

#[test]
fn css_errors_are_typed_in_strict_and_permissive_modes() {
    let registry = ComponentRegistry::with_defaults();
    let error = compile_template_with_style(
        "<div></div>",
        Some(".panel { unknown: yes; }"),
        &registry,
        CompileOptions::strict(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("template validation failed"));

    let permissive = compile_template_with_style(
        "<div></div>",
        Some(".panel { unknown: yes; }"),
        &registry,
        CompileOptions::permissive(),
    )
    .unwrap();
    assert_eq!(1, permissive.diagnostics().warnings().count());
}

#[test]
fn css_resource_limits_are_enforced() {
    let source = ".a { padding: 1px; }";
    let error = parse_css(
        source,
        CompileLimits {
            max_css_source_bytes: source.len() - 1,
            ..CompileLimits::default()
        },
    )
    .unwrap_err();
    assert!(matches!(error, CssError::ResourceLimitExceeded { .. }));
}

#[test]
fn css_parser_accepts_layout_border_position_and_color_extensions() {
    let stylesheet = parse_css(
        r#"
        .panel {
            max-width: 80%;
            max-height: 640px;
            margin: 8px;
            margin-x: 12px;
            margin-y: 4px;
            flex-basis: 50%;
            align-self: center;
            flex-wrap: wrap;
            border: 1px dashed rgba(12, 34, 56, 0.5);
            position: absolute;
            top: 10px;
            right: 20px;
            bottom: 30px;
            left: 40px;
            overflow: visible scroll;
            background: #abc;
            color: rgb(1, 2, 3);
            opacity: 0.5;
        }
        "#,
        CompileLimits::DEFAULT,
    )
    .unwrap();
    let declarations: Vec<_> = stylesheet.rules[0]
        .declarations
        .iter()
        .map(|declaration| declaration.property.clone())
        .collect();

    assert!(declarations.contains(&CssProperty::MaxWidth(CssLength::Percent(0.8))));
    assert!(declarations.contains(&CssProperty::MaxHeight(CssLength::Px(640.0))));
    assert!(declarations.contains(&CssProperty::Margin(CssLength::Px(8.0))));
    assert!(declarations.contains(&CssProperty::MarginX(CssLength::Px(12.0))));
    assert!(declarations.contains(&CssProperty::MarginY(CssLength::Px(4.0))));
    assert!(declarations.contains(&CssProperty::FlexBasis(CssLength::Percent(0.5))));
    assert!(declarations.contains(&CssProperty::AlignSelf(CssAlignItems::Center)));
    assert!(declarations.contains(&CssProperty::FlexWrap(CssFlexWrap::Wrap)));
    assert!(declarations.contains(&CssProperty::BorderWidth(CssLength::Px(1.0))));
    assert!(declarations.contains(&CssProperty::BorderStyle(CssBorderStyle::Dashed)));
    assert!(
        declarations.contains(&CssProperty::BorderColor(CssColor::Rgba {
            red: 12,
            green: 34,
            blue: 56,
            alpha: 0.5,
        }))
    );
    assert!(declarations.contains(&CssProperty::Position(CssPosition::Absolute)));
    assert!(declarations.contains(&CssProperty::Top(CssLength::Px(10.0))));
    assert!(declarations.contains(&CssProperty::Right(CssLength::Px(20.0))));
    assert!(declarations.contains(&CssProperty::Bottom(CssLength::Px(30.0))));
    assert!(declarations.contains(&CssProperty::Left(CssLength::Px(40.0))));
    assert!(declarations.contains(&CssProperty::OverflowX(CssOverflow::Visible)));
    assert!(declarations.contains(&CssProperty::OverflowY(CssOverflow::Scroll)));
    assert!(
        declarations.contains(&CssProperty::Background(CssColor::Rgb {
            red: 170,
            green: 187,
            blue: 204,
        }))
    );
    assert!(declarations.contains(&CssProperty::Color(CssColor::Rgb {
        red: 1,
        green: 2,
        blue: 3,
    })));
}

#[test]
fn css_parser_expands_shorthands_and_preserves_longhand_override() {
    let stylesheet = parse_css(
        ".panel { border: 2px solid #123456; border-color: transparent; overflow: scroll; }",
        CompileLimits::DEFAULT,
    )
    .unwrap();
    let declarations: Vec<_> = stylesheet.rules[0]
        .declarations
        .iter()
        .map(|declaration| declaration.property.clone())
        .collect();

    assert!(declarations.contains(&CssProperty::BorderColor(CssColor::Transparent)));
    assert!(declarations.contains(&CssProperty::OverflowX(CssOverflow::Scroll)));
    assert!(declarations.contains(&CssProperty::OverflowY(CssOverflow::Scroll)));
}

#[test]
fn css_parser_rejects_invalid_extension_values() {
    for source in [
        ".a { margin: -1px; }",
        ".a { gap: 10%; }",
        ".a { padding: 10%; }",
        ".a { font-size: 10%; }",
        ".a { border-width: 10%; }",
        ".a { color: rgba(1, 2, 3, 1.5); }",
        ".a { color: rgb(1, 2, 256); }",
        ".a { border-style: dotted; }",
        ".a { border: 1px dotted; }",
        ".a { overflow: hidden scroll visible; }",
    ] {
        assert!(
            parse_css(source, CompileLimits::DEFAULT).is_err(),
            "expected rejection: {source}"
        );
    }
}
