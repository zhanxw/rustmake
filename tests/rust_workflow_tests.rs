use std::path::{Path, PathBuf};
use std::process::Command;

use md5::{Digest, Md5};
use tempfile::TempDir;
use walkdir::WalkDir;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rustmake_bin() -> PathBuf {
    let release = repo_root().join("target/release/rustmake");
    if release.exists() {
        return release;
    }
    let debug = repo_root().join("target/debug/rustmake");
    if debug.exists() {
        return debug;
    }
    PathBuf::from(env!("CARGO_BIN_EXE_rustmake"))
}

fn strip_ascii_ws(mut b: &[u8]) -> &[u8] {
    while let Some(&first) = b.first() {
        if matches!(first, b' ' | b'\t' | b'\r' | b'\n' | 0x0b | 0x0c) {
            b = &b[1..];
        } else {
            break;
        }
    }
    while let Some(&last) = b.last() {
        if matches!(last, b' ' | b'\t' | b'\r' | b'\n' | 0x0b | 0x0c) {
            b = &b[..b.len() - 1];
        } else {
            break;
        }
    }
    b
}

fn md5_stripped(bytes: &[u8]) -> String {
    let mut h = Md5::new();
    h.update(strip_ascii_ws(bytes));
    hex::encode(h.finalize())
}

fn copy_test_dir(src: &Path, dst: &Path) {
    for entry in WalkDir::new(src).into_iter().flatten() {
        let rel = entry.path().strip_prefix(src).unwrap();
        if rel.as_os_str().is_empty() {
            continue;
        }
        // Keep expected-results outside working tmpdir so globs/list-untracked aren't polluted
        if rel.components().next().is_some_and(|c| c.as_os_str() == "expected-results") {
            continue;
        }
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
        } else if entry.file_type().is_symlink() {
            let link_target = std::fs::read_link(entry.path()).unwrap();
            #[cfg(unix)]
            let _ = std::os::unix::fs::symlink(&link_target, &target);
        } else {
            if let Some(p) = target.parent() {
                std::fs::create_dir_all(p).unwrap();
            }
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

struct RunSpec<'a> {
    name: &'a str,
    cores: &'a str,
    targets: &'a [&'a str],
    extra_args: &'a [&'a str],
    extra_env: &'a [(&'a str, &'a str)],
    check_results: bool,
    check_md5: bool,
    shouldfail: bool,
}

impl<'a> RunSpec<'a> {
    fn simple(name: &'a str) -> Self {
        Self {
            name,
            cores: "3",
            targets: &[],
            extra_args: &[],
            extra_env: &[],
            check_results: true,
            check_md5: true,
            shouldfail: false,
        }
    }
}

