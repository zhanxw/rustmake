#!/usr/bin/env python3
"""Standard-library unittest runner verifying the compiled `rustmake` binary
across CLI options, linting rules, example workflows, and local workflow tests.
"""

import hashlib
import json
import os
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
TESTS_DIR = REPO_ROOT / "tests"
EXAMPLES_DIR = REPO_ROOT / "examples"


def find_rustmake_bin() -> Path:
    for cand in (
        REPO_ROOT / "target" / "release" / "rustmake",
        REPO_ROOT / "target" / "debug" / "rustmake",
    ):
        if cand.is_file():
            return cand
    raise RuntimeError("rustmake binary not found; run `cargo build --release --offline` first")


RUSTMAKE = find_rustmake_bin()


def run_cmd(args, cwd, env=None):
    merged_env = os.environ.copy()
    if env:
        merged_env.update(env)
    return subprocess.run(
        [str(RUSTMAKE), *args],
        cwd=str(cwd),
        env=merged_env,
        capture_output=True,
        text=True,
    )


def copy_workflow_tree(src: Path, dst: Path, skip_expected: bool = True):
    for root, dirs, files in os.walk(src, followlinks=False):
        rel_root = Path(root).relative_to(src)
        if skip_expected and rel_root.parts and rel_root.parts[0] == "expected-results":
            continue
        if skip_expected and "expected-results" in dirs:
            dirs.remove("expected-results")
        target_dir = dst / rel_root
        target_dir.mkdir(parents=True, exist_ok=True)
        for d in dirs:
            sp = Path(root) / d
            dp = target_dir / d
            if sp.is_symlink():
                os.symlink(os.readlink(sp), dp)
        for f in files:
            sp = Path(root) / f
            dp = target_dir / f
            if sp.is_symlink():
                os.symlink(os.readlink(sp), dp)
            else:
                shutil.copy2(sp, dp)


def md5_stripped(path: Path) -> str:
    data = path.read_bytes().strip()
    return hashlib.md5(data).hexdigest()


