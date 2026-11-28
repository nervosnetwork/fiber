use std::path::{Path, PathBuf};

/// The core must not name the LSP module: tenancy is reached through
/// [`HostedDeliveryPort`]. Without this the port erodes one `crate::lsp::`
/// import at a time, and `lsp` can never become a feature.
#[test]
fn core_does_not_depend_on_the_lsp_module() {
    fn visit(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read core directory") {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                // Test modules may use LSP fixtures.
                if path.file_name().and_then(|name| name.to_str()) != Some("tests") {
                    visit(&path, out);
                }
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs")
                && !matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    // This test's own file, and the sibling test suites that
                    // sit next to a module instead of under `tests/`.
                    Some("delivery.rs" | "tests.rs")
                )
            {
                out.push(path);
            }
        }
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/fiber");
    let mut files = Vec::new();
    visit(&root, &mut files);
    assert!(
        !files.is_empty(),
        "no core sources under {}",
        root.display()
    );

    let offenders: Vec<String> = files
        .iter()
        .filter_map(|path| {
            let source = std::fs::read_to_string(path).expect("read core source");
            let code_reaches_lsp = source
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .any(|line| line.contains("crate::lsp"));
            code_reaches_lsp.then(|| {
                path.strip_prefix(&root)
                    .unwrap_or(path)
                    .display()
                    .to_string()
            })
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "these core files reach into crate::lsp; route them through \
         crate::fiber::delivery::HostedDeliveryPort instead: {offenders:?}"
    );
}