fn run_case(spec: RunSpec<'_>) -> (TempDir, std::process::Output) {
    let test_src = repo_root().join("tests").join(spec.name);
    let tmp = TempDir::new().unwrap();
    copy_test_dir(&test_src, tmp.path());

    let out = invoke_in_dir(tmp.path(), &spec);

    if spec.shouldfail {
        assert!(
            !out.status.success(),
            "{}: expected failure but exited 0\nstdout: {}\nstderr: {}",
            spec.name,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    } else {
        assert!(
            out.status.success(),
            "{}: expected success but exited {:?}\nstdout: {}\nstderr: {}",
            spec.name,
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    if spec.check_results {
        let exp_dir = test_src.join("expected-results");
        if exp_dir.is_dir() {
            verify_expected(&exp_dir, tmp.path(), spec.name, spec.check_md5);
        }
    }

    (tmp, out)
}

fn invoke_in_dir(dir: &Path, spec: &RunSpec<'_>) -> std::process::Output {
    let bin = rustmake_bin();
    let mut cmd = Command::new(bin);
    cmd.current_dir(dir);
    cmd.args(["--cores", spec.cores]);
    cmd.args(spec.extra_args);
    cmd.args(spec.targets);
    for (k, v) in spec.extra_env {
        cmd.env(k, v);
    }
    cmd.output().expect("failed to execute rustmake")
}

fn verify_expected(exp_dir: &Path, actual_dir: &Path, test_name: &str, check_md5: bool) {
    for entry in WalkDir::new(exp_dir).into_iter().flatten() {
        if entry.file_type().is_dir() {
            continue;
        }
        let rel = entry.path().strip_prefix(exp_dir).unwrap();
        let fname = rel.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if rel
            .components()
            .any(|c| c.as_os_str().to_str().is_some_and(|s| s.starts_with('.')))
            || fname.ends_with("_WIN")
        {
            continue;
        }
        let actual_path = actual_dir.join(rel);
        assert!(
            std::fs::symlink_metadata(&actual_path).is_ok(),
            "{test_name}: missing expected output {}",
            rel.display()
        );
        if check_md5 {
            let exp_bytes = std::fs::read(entry.path()).unwrap();
            let act_bytes = std::fs::read(&actual_path).unwrap();
            let exp_h = md5_stripped(&exp_bytes);
            let act_h = md5_stripped(&act_bytes);
            assert_eq!(
                act_h,
                exp_h,
                "{test_name}: content MD5 mismatch for {}\nactual:\n{}\nexpected:\n{}",
                rel.display(),
                String::from_utf8_lossy(&act_bytes),
                String::from_utf8_lossy(&exp_bytes)
            );
        }
    }
}

#[test]
fn wf_test01() {
    run_case(RunSpec::simple("test01"));
}

#[test]
fn wf_test02() {
    run_case(RunSpec::simple("test02"));
}

#[test]
fn wf_test03() {
    run_case(RunSpec {
        targets: &["test.out"],
        ..RunSpec::simple("test03")
    });
}

#[test]
fn wf_test04() {
    run_case(RunSpec {
        targets: &["test.out"],
        ..RunSpec::simple("test04")
    });
}

#[test]
fn wf_test05_batch() {
    run_case(RunSpec {
        extra_args: &["--batch", "aggregate=1/2"],
        ..RunSpec::simple("test05")
    });
}

#[test]
fn wf_test06() {
    run_case(RunSpec {
        targets: &["test.bla.out"],
        ..RunSpec::simple("test06")
    });
}

#[test]
fn wf_test07() {
    run_case(RunSpec {
        targets: &["test.out", "test2.out"],
        ..RunSpec::simple("test07")
    });
}

#[test]
fn wf_test08() {
    run_case(RunSpec {
        targets: &["test.out", "test2.out"],
        ..RunSpec::simple("test08")
    });
}

#[test]
fn wf_test09_shouldfail() {
    run_case(RunSpec {
        check_results: false,
        shouldfail: true,
        ..RunSpec::simple("test09")
    });
}

#[test]
fn wf_test12() {
    run_case(RunSpec::simple("test12"));
}

#[test]
fn wf_test13() {
    run_case(RunSpec::simple("test13"));
}

#[test]
fn wf_test15() {
    run_case(RunSpec::simple("test15"));
}

#[test]
fn wf_list_untracked() {
    let (_, out) = run_case(RunSpec {
        extra_args: &["-s", "Snakefile_inner", "--list-untracked"],
        check_results: false,
        ..RunSpec::simple("test_list_untracked")
    });
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.trim(), "some_subdir/not_used");
}

#[test]
fn wf_delete_all_output() {
    let (tmp, _) = run_case(RunSpec {
        cores: "3",
        check_results: false,
        ..RunSpec::simple("test01")
    });
    assert!(tmp.path().join("test.inter").exists());
    let del = invoke_in_dir(
        tmp.path(),
        &RunSpec {
            cores: "3",
            extra_args: &["--delete-all-output"],
            ..RunSpec::simple("test01")
        },
    );
    assert!(del.status.success());
    assert!(!tmp.path().join("test.inter").exists());
}

#[test]
fn wf_params() {
    run_case(RunSpec::simple("test_params"));
}

#[test]
fn wf_same_wildcard() {
    run_case(RunSpec::simple("test_same_wildcard"));
}

#[test]
fn wf_conditional() {
    run_case(RunSpec {
        targets: &["test.out", "test.0.out", "test.1.out", "test.2.out"],
        ..RunSpec::simple("test_conditional")
    });
}

#[test]
fn wf_unpack_dict() {
    run_case(RunSpec::simple("test_unpack_dict"));
}

#[test]
fn wf_unpack_list() {
    run_case(RunSpec::simple("test_unpack_list"));
}

#[test]
fn wf_shell() {
    run_case(RunSpec::simple("test_shell"));
}

#[test]
fn wf_temp_and_no_temp() {
    run_case(RunSpec {
        targets: &["test.realigned.bam"],
        ..RunSpec::simple("test_temp")
    });
    run_case(RunSpec {
        extra_args: &["--notemp"],
        ..RunSpec::simple("test_no_temp")
    });
}

#[test]
fn wf_keyword_list() {
    run_case(RunSpec::simple("test_keyword_list"));
}

#[test]
fn wf_globwildcards() {
    run_case(RunSpec::simple("test_globwildcards"));
}

#[test]
fn wf_local_import() {
    run_case(RunSpec::simple("test_local_import"));
}

#[test]
fn wf_ruledeps() {
    run_case(RunSpec::simple("test_ruledeps"));
}

#[test]
fn wf_touch_and_touch_flag_dirs() {
    run_case(RunSpec {
        extra_args: &["--touch"],
        check_results: false,
        ..RunSpec::simple("test_touch")
    });
    run_case(RunSpec {
        extra_args: &["--touch"],
        ..RunSpec::simple("test_touch_with_directories")
    });
}

#[test]
fn wf_config_and_yaml() {
    run_case(RunSpec {
        extra_args: &["--config", "test=override"],
        ..RunSpec::simple("test_config")
    });
    run_case(RunSpec::simple("test_yaml_config"));
}

#[test]
fn wf_wildcard_keyword() {
    run_case(RunSpec::simple("test_wildcard_keyword"));
}

#[test]
fn wf_benchmark_self_contained() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("Snakefile"),
        r#"rule all:
    input: "out.txt"

rule work:
    output: "out.txt"
    benchmark: "bm.tsv"
    shell: "echo ok > {output}"

rule work_jsonl:
    output: "out2.txt"
    benchmark: "bm.jsonl"
    shell: "echo ok2 > {output}"
"#,
    )
    .unwrap();
    let res = invoke_in_dir(
        tmp.path(),
        &RunSpec {
            targets: &["out.txt", "out2.txt"],
            check_results: false,
            ..RunSpec::simple("benchmark_self")
        },
    );
    assert!(
        res.status.success(),
        "benchmark self-contained failed:\nstderr: {}",
        String::from_utf8_lossy(&res.stderr)
    );
    let tsv = std::fs::read_to_string(tmp.path().join("bm.tsv")).unwrap();
    assert!(tsv.lines().next().unwrap_or("").starts_with("s\t"));
    let jsonl = std::fs::read_to_string(tmp.path().join("bm.jsonl")).unwrap();
    assert!(jsonl.contains("\"s\""));
}