class TestRustmakeCliAndGraphs(unittest.TestCase):
    def test_version_and_help(self):
        v = run_cmd(["--version"], REPO_ROOT)
        self.assertEqual(v.returncode, 0, v.stderr)
        self.assertIn("0.1.0", v.stdout)

        h = run_cmd(["--help"], REPO_ROOT)
        self.assertEqual(h.returncode, 0, h.stderr)
        self.assertIn("usage: rustmake", h.stdout)
        self.assertIn("--snakefile", h.stdout)
        self.assertIn("--persistence-backend", h.stdout)

    def test_graphs_list_summary_archive_report_unlock_db(self):
        with tempfile.TemporaryDirectory() as td:
            work = Path(td)
            copy_workflow_tree(TESTS_DIR / "test01", work)

            # list-rules / list-target-rules
            lr = run_cmd(["--list-rules"], work)
            self.assertEqual(lr.returncode, 0, lr.stderr)
            self.assertIn("rule1", lr.stdout)
            ltr = run_cmd(["--lt"], work)
            self.assertEqual(ltr.returncode, 0, ltr.stderr)

            # dag / rulegraph / filegraph / d3dag / mermaid-js
            for flag, needle in [
                (["--dag"], "digraph snakemake_dag"),
                (["--rulegraph"], "digraph snakemake_dag"),
                (["--filegraph"], "<TABLE"),
                (["--dag", "--print-dag-as", "mermaid-js"], "flowchart TB"),
            ]:
                res = run_cmd(flag, work)
                self.assertEqual(res.returncode, 0, res.stderr)
                self.assertIn(needle, res.stdout)

            d3 = run_cmd(["--d3dag"], work)
            self.assertEqual(d3.returncode, 0, d3.stderr)
            parsed = json.loads(d3.stdout)
            self.assertIn("nodes", parsed)
            self.assertIn("edges", parsed)

            # Summary before run
            sm = run_cmd(["--summary"], work)
            self.assertEqual(sm.returncode, 0, sm.stderr)
            self.assertIn("output_file\tdate\trule", sm.stdout)

            # Execute with sqlite DB backend
            ex = run_cmd(["-c", "3", "--persistence-backend", "db"], work)
            self.assertEqual(ex.returncode, 0, ex.stderr)
            self.assertTrue((work / "dir" / "test.out").is_file())
            self.assertTrue((work / ".snakemake" / "metadata.db").exists())

            # Detailed summary after run
            dsm = run_cmd(["-D", "--persistence-backend", "db"], work)
            self.assertEqual(dsm.returncode, 0, dsm.stderr)
            self.assertIn("shellcmd\tstatus\tplan", dsm.stdout)

            # Archive .tar.gz
            arc_path = work / "bundle.tar.gz"
            arc = run_cmd(["--archive", str(arc_path)], work)
            self.assertEqual(arc.returncode, 0, arc.stderr)
            self.assertTrue(arc_path.is_file())
            with tarfile.open(arc_path, "r:gz") as tf:
                names = tf.getnames()
                self.assertTrue(any("Snakefile" in n for n in names), names)

            # Report HTML & ZIP & dir
            r_html = run_cmd(["--report", str(work / "rep.html")], work)
            self.assertEqual(r_html.returncode, 0, r_html.stderr)
            self.assertIn("Snakemake / rustmake Workflow Report", (work / "rep.html").read_text())

            r_zip = run_cmd(["--report", str(work / "rep.zip")], work)
            self.assertEqual(r_zip.returncode, 0, r_zip.stderr)
            with zipfile.ZipFile(work / "rep.zip", "r") as zf:
                self.assertIn("report.html", zf.namelist())

            r_dir = run_cmd(["--report", str(work / "rep_dir")], work)
            self.assertEqual(r_dir.returncode, 0, r_dir.stderr)
            self.assertTrue((work / "rep_dir" / "report.html").is_file())

            # Unlock
            ul = run_cmd(["--unlock"], work)
            self.assertEqual(ul.returncode, 0, ul.stderr)

            # Cleanup metadata error on nonexistent file, then valid cleanup on existing
            cm_bad = run_cmd(["--cleanup-metadata", "no_such_output.txt"], work)
            self.assertNotEqual(cm_bad.returncode, 0)


class TestRustmakeLinting(unittest.TestCase):
    def test_all_lint_suites(self):
        lint_root = TESTS_DIR / "linting"
        dirs = sorted(d for d in lint_root.iterdir() if d.is_dir())
        self.assertGreaterEqual(len(dirs), 12)
        for ldir in dirs:
            pos = ldir / "positive.smk"
            neg = ldir / "negative.smk"
            if pos.exists():
                res = run_cmd(["-s", str(pos), "--lint", "json"], REPO_ROOT)
                self.assertEqual(
                    res.returncode,
                    0,
                    f"{ldir.name}/positive.smk should pass lint, got: {res.stdout} {res.stderr}",
                )
            if neg.exists():
                res = run_cmd(["-s", str(neg), "--lint", "json"], REPO_ROOT)
                self.assertNotEqual(
                    res.returncode,
                    0,
                    f"{ldir.name}/negative.smk should report lints, got clean",
                )
                payload = json.loads(res.stdout)
                self.assertTrue(payload.get("snakefiles") or payload.get("rules"))


