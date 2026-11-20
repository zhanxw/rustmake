use std::path::{Path, PathBuf};
use std::process::Command;

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

fn copy_tree(src: &Path, dst: &Path) {
    for entry in WalkDir::new(src).into_iter().flatten() {
        let rel = entry.path().strip_prefix(src).unwrap();
        if rel.as_os_str().is_empty() {
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

fn run_rustmake(cwd: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> std::process::Output {
    let bin = rustmake_bin();
    let mut cmd = Command::new(bin);
    cmd.args(args).current_dir(cwd);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.output().expect("failed to execute rustmake")
}

#[test]
fn example_c_workflow_build_and_clean() {
    let tmp = TempDir::new().unwrap();
    copy_tree(&repo_root().join("examples/c"), tmp.path());
    let src_dir = tmp.path().join("src");

    // 1. Dry-run and DAG
    let dry = run_rustmake(&src_dir, &["-n", "-p"], &[]);
    assert!(
        dry.status.success(),
        "examples/c dry-run failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&dry.stdout),
        String::from_utf8_lossy(&dry.stderr)
    );

    let dag = run_rustmake(&src_dir, &["--dag"], &[]);
    assert!(dag.status.success());
    let dag_out = String::from_utf8_lossy(&dag.stdout);
    assert!(dag_out.contains("digraph snakemake_dag"));

    // 2. Full build
    let build = run_rustmake(&src_dir, &["-c", "2", "-p"], &[]);
    assert!(
        build.status.success(),
        "examples/c build failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );
    let hello_bin = src_dir.join("hello");
    assert!(hello_bin.exists(), "hello binary was not created");

    let run_hello = Command::new(&hello_bin).current_dir(&src_dir).output().unwrap();
    assert!(run_hello.status.success());
    let hello_stdout = String::from_utf8_lossy(&run_hello.stdout);
    assert!(
        hello_stdout.contains("Hello makefiles"),
        "unexpected hello output: {hello_stdout}"
    );

    // 3. Incremental no-op second run
    let noop = run_rustmake(&src_dir, &["-c", "1"], &[]);
    assert!(noop.status.success());
    let noop_err = String::from_utf8_lossy(&noop.stderr);
    assert!(
        noop_err.contains("Nothing to be done"),
        "expected Nothing to be done, got:\n{noop_err}"
    );

    // 4. Clean rule (removes `*~`, `core`, `../include/*~`)
    let backup = src_dir.join("test_backup~");
    std::fs::write(&backup, "tmp").unwrap();
    let clean = run_rustmake(&src_dir, &["-c", "1", "clean"], &[]);
    assert!(
        clean.status.success(),
        "examples/c clean failed:\nstderr: {}",
        String::from_utf8_lossy(&clean.stderr)
    );
    assert!(!backup.exists(), "backup file should be removed by clean");
}

#[test]
fn example_flux_dryrun_and_dag() {
    let tmp = TempDir::new().unwrap();
    copy_tree(&repo_root().join("examples/flux"), tmp.path());

    let dry = run_rustmake(tmp.path(), &["-n", "-p"], &[]);
    assert!(
        dry.status.success(),
        "examples/flux dry-run failed:\nstderr: {}",
        String::from_utf8_lossy(&dry.stderr)
    );

    let dag = run_rustmake(tmp.path(), &["--dag"], &[]);
    assert!(dag.status.success());
    assert!(String::from_utf8_lossy(&dag.stdout).contains("digraph snakemake_dag"));
}

#[test]
fn example_cufflinks_dryrun_dag_rulegraph_and_touch() {
    let tmp = TempDir::new().unwrap();
    copy_tree(&repo_root().join("examples/cufflinks"), tmp.path());

    let dry = run_rustmake(tmp.path(), &["-n", "-p"], &[]);
    assert!(
        dry.status.success(),
        "examples/cufflinks dry-run failed:\nstderr: {}",
        String::from_utf8_lossy(&dry.stderr)
    );

    let dag = run_rustmake(tmp.path(), &["--dag"], &[]);
    assert!(dag.status.success());
    assert!(String::from_utf8_lossy(&dag.stdout).contains("digraph snakemake_dag"));

    let rg = run_rustmake(tmp.path(), &["--rulegraph"], &[]);
    assert!(rg.status.success());
    assert!(String::from_utf8_lossy(&rg.stdout).contains("digraph snakemake_dag"));

    // Touch mode execution in isolated tempdir
    let touch = run_rustmake(tmp.path(), &["-c", "1", "-t"], &[]);
    assert!(
        touch.status.success(),
        "examples/cufflinks touch failed:\nstderr: {}",
        String::from_utf8_lossy(&touch.stderr)
    );
    assert!(tmp.path().join("diffexp/isoform_exp.diff").exists());
}

#[test]
fn example_latex_dryrun_dag_and_pdflatex() {
    let tmp = TempDir::new().unwrap();
    copy_tree(&repo_root().join("examples/latex"), tmp.path());

    let dry = run_rustmake(tmp.path(), &["-n", "-p"], &[]);
    assert!(
        dry.status.success(),
        "examples/latex dry-run failed:\nstderr: {}",
        String::from_utf8_lossy(&dry.stderr)
    );

    let dag = run_rustmake(tmp.path(), &["--dag"], &[]);
    assert!(dag.status.success());
    assert!(String::from_utf8_lossy(&dag.stdout).contains("digraph snakemake_dag"));

    // If TeX toolchain exists on host, replace 0-byte placeholders in tempdir and run real build
    let pdflatex = Path::new("/Library/TeX/texbin/pdflatex");
    if pdflatex.exists() {
        std::fs::write(
            tmp.path().join("document.tex"),
            "\\documentclass{article}\n\\begin{document}\nHello Rustmake LaTeX.\\end{document}\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("response-to-editor.tex"),
            "\\documentclass{article}\n\\begin{document}\nResponse.\\end{document}\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("fig1.pdf"), "%PDF-1.4\n%%EOF\n").unwrap();

        let orig_path = std::env::var("PATH").unwrap_or_default();
        let tex_path = format!("/Library/TeX/texbin:{orig_path}");
        let res = run_rustmake(
            tmp.path(),
            &["-c", "1", "document.pdf"],
            &[("PATH", &tex_path)],
        );
        assert!(
            res.status.success(),
            "examples/latex pdflatex run failed:\nstderr: {}",
            String::from_utf8_lossy(&res.stderr)
        );
        assert!(tmp.path().join("document.pdf").exists());
    }
}

#[test]
fn example_hello_world_dryrun_dag_and_mock_e2e() {
    let tmp = TempDir::new().unwrap();
    copy_tree(&repo_root().join("examples/hello-world"), tmp.path());

    let dry = run_rustmake(tmp.path(), &["-n", "-p"], &[]);
    assert!(
        dry.status.success(),
        "examples/hello-world dry-run failed:\nstderr: {}",
        String::from_utf8_lossy(&dry.stderr)
    );

    let dag = run_rustmake(tmp.path(), &["--dag"], &[]);
    assert!(dag.status.success());
    assert!(String::from_utf8_lossy(&dag.stdout).contains("digraph snakemake_dag"));

    // Provide lightweight curl shim + matplotlib/pandas stubs in tempdir so download -> xsv -> script: runs offline
    let shim_bin = tmp.path().join("shim_bin");
    let pymod = tmp.path().join("pymod");
    std::fs::create_dir_all(&shim_bin).unwrap();
    std::fs::create_dir_all(pymod.join("matplotlib")).unwrap();
    std::fs::create_dir_all(pymod.join("pandas")).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let curl_path = shim_bin.join("curl");
        let curl_script = "#!/bin/sh\ncat << 'EOF'\nCountry,City,AccentCity,Region,Population,Latitude,Longitude\nfr,paris,Paris,A8,2138551,48.85341,2.3488\nat,vienna,Vienna,09,1691468,48.20849,16.37208\nEOF\n";
        std::fs::write(&curl_path, curl_script).unwrap();
        let mut perms = std::fs::metadata(&curl_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&curl_path, perms).unwrap();
    }

    std::fs::write(
        pymod.join("matplotlib/__init__.py"),
        "def use(backend): pass\n",
    )
    .unwrap();
    std::fs::write(
        pymod.join("matplotlib/pyplot.py"),
        "def hist(*args, **kwargs): pass\ndef savefig(p, *args, **kwargs):\n    import os\n    os.makedirs(os.path.dirname(p) or '.', exist_ok=True)\n    open(p, 'w').write('<svg></svg>')\n",
    )
    .unwrap();
    std::fs::write(
        pymod.join("pandas/__init__.py"),
        "import csv\nclass _Col(list):\n    def dropna(self): return self\nclass _DF:\n    def __init__(self, rows):\n        self._cols = {}\n        for r in rows:\n            for k, v in r.items():\n                self._cols.setdefault(k, _Col()).append(v)\n    def __getitem__(self, k): return self._cols.get(k, _Col())\n    def __getattr__(self, k): return self._cols.get(k, _Col())\ndef read_csv(p, *args, **kwargs):\n    with open(p) as f: return _DF(list(csv.DictReader(f)))\ndef read_table(p, *args, **kwargs):\n    return read_csv(p)\n",
    )
    .unwrap();

    let orig_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{orig_path}", shim_bin.display());
    let new_pypath = pymod.to_string_lossy().into_owned();

    let res = run_rustmake(
        tmp.path(),
        &["-c", "2", "-p", "--until", "plot_histogram"],
        &[("PATH", &new_path), ("PYTHONPATH", &new_pypath)],
    );
    assert!(
        res.status.success(),
        "examples/hello-world mock e2e failed:\nstderr: {}",
        String::from_utf8_lossy(&res.stderr)
    );
    assert!(tmp.path().join("by-country/fr.csv").exists());
    assert!(tmp.path().join("by-country/at.csv").exists());
    assert!(tmp.path().join("plots/fr.hist.svg").exists());
    assert!(tmp.path().join("plots/at.hist.svg").exists());
}