#[test]
fn wf_temp_expand() {
    run_case(RunSpec::simple("test_temp_expand"));
}

#[test]
fn wf_subpath() {
    run_case(RunSpec::simple("test_subpath"));
}

#[test]
fn wf_exists() {
    run_case(RunSpec {
        extra_args: &["-n"],
        check_results: false,
        ..RunSpec::simple("test_exists")
    });
}

#[test]
fn wf_includes() {
    run_case(RunSpec::simple("test_multiple_includes"));
    run_case(RunSpec::simple("test_empty_include"));
}

#[test]
fn wf_script_python() {
    run_case(RunSpec::simple("test_script_py"));
}

#[test]
fn wf_shadow_and_copy_and_prefix() {
    run_case(RunSpec::simple("test_shadow"));
    run_case(RunSpec::simple("test_shadow_copy"));
}

#[test]
fn wf_until_and_omitfrom() {
    run_case(RunSpec {
        extra_args: &["--until", "level2"],
        check_results: false,
        ..RunSpec::simple("test_until")
    });
    run_case(RunSpec {
        extra_args: &["--omit-from", "level2"],
        check_results: false,
        ..RunSpec::simple("test_omitfrom")
    });
}

#[test]
fn wf_nonstr_params() {
    run_case(RunSpec::simple("test_nonstr_params"));
}