class TestRustmakeExamples(unittest.TestCase):
    def test_example_c(self):
        with tempfile.TemporaryDirectory() as td:
            work = Path(td)
            copy_workflow_tree(EXAMPLES_DIR / "c", work, skip_expected=False)
            src = work / "src"
            res = run_cmd(["-c", "2", "-p"], src)
            self.assertEqual(res.returncode, 0, res.stderr)
            hello_out = subprocess.run([str(src / "hello")], cwd=str(src), capture_output=True, text=True)
            self.assertEqual(hello_out.returncode, 0)
            self.assertIn("Hello makefiles", hello_out.stdout)

    def test_example_flux(self):
        with tempfile.TemporaryDirectory() as td:
            work = Path(td)
            copy_workflow_tree(EXAMPLES_DIR / "flux", work, skip_expected=False)
            self.assertEqual(run_cmd(["-n"], work).returncode, 0)
            dag = run_cmd(["--dag"], work)
            self.assertEqual(dag.returncode, 0)
            self.assertIn("digraph snakemake_dag", dag.stdout)

    def test_example_cufflinks(self):
        with tempfile.TemporaryDirectory() as td:
            work = Path(td)
            copy_workflow_tree(EXAMPLES_DIR / "cufflinks", work, skip_expected=False)
            self.assertEqual(run_cmd(["-n"], work).returncode, 0)
            self.assertEqual(run_cmd(["-c", "1", "-t"], work).returncode, 0)
            self.assertTrue((work / "diffexp" / "isoform_exp.diff").exists())

    def test_example_latex(self):
        with tempfile.TemporaryDirectory() as td:
            work = Path(td)
            copy_workflow_tree(EXAMPLES_DIR / "latex", work, skip_expected=False)
            res = run_cmd(["-n"], work)
            self.assertEqual(res.returncode, 0, res.stderr)

    def test_example_hello_world(self):
        with tempfile.TemporaryDirectory() as td:
            work = Path(td)
            copy_workflow_tree(EXAMPLES_DIR / "hello-world", work, skip_expected=False)
            self.assertEqual(run_cmd(["-n"], work).returncode, 0)


class TestRustmakeWorkflows(unittest.TestCase):
    def _assert_workflow(self, name, args=None, targets=None, check_md5=True):
        args = args or ["-c", "3"]
        targets = targets or []
        src = TESTS_DIR / name
        with tempfile.TemporaryDirectory() as td:
            work = Path(td)
            copy_workflow_tree(src, work, skip_expected=True)
            res = run_cmd([*args, *targets], work)
            self.assertEqual(
                res.returncode,
                0,
                f"{name} failed:\nstdout: {res.stdout}\nstderr: {res.stderr}",
            )
            exp_dir = src / "expected-results"
            if exp_dir.is_dir():
                for root, _, files in os.walk(exp_dir):
                    for fn in files:
                        exp_file = Path(root) / fn
                        rel = exp_file.relative_to(exp_dir)
                        if any(p.startswith(".") for p in rel.parts) or fn.endswith("_WIN"):
                            continue
                        act_file = work / rel
                        self.assertTrue(act_file.exists() or act_file.is_symlink(), f"{name}: missing {rel}")
                        if check_md5:
                            self.assertEqual(
                                md5_stripped(act_file),
                                md5_stripped(exp_file),
                                f"{name}: MD5 mismatch for {rel}",
                            )

    def test_core_workflows(self):
        self._assert_workflow("test01")
        self._assert_workflow("test02")
        self._assert_workflow("test03", targets=["test.out"])
        self._assert_workflow("test04", targets=["test.out"])
        self._assert_workflow("test05", args=["-c", "3", "--batch", "aggregate=1/2"])
        self._assert_workflow("test06", targets=["test.bla.out"])
        self._assert_workflow("test07", targets=["test.out", "test2.out"])
        self._assert_workflow("test08", targets=["test.out", "test2.out"])
        self._assert_workflow("test12")
        self._assert_workflow("test13")
        self._assert_workflow("test15")

    def test_advanced_workflows(self):
        self._assert_workflow("test_checkpoints")
        self._assert_workflow("test_checkpoints_many")
        self._assert_workflow("test_temp_checkpoint")
        self._assert_workflow("test_modules_all", targets=["all"])
        self._assert_workflow("test_modules_specific", targets=["test_a"])
        self._assert_workflow("test_pathvars")
        self._assert_workflow("test_group_jobs")
        self._assert_workflow("test_shadow")
        self._assert_workflow("test_ruledeps")
        self._assert_workflow("test_script_py")
        self._assert_workflow("test_log_input")
        self._assert_workflow("test_wildcard_keyword")
        self._assert_workflow("test_deferred_func_eval")
        self._assert_workflow("test_input_generator")
        self._assert_workflow("test_priority", args=["-c", "1"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
