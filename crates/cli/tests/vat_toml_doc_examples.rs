//! Every `vat.toml` example in the docs must still parse now that unknown keys
//! are rejected (faberline/vat#36).

use std::path::Path;

use vat::config::{parse_str, validate};

/// The bodies of the ```toml fences in `markdown`.
fn toml_blocks(markdown: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in markdown.lines() {
        let fence = line.trim_start();
        match current.as_mut() {
            None if fence.starts_with("```toml") => current = Some(Vec::new()),
            Some(_) if fence.starts_with("```") => {
                blocks.push(current.take().unwrap().join("\n"));
            }
            Some(lines) => lines.push(fence),
            None => {}
        }
    }
    blocks
}

fn check_examples(name: &str, markdown: &str) -> usize {
    let blocks = toml_blocks(markdown);
    for (index, block) in blocks.iter().enumerate() {
        let complete = block.lines().any(|line| line.starts_with("version ="));
        let text = if complete {
            block.clone()
        } else {
            format!("version = 1\n{block}")
        };
        let cfg = parse_str(&text)
            .unwrap_or_else(|err| panic!("{name} toml example #{}: {err:#}\n{block}", index + 1));
        if complete {
            validate(&cfg)
                .unwrap_or_else(|err| panic!("{name} toml example #{}: {err:#}", index + 1));
        }
    }
    blocks.len()
}

#[test]
fn readme_vat_toml_examples_parse() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let readme = std::fs::read_to_string(root.join("README.md")).unwrap();
    assert!(check_examples("README.md", &readme) >= 3);
}

#[test]
fn llm_guide_vat_toml_examples_parse() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_vat"))
        .args(["llm", "--topic", "guide"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let guide = String::from_utf8(output.stdout).unwrap();
    assert!(check_examples("vat llm", &guide) >= 1);
}