#[test]
fn wf_delete_output() {
    run_case(RunSpec {
        extra_args: &["--delete-all-output"],
        check_results: false,
        ..RunSpec::simple("test_delete_output")
    });
}

#[test]
fn wf_input_generator() {
    run_case(RunSpec::simple("test_input_generator"));
}

#[test]
fn wf_spaces_in_fnames() {
    run_case(RunSpec {
        targets: &["test bam file realigned.bam"],
        ..RunSpec::simple("test_spaces_in_fnames")
    });
}

#[test]
fn wf_deferred_func_eval() {
    run_case(RunSpec::simple("test_deferred_func_eval"));
}

#[test]
fn wf_format_variants() {
    run_case(RunSpec::simple("test_format_params"));
    run_case(RunSpec::simple("test_format_wildcards"));
}

#[test]
fn wf_rule_defined_in_for_loop() {
    run_case(RunSpec::simple("test_rule_defined_in_for_loop"));
}

#[test]
fn wf_with_parentheses_and_dup_out() {
    run_case(RunSpec::simple("test (with parenthese's)"));
    run_case(RunSpec::simple("test_path with spaces"));
    run_case(RunSpec::simple("test_dup_out_patterns"));
}

#[test]
fn wf_threads_variants() {
    run_case(RunSpec {
        cores: "20",
        ..RunSpec::simple("test_threads")
    });
    let (tmp, out) = run_case(RunSpec {
        cores: "20",
        extra_args: &["--set-threads", "a=20"],
        check_results: false,
        ..RunSpec::simple("test_threads")
    });
    let _ = (tmp, out);
    run_case(RunSpec::simple("test_threads0"));
}

#[test]
fn wf_run_namedlist_and_bash() {
    run_case(RunSpec::simple("test_run_namedlist"));
    run_case(RunSpec::simple("test_bash"));
}

#[test]
fn wf_inoutput_is_path_and_log_input() {
    run_case(RunSpec::simple("test_inoutput_is_path"));
    run_case(RunSpec::simple("test_log_input"));
}

#[test]
fn wf_fstrings_and_pathlib() {
    run_case(RunSpec {
        targets: &["SID23454678.txt"],
        ..RunSpec::simple("test_fstring")
    });
    run_case(RunSpec {
        extra_args: &["-n"],
        check_results: false,
        ..RunSpec::simple("test_function_f_string")
    });
    run_case(RunSpec::simple("test_pathlib"));
}

#[test]
fn wf_priority_and_retries() {
    run_case(RunSpec {
        cores: "1",
        ..RunSpec::simple("test_priority")
    });
    run_case(RunSpec {
        extra_args: &["--retries", "2"],
        ..RunSpec::simple("test_retries")
    });
}

#[test]
fn wf_ensure_checks() {
    run_case(RunSpec {
        targets: &["b", "c"],
        check_results: false,
        ..RunSpec::simple("test_ensure")
    });
    run_case(RunSpec {
        targets: &["a"],
        check_results: false,
        shouldfail: true,
        ..RunSpec::simple("test_ensure")
    });
    run_case(RunSpec {
        targets: &["d"],
        check_results: false,
        shouldfail: true,
        ..RunSpec::simple("test_ensure")
    });
}

#[test]
fn wf_localrule_and_ancient() {
    run_case(RunSpec {
        targets: &["1.txt", "2.txt"],
        ..RunSpec::simple("test_localrule")
    });
    run_case(RunSpec {
        targets: &["D", "C", "old_file"],
        ..RunSpec::simple("test_ancient")
    });
}

