use std::fs;

use declarative_ui_demo::{CompileOptions, ComponentRegistry, compile_template_with_style};

#[test]
fn reference_resource_plugin_templates_compile_in_strict_mode() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let examples = [
        "nacos",
        "elasticsearch",
        "rocketmq",
        "kafka",
        "docker",
        "kubernetes",
        "api-test",
    ];
    let registry = ComponentRegistry::with_defaults();

    for name in examples {
        let example_root = repo_root
            .join("docs/extension-resource-plugins/examples")
            .join(name);
        let path = example_root.join("ui/main.html");
        let source =
            fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let style_path = example_root.join("ui/main.css");
        let css_source = style_path.exists().then(|| {
            fs::read_to_string(&style_path)
                .unwrap_or_else(|error| panic!("{}: {error}", style_path.display()))
        });

        compile_template_with_style(
            &source,
            css_source.as_deref(),
            &registry,
            CompileOptions::strict(),
        )
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    }
}
