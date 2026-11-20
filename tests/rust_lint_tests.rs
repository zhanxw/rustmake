use std::path::Path;

use rustmake::linting::{format_lint_report_plain, parse_for_test};
use rustmake::py_bridge::lint_workflow;

#[test]
fn test_all_linting_cases() {
    let lint_root = Path::new("tests/linting");
    let mut dirs: Vec<_> = std::fs::read_dir(lint_root)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    assert_eq!(dirs.len(), 12);

    for dir in dirs {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        for case in ["positive", "negative"] {
            let smk = dir.join(format!("{case}.smk"));
            assert!(smk.exists(), "missing {}", smk.display());

            let abs_dir = dir.canonicalize().unwrap();
            let abs_smk = smk.canonicalize().unwrap();

            let report = lint_workflow(&abs_smk, Some(&abs_dir))
                .unwrap_or_else(|e| panic!("lint_workflow failed for {name}/{case}: {e}"));
            let formatted = format_lint_report_plain(&report);

            if case == "positive" {
                assert!(
                    !report.linted,
                    "Expected clean lint for {name}/positive, got:\n{formatted}"
                );
                assert_eq!(
                    formatted,
                    "Congratulations, your workflow is in a good condition!"
                );
            } else {
                assert!(
                    report.linted,
                    "Expected lint warnings for {name}/negative, got none!"
                );
                if name.contains("not_used_params") {
                    assert!(
                        formatted.contains("line 3"),
                        "Expected 'line 3' in not_used_params output, got:\n{formatted}"
                    );
                }
            }
        }
    }
}

#[test]
fn test_parse_all_examples_and_sample_snakefiles() {
    let mut paths = vec![
        "examples/c/src/Snakefile",
        "examples/flux/Snakefile",
        "examples/latex/Snakefile",
        "examples/latex/tex.rules",
        "examples/cufflinks/Snakefile",
        "examples/hello-world/Snakefile",
        "tests/test01/Snakefile",
        "tests/test05/Snakefile",
        "tests/test_fstring/Snakefile",
        "tests/test_modules_all/Snakefile",
        "tests/test_modules_dynamic/Snakefile",
        "tests/test_parsing_terminal_comment_following_statement/Snakefile",
        "tests/test (with parenthese's)/Snakefile",
        "tests/test_checkpoints/Snakefile",
        "tests/test_checkpoints_many/Snakefile",
        "tests/test_unpack_dict/Snakefile",
        "tests/test_unpack_list/Snakefile",
    ];
    for p in paths.drain(..) {
        let src = std::fs::read_to_string(p).unwrap_or_else(|e| panic!("read {p}: {e}"));
        let parsed = parse_for_test(&src, p).unwrap_or_else(|e| panic!("parse {p}: {e}"));
        assert!(!parsed.is_empty(), "empty compilation for {p}");
    }
}