#[test]
fn wf_checkpoints() {
    run_case(RunSpec::simple("test_checkpoints"));
    run_case(RunSpec::simple("test_checkpoints_many"));
    run_case(RunSpec::simple("test_temp_checkpoint"));
}

#[test]
fn wf_modules_and_inheritance() {
    run_case(RunSpec {
        targets: &["all"],
        ..RunSpec::simple("test_modules_all")
    });
    run_case(RunSpec {
        targets: &["test_a"],
        ..RunSpec::simple("test_modules_specific")
    });
    {
        let mp_src = repo_root().join("tests").join("test_modules_prefix");
        let mp_tmp = TempDir::new().unwrap();
        copy_test_dir(&mp_src, mp_tmp.path());
        let staged_http = mp_tmp
            .path()
            .join(".snakemake/storage/http/raw.githubusercontent.com/snakemake/snakemake/main/LICENSE.md");
        std::fs::create_dir_all(staged_http.parent().unwrap()).unwrap();
        std::fs::write(&staged_http, "MIT License\n").unwrap();
        let spec = RunSpec {
            targets: &["a"],
            ..RunSpec::simple("test_modules_prefix")
        };
        let out = invoke_in_dir(mp_tmp.path(), &spec);
        let _ = std::fs::remove_file("/tmp/foo.txt");
        assert!(
            out.status.success(),
            "test_modules_prefix failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        verify_expected(
            &mp_src.join("expected-results"),
            mp_tmp.path(),
            "test_modules_prefix",
            true,
        );
    }
    run_case(RunSpec {
        targets: &["out_1/test_final.txt"],
        ..RunSpec::simple("test_modules_prefix_local")
    });
    run_case(RunSpec {
        targets: &["foo.txt"],
        extra_args: &["--report", "report.html"],
        check_md5: false,
        ..RunSpec::simple("test_rule_inheritance_globals")
    });
}

#[test]
fn wf_pathvars() {
    run_case(RunSpec::simple("test_pathvars"));
    run_case(RunSpec::simple("test_pathvars_modules"));
    run_case(RunSpec {
        check_results: false,
        shouldfail: true,
        ..RunSpec::simple("test_pathvars_cycle")
    });
}

#[test]
fn wf_groups_and_directory() {
    run_case(RunSpec::simple("test_group_jobs"));
    run_case(RunSpec::simple("test_group_temp"));
    run_case(RunSpec {
        targets: &[
            "downstream",
            "symlinked_input",
            "child_to_input",
            "not_child_to_other",
            "some/dir-child",
            "some/shadow",
        ],
        ..RunSpec::simple("test_directory")
    });
    run_case(RunSpec {
        targets: &["file_expecting_dir"],
        check_results: false,
        shouldfail: true,
        ..RunSpec::simple("test_directory")
    });
    run_case(RunSpec {
        targets: &["dir_expecting_file"],
        check_results: false,
        shouldfail: true,
        ..RunSpec::simple("test_directory")
    });
    run_case(RunSpec {
        targets: &["child_to_other"],
        check_results: false,
        shouldfail: true,
        ..RunSpec::simple("test_directory")
    });
}

#[test]
fn wf_profiles_report_summary_archive_and_db() {
    run_case(RunSpec {
        extra_args: &["--profile", "dummy-general-profile"],
        ..RunSpec::simple("test_workflow_profile")
    });

    // Verify test_report parses cleanly (callable report category/labels) via dry-run without external shuf/curl,
    // then generate HTML report + custom stylesheet on a self-contained offline workflow.
    run_case(RunSpec {
        extra_args: &["--dry-run"],
        check_results: false,
        ..RunSpec::simple("test_report")
    });
    let rep_tmp = TempDir::new().unwrap();
    std::fs::write(
        rep_tmp.path().join("custom-stylesheet.css"),
        "body { background: #fafafa; }\n",
    )
    .unwrap();
    std::fs::write(
        rep_tmp.path().join("cap.rst"),
        "Test Report Caption\n===================\n\nGenerated offline.\n",
    )
    .unwrap();
    std::fs::write(
        rep_tmp.path().join("Snakefile"),
        r#"
rule all:
    input: "out.txt"

rule make_out:
    output:
        report("out.txt", caption="cap.rst", category=lambda w: "CustomCat", labels=lambda w, params: {"k": params.val})
    params:
        val="v1"
    shell:
        "echo hello > {output}"
"#,
    )
    .unwrap();
    let rep_exec = invoke_in_dir(
        rep_tmp.path(),
        &RunSpec {
            check_results: false,
            ..RunSpec::simple("test_report")
        },
    );
    assert!(
        rep_exec.status.success(),
        "offline report workflow failed: {}",
        String::from_utf8_lossy(&rep_exec.stderr)
    );
    let rep_out = invoke_in_dir(
        rep_tmp.path(),
        &RunSpec {
            extra_args: &[
                "--report",
                "report.html",
                "--report-stylesheet",
                "custom-stylesheet.css",
            ],
            check_results: false,
            ..RunSpec::simple("test_report")
        },
    );
    assert!(
        rep_out.status.success(),
        "--report report.html failed: {}",
        String::from_utf8_lossy(&rep_out.stderr)
    );
    let report_html = std::fs::read_to_string(rep_tmp.path().join("report.html")).unwrap();
    assert!(report_html.contains("background: #fafafa"));

    // ZIP report (matches tests/tests.py:305 — outputs are pre-committed in test_report_zip)
    let (zip_tmp, _) = run_case(RunSpec {
        extra_args: &["--report", "report.zip"],
        check_md5: false,
        ..RunSpec::simple("test_report_zip")
    });
    let zip_bytes = std::fs::read(zip_tmp.path().join("report.zip")).unwrap();
    assert!(zip_bytes.starts_with(b"PK\x03\x04"));

    // Directory report + test_report_dir_but_file failure check
    let (dir_tmp, _) = run_case(RunSpec {
        check_results: false,
        ..RunSpec::simple("test_report_dir")
    });
    let dir_out = invoke_in_dir(
        dir_tmp.path(),
        &RunSpec {
            extra_args: &["--report", "report_dir"],
            check_results: false,
            ..RunSpec::simple("test_report_dir")
        },
    );
    assert!(dir_out.status.success());
    assert!(dir_tmp.path().join("report_dir/report.html").is_file());

    std::fs::write(dir_tmp.path().join("existing_file"), "not a dir").unwrap();
    let dir_fail = invoke_in_dir(
        dir_tmp.path(),
        &RunSpec {
            extra_args: &["--report", "existing_file"],
            check_results: false,
            shouldfail: true,
            ..RunSpec::simple("test_report_dir")
        },
    );
    assert!(!dir_fail.status.success());

    // Summary & archive (.tar.gz) & SQLite DB backend
    run_case(RunSpec {
        extra_args: &["--summary"],
        check_results: false,
        ..RunSpec::simple("test01")
    });
    let (db_tmp, _) = run_case(RunSpec {
        extra_args: &["--persistence-backend", "db"],
        ..RunSpec::simple("test01")
    });
    assert!(db_tmp.path().join(".snakemake/metadata.db").exists());
    let arch_out = invoke_in_dir(
        db_tmp.path(),
        &RunSpec {
            extra_args: &["--archive", "workflow.tar.gz"],
            check_results: false,
            ..RunSpec::simple("test01")
        },
    );
    assert!(arch_out.status.success());
    let tar_gz = std::fs::read(db_tmp.path().join("workflow.tar.gz")).unwrap();
    assert_eq!(&tar_gz[..2], b"\x1f\x8b");
}

#[test]
fn wf_ambiguity_and_cycles() {
    run_case(RunSpec {
        check_results: false,
        shouldfail: true,
        ..RunSpec::simple("test_ambiguousruleexception")
    });
    run_case(RunSpec {
        check_results: false,
        shouldfail: true,
        ..RunSpec::simple("test_cyclic_dependency_single")
    });
    run_case(RunSpec {
        check_results: false,
        shouldfail: true,
        ..RunSpec::simple("test_cyclic_dependency_split")
    });
}
