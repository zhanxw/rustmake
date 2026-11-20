import collections
from collections import OrderedDict, namedtuple
from collections.abc import Iterable, Mapping
import copy
from datetime import datetime
from functools import partial
import glob as _glob
import hashlib
import inspect
import itertools
from itertools import chain, filterfalse, product
import json
import os
from pathlib import Path, PosixPath
import platform
import re
import shlex
import shutil
import stat
import string
import subprocess
import sys
import tarfile
import tempfile
import threading
import traceback
import types

# Isolate RPC communication pipes from user print() / subprocess stdout!
_RPC_OUT_FD = os.dup(1)
_RPC_IN_FD = os.dup(0)
os.dup2(2, 1)
_RPC_OUT = os.fdopen(_RPC_OUT_FD, "w", buffering=1, encoding="utf-8")
_RPC_IN = os.fdopen(_rpc_in_fd := _RPC_IN_FD, "r", encoding="utf-8")
sys.stdout = sys.stderr
if "" not in sys.path:
    sys.path.insert(0, "")


def _rpc_send(obj):
    _RPC_OUT.write(json.dumps(obj) + "\n")
    _RPC_OUT.flush()


def _rpc_recv():
    line = _RPC_IN.readline()
    if not line:
        raise EOFError("RPC pipe closed")
    return json.loads(line)


def _call_rust(req_dict):
    _rpc_send(req_dict)
    resp = _rpc_recv()
    if resp.get("error"):
        raise WorkflowError(resp["error"])
    return resp


# ---- Exceptions ----

class WorkflowError(Exception):
    def __init__(self, *args, rule=None, lineno=None, snakefile=None):
        msg = "\n".join(str(a) for a in args if a is not None)
        super().__init__(msg)
        self.rule = rule
        self.lineno = lineno
        self.snakefile = snakefile


class WildcardError(WorkflowError):
    pass


class InputFunctionException(WorkflowError):
    pass


class CreateRuleException(WorkflowError):
    pass


class RuleException(Exception):
    def __init__(self, msg=None, include=None, lineno=None, snakefile=None, rule=None):
        super().__init__(str(msg) if msg is not None else "")
        self.rule = rule
        self.lineno = lineno
        self.snakefile = snakefile


class SourceFileError(WorkflowError):
    pass


class IncompleteCheckpointException(Exception):
    def __init__(self, rule, targetfile):
        self.rule = rule
        self.targetfile = targetfile
        super().__init__(f"Incomplete checkpoint {rule}: {targetfile}")


class UndefinedPathvarException(WorkflowError):
    pass


class LookupError(WorkflowError):
    def __init__(self, query=None, dpath=None, msg=None, exc=None):
        detail = msg or (str(exc) if exc else "Lookup failed")
        super().__init__(detail)


class AmbiguousRuleException(RuleException):
    pass


class MissingInputException(RuleException):
    pass


class MissingOutputException(RuleException):
    pass


class ProtectedOutputException(RuleException):
    pass


# ---- Wildcard & Formatting Helpers ----

WILDCARD_REGEX = re.compile(
    r"\{\s*(?P<name>\w+?)\s*(?:,\s*(?P<constraint>(?:[^{}]+|\{\d+(?:,\d*)?\})*))?\}"
)


def _not_iterable(x):
    return isinstance(x, (str, bytes, Path)) or not isinstance(x, Iterable)


class AnnotatedString(str):
    def __new__(cls, value=""):
        if isinstance(value, Path):
            value = str(value.as_posix())
        obj = super().__new__(cls, "" if callable(value) else str(value))
        obj._flags = dict(getattr(value, "flags", {})) if hasattr(value, "flags") else {}
        obj.callable = value if callable(value) else getattr(value, "callable", None)
        return obj

    @property
    def flags(self):
        return self._flags

    @flags.setter
    def flags(self, value):
        self._flags = dict(value)

    def is_callable(self):
        return self.callable is not None

    def is_flagged(self, name):
        return name in self._flags

    def format(self, *args, **kwargs):
        if "storage_object" in self._flags:
            raise WorkflowError(
                "Cannot call .format() on a storage-flagged AnnotatedString."
            )
        formatted = super().format(*args, **kwargs)
        if not self._flags:
            return formatted
        res = AnnotatedString(formatted)
        res._flags = dict(self._flags)
        res.callable = self.callable
        return res


class _IOFile(str):
    def __new__(cls, file, rule=None):
        raw_str = str(file.as_posix()) if isinstance(file, Path) else str(file)
        obj = super().__new__(cls, raw_str)
        obj._file = file
        obj.rule = rule
        obj._flags = dict(getattr(file, "flags", {})) if hasattr(file, "flags") else {}
        return obj

    @property
    def flags(self):
        return self._flags

    def is_flagged(self, name):
        return name in self._flags

    def is_callable(self):
        return callable(self._file) or (
            isinstance(self._file, AnnotatedString) and self._file.is_callable()
        )

    def open(self, mode="r", *args, **kwargs):
        return open(str(self), mode, *args, **kwargs)

    @property
    def size(self):
        return os.path.getsize(str(self))

    @property
    def size_mb(self):
        return self.size / 1024 / 1024

    @property
    def size_mib(self):
        return self.size / 1048576

    def mtime(self):
        return os.stat(str(self)).st_mtime

    def exists(self):
        return os.path.exists(str(self))

    def apply_wildcards(self, wildcards):
        pattern = str(self)

        def repl(m):
            name = m.group("name")
            if name not in wildcards:
                raise WildcardError(f"Wildcard '{name}' not found")
            return str(wildcards[name])

        new_val = WILDCARD_REGEX.sub(repl, pattern)
        ann = AnnotatedString(new_val)
        ann._flags = dict(self._flags)
        return _IOFile(ann, rule=self.rule)

    def format(self, *args, **kwargs):
        if "storage_object" in self._flags:
            raise WorkflowError(
                "Cannot call .format() on a storage-flagged _IOFile. Use .apply_wildcards() instead."
            )
        formatted = super().format(*args, **kwargs)
        if not self._flags:
            return formatted
        ann = AnnotatedString(formatted)
        ann._flags = dict(self._flags)
        return _IOFile(ann, rule=self.rule)


def IOFile(file, rule=None):
    return _IOFile(file, rule=rule)


def is_flagged(value, flag_name):
    if hasattr(value, "is_flagged"):
        return value.is_flagged(flag_name)
    if hasattr(value, "flags") and isinstance(value.flags, dict):
        return flag_name in value.flags
    return False


def get_flag_value(value, flag_name):
    flags = getattr(value, "flags", None)
    if isinstance(flags, dict):
        return flags.get(flag_name)
    return None


def flag(value, flag_type, flag_value=True):
    if isinstance(value, (AnnotatedString, _IOFile)):
        value.flags[flag_type] = flag_value
        return value
    if callable(value):
        ann = AnnotatedString(value)
        ann.flags[flag_type] = flag_value
        return ann
    if _not_iterable(value):
        ann = AnnotatedString(value)
        ann.flags[flag_type] = flag_value
        return ann
    return [flag(v, flag_type, flag_value=flag_value) for v in value]


def temp(value, group_jobs=False):
    if is_flagged(value, "protected"):
        raise SyntaxError("Protected and temporary flags are mutually exclusive.")
    if group_jobs:
        value = flag(value, "nodelocal")
    return flag(value, "temp")


def temporary(value):
    return temp(value)


def protected(value):
    if is_flagged(value, "temp"):
        raise SyntaxError("Protected and temporary flags are mutually exclusive.")
    return flag(value, "protected")


def directory(value):
    if is_flagged(value, "pipe"):
        raise SyntaxError("Pipe and directory flags are mutually exclusive.")
    return flag(value, "directory")


def ancient(value):
    return flag(value, "ancient")


def touch(value):
    return flag(value, "touch")


def pipe(value):
    if is_flagged(value, "protected"):
        raise SyntaxError("Pipes may not be protected.")
    return flag(value, "pipe")


def service(value):
    if is_flagged(value, "protected"):
        raise SyntaxError("Service may not be protected.")
    return flag(value, "service")


def local(value):
    return flag(value, "local")


def unpack(value):
    return flag(value, "unpack")


def repeat(value, n_repeat):
    return flag(value, "repeat", n_repeat)


def checkpoint_target(value):
    return flag(value, "checkpoint_target")


def ensure(value, non_empty=False, sha256=None, md5=None, sha1=None):
    checksum = sha256 or md5 or sha1
    algo = (
        "sha256" if sha256 is not None else "md5" if md5 is not None else "sha1" if sha1 is not None else None
    )
    return flag(
        value,
        "ensure",
        {"non_empty": non_empty, "checksum": checksum, "checksum_algorithm": algo},
    )


ReportObject = namedtuple(
    "ReportObject", ["caption", "category", "subcategory", "labels", "patterns", "htmlindex"]
)


def report(value, caption=None, category=None, subcategory=None, labels=None, patterns=None, htmlindex=None):
    return flag(
        value,
        "report",
        {"caption": caption, "category": category, "subcategory": subcategory, "labels": labels, "patterns": patterns or [], "htmlindex": htmlindex},
    )


def sourcecache_entry(value, orig_path_or_uri):
    return flag(value, "sourcecache_entry", orig_path_or_uri)


class MultiextValue:
    def __init__(self, prefix, name=None):
        self.prefix = prefix
        self.name = name


def multiext(prefix, *extensions, **named_extensions):
    if any(("/" in ext or "\\" in ext) for ext in chain(extensions, named_extensions.values())):
        raise WorkflowError(r"Extensions for multiext may not contain path delimiters (/,\).")
    if not ((extensions and not named_extensions) or (not extensions and named_extensions)):
        raise WorkflowError("multiext should be given with all named or all positional extensions.")
    if extensions:
        return [flag(prefix + ext, "multiext", flag_value=MultiextValue(prefix=prefix)) for ext in extensions]
    return [
        flag(prefix + ext, "multiext", flag_value=MultiextValue(name=name, prefix=prefix))
        for name, ext in named_extensions.items()
    ]


def get_wildcard_names(pattern):
    return {m.group("name") for m in WILDCARD_REGEX.finditer(str(pattern))}


def apply_wildcards(pattern, wildcards):
    def repl(m):
        name = m.group("name")
        if name not in wildcards:
            raise WildcardError(f"'{name}'")
        return str(wildcards[name])

    return WILDCARD_REGEX.sub(repl, str(pattern))


def strip_wildcard_constraints(pattern):
    if callable(pattern) or (isinstance(pattern, AnnotatedString) and pattern.is_callable()):
        return pattern
    return WILDCARD_REGEX.sub(lambda m: f"{{{m.group('name')}}}", str(pattern))


def get_function_params(func):
    try:
        return inspect.signature(func).parameters
    except (ValueError, TypeError):
        return {}


def get_input_function_aux_params(func, candidate_params):
    fp = get_function_params(func)
    if any(p.kind == inspect.Parameter.VAR_KEYWORD for p in fp.values()):
        return candidate_params
    return {k: v for k, v in candidate_params.items() if k in fp}


def overwrite_function_params(func, params):
    sig = inspect.signature(func)
    new_params = [inspect.Parameter("wildcards", inspect.Parameter.POSITIONAL_OR_KEYWORD)]
    for name, p in params.items():
        if name != "wildcards":
            new_params.append(p)
    func.__signature__ = sig.replace(parameters=new_params)


# ---- Namedlist & IOContainers ----

class Namedlist(list):
    def __init__(self, toclone=None, fromdict=None):
        super().__init__()
        self._names = {}
        if toclone is not None:
            self.extend(toclone)
            if isinstance(toclone, Namedlist):
                self._names = dict(toclone._names)
        if fromdict is not None:
            for k, v in fromdict.items():
                start = len(self)
                if isinstance(v, list):
                    self.extend(v)
                    self._names[k] = (start, len(self))
                else:
                    self.append(v)
                    self._names[k] = (start, None)

    def _set_name(self, name, start, end=None):
        self._names[name] = (start, end)

    def _allitems(self):
        named_indices = {}
        for name, (start, end) in self._names.items():
            named_indices[start] = (name, end)
        i = 0
        while i < len(self):
            if i in named_indices:
                name, end = named_indices[i]
                if end is not None:
                    yield name, list(self[i:end])
                    i = end
                else:
                    yield name, self[i]
                    i += 1
            else:
                yield None, self[i]
                i += 1

    def keys(self):
        return self._names.keys()

    def items(self):
        for name in self._names:
            yield name, getattr(self, name)

    def get(self, key, default=None):
        if key in self._names:
            return getattr(self, key)
        return default

    def __getattr__(self, name):
        if name.startswith("_"):
            raise AttributeError(name)
        if name in self._names:
            start, end = self._names[name]
            if end is None:
                return self[start]
            return Namedlist(self[start:end])
        raise AttributeError(f"'{type(self).__name__}' object has no attribute '{name}'")

    def __getitem__(self, key):
        if isinstance(key, str):
            return getattr(self, key)
        res = super().__getitem__(key)
        if isinstance(key, slice):
            return Namedlist(res)
        return res

    def __str__(self):
        return " ".join(str(x) for x in self)

    @property
    def size(self):
        return sum(os.path.getsize(str(f)) for f in self)

    @property
    def size_mb(self):
        return self.size / 1024 / 1024

    @property
    def size_mib(self):
        return self.size / 1048576

    @property
    def size_kb(self):
        return self.size / 1024


class InputFiles(Namedlist):
    pass


class OutputFiles(Namedlist):
    pass


class Params(Namedlist):
    pass


class Log(Namedlist):
    pass


class Wildcards(Namedlist):
    pass


class Resources(Namedlist):
    pass


# ---- Sequence/Quoted Formatters & shell() ----

class SequenceFormatter(string.Formatter):
    def __init__(self, separator=" ", element_formatter=None):
        self.separator = separator
        self.element_formatter = element_formatter or QuotedFormatter()

    def format_field(self, value, format_spec):
        if isinstance(value, Wildcards):
            return ",".join(
                f"{name}={val}"
                for name, val in sorted(value.items(), key=lambda item: item[0])
            )
        if isinstance(value, (list, tuple, set, frozenset)):
            return self.separator.join(
                self.element_formatter.format_field(v, format_spec) for v in value
            )
        return self.element_formatter.format_field(value, format_spec)


class QuotedFormatter(string.Formatter):
    def format_field(self, value, format_spec):
        if format_spec.endswith("u"):
            format_spec = format_spec[:-1]
            do_quote = False
        else:
            do_quote = format_spec.endswith("q")
            if do_quote:
                format_spec = format_spec[:-1]
        formatted = super().format_field(value, format_spec)
        if do_quote and formatted != "":
            formatted = shlex.quote(formatted)
        return formatted


def snakemake_format(_pattern, *args, stepout=1, _quote_all=False, **kwargs):
    frame = inspect.currentframe().f_back
    while stepout > 1 and frame.f_back:
        frame = frame.f_back
        stepout -= 1
    variables = dict(frame.f_globals)
    variables.update(frame.f_locals)
    variables.update(kwargs)
    fmt = SequenceFormatter(separator=" ")
    try:
        return fmt.format(_pattern, *args, **variables)
    except KeyError as ex:
        key = str(ex).strip("'")
        if "wildcards" in variables and hasattr(variables["wildcards"], "keys") and key in variables["wildcards"].keys():
            raise NameError(f"The name '{key}' is unknown in this context. Did you mean 'wildcards.{key}'?")
        raise NameError(
            f"The name {ex} is unknown in this context. Please make sure that you defined that variable."
        )


class _ShellRunner:
    def __init__(self):
        self._executable = None
        self._prefix = ""
        self._suffix = ""

    def executable(self, exec_path):
        self._executable = exec_path

    def prefix(self, p):
        self._prefix = p

    def suffix(self, s):
        self._suffix = s

    def get_executable(self):
        return self._executable

    def _effective_exec(self):
        if self._executable:
            return self._executable
        return shutil.which("bash") or "/bin/sh"

    def __call__(self, cmd, *args, read=False, iterable=False, async_run=False, bench_record=None, **kwargs):
        if "stepout" not in kwargs:
            kwargs["stepout"] = 2
        formatted = snakemake_format(cmd, *args, **kwargs)
        full_cmd = f"{self._prefix}{formatted}{self._suffix}"
        exe = self._effective_exec()
        base = os.path.basename(exe)
        if base in ("bash", "zsh"):
            argv = [exe, "-euo", "pipefail", "-c", full_cmd]
        elif base in ("sh", "dash", "ash"):
            argv = [exe, "-eu", "-c", full_cmd]
        else:
            argv = [exe, "-c", full_cmd]

        if read:
            out = subprocess.check_output(argv)
            return out.decode("utf-8", errors="replace")
        if iterable:
            out = subprocess.check_output(argv).decode("utf-8", errors="replace")
            return iter(out.splitlines())
        res = subprocess.run(argv)
        if res.returncode != 0:
            raise subprocess.CalledProcessError(res.returncode, full_cmd)
        return None


shell = _ShellRunner()


# ---- expand / collect / ioutils ----

def expand(*args, **wildcard_values):
    filepatterns = args[0]
    combinator = product if len(args) == 1 else args[1]
    if isinstance(filepatterns, (str, Path)):
        filepatterns = [filepatterns]
    filepatterns = [str(f) if isinstance(f, Path) else f for f in filepatterns]

    for fp in filepatterns:
        fp_flags = {
            k: v
            for k, v in getattr(fp, "flags", {}).items()
            if k not in ("path_modified", "from_rule")
        }
        if fp_flags:
            raise WorkflowError(f"Flags ({fp_flags}) in file pattern '{fp}' given to expand() are invalid.")

    format_dict = dict
    if wildcard_values.get("allow_missing") is True:
        class FormatDict(dict):
            def __missing__(self, key):
                return "{" + key + "}"
        format_dict = FormatDict
        for fp in filepatterns:
            if "allow_missing" in re.findall(r"\{([^}\.[!:]+)", str(fp)):
                format_dict = dict
                break

    callables = {
        k: v for k, v in wildcard_values.items()
        if callable(v) or (isinstance(v, list) and any(callable(x) for x in v))
    }

    per_pat = {
        fp: {k: v for k, v in wildcard_values.items() if k in re.findall(r"\{([^}\.[!:]+)", str(fp))}
        for fp in filepatterns
    }

    def do_expand(wv_map):
        def flatten_wv(wv):
            for wc, val in wv.items():
                if isinstance(val, str) or not isinstance(val, Iterable) or (isinstance(val, tuple) and hasattr(val, "_fields")):
                    vals = [val]
                else:
                    vals = val
                yield [(wc, v) for v in vals]

        def copy_flags(from_path, dest_path):
            if hasattr(from_path, "flags") and from_path.flags:
                ann = AnnotatedString(dest_path)
                ann.flags.update(from_path.flags)
                return ann
            return dest_path

        formatter = string.Formatter()
        try:
            return [
                copy_flags(fp, formatter.vformat(str(fp), (), comb))
                for fp in filepatterns
                for comb in map(format_dict, combinator(*flatten_wv(wv_map[fp])))
            ]
        except KeyError as e:
            raise WildcardError(f"No values given for wildcard {e}.")

    if callables:
        def inner(wildcards, **aux_params):
            for wc, fn_or_list in callables.items():
                if callable(fn_or_list):
                    ret = fn_or_list(wildcards, **get_input_function_aux_params(fn_or_list, aux_params))
                else:
                    ret = [
                        (f(wildcards, **get_input_function_aux_params(f, aux_params)) if callable(f) else f)
                        for f in fn_or_list
                    ]
                for pv in per_pat.values():
                    pv[wc] = ret
            return do_expand(per_pat)
        return inner
    return do_expand(per_pat)


collect = expand


def glob_wildcards(pattern, files=None, followlinks=False):
    pattern = os.path.normpath(str(pattern))
    first_wc = re.search(r"\{[^{]", pattern)
    dirname = os.path.dirname(pattern[:first_wc.start()]) if first_wc else os.path.dirname(pattern)
    if not dirname:
        dirname = "."
    _names = [m.group("name") for m in WILDCARD_REGEX.finditer(pattern)]
    names = sorted(set(_names), key=_names.index)
    WcTuple = namedtuple("Wildcards", names)
    wc_res = WcTuple(*[list() for _ in names])

    parts = ["^"]
    last = 0
    seen = set()
    for m in WILDCARD_REGEX.finditer(pattern):
        parts.append(re.escape(pattern[last:m.start()]))
        wname = m.group("name")
        if wname in seen:
            parts.append(f"(?P={wname})")
        else:
            seen.add(wname)
            c = m.group("constraint") or ".+"
            parts.append(f"(?P<{wname}>{c})")
        last = m.end()
    parts.append(re.escape(pattern[last:]))
    parts.append("$")
    cre = re.compile("".join(parts))

    if files is None:
        files = sorted(
            os.path.normpath(os.path.join(dp, f))
            for dp, dns, fns in os.walk(dirname, followlinks=followlinks)
            for f in chain(fns, dns)
        )
    for f in files:
        m = cre.match(f)
        if m:
            for name, val in m.groupdict().items():
                getattr(wc_res, name).append(val)
    return wc_res


def subpath(path_or_func, strip_suffix=None, with_suffix=None, basename=False, parent=False, ancestor=None):
    def do(path):
        if isinstance(path, Path):
            path = str(path)
        if not isinstance(path, str):
            raise ValueError(f"Value passed to subpath must be str/Path, got {path!r}")
        if strip_suffix is not None:
            if not path.endswith(strip_suffix):
                raise ValueError(f"Path {path} does not end with {strip_suffix}")
            path = path[:-len(strip_suffix)]
        if with_suffix is not None:
            path += with_suffix
        if basename:
            if parent or ancestor is not None:
                raise ValueError("basename cannot be used together with parent or ancestor")
            path = os.path.basename(path)
        elif parent:
            if ancestor is not None:
                raise ValueError("parent cannot be used together with ancestor")
            path = os.path.dirname(path) or "."
        elif ancestor is not None:
            if ancestor < 1:
                raise ValueError("ancestor must be greater than 0")
            for _ in range(ancestor):
                path = os.path.dirname(path)
                if path == "":
                    path = "."
                    break
        return path

    if callable(path_or_func):
        params = get_function_params(path_or_func)
        def inner(wildcards, **args):
            return do(path_or_func(wildcards, **get_input_function_aux_params(path_or_func, args)))
        overwrite_function_params(inner, params)
        return inner
    return do(path_or_func)


def branch(condition, then=None, otherwise=None, cases=None):
    def convert_none(v):
        return v or []

    def handle_c(v, wc):
        return convert_none(v(wc) if callable(v) else v)

    def do_then_else(wc):
        return handle_c(then, wc) if handle_c(condition, wc) else handle_c(otherwise, wc)

    def do_cases(wc):
        res = handle_c(condition, wc)
        if res in cases:
            return handle_c(cases[res], wc)
        if otherwise is not None:
            return handle_c(otherwise, wc)
        raise KeyError(f"Key {res} not found in branch cases")

    fn = do_cases if cases is not None else do_then_else
    if any(callable(v) for v in (condition, then, otherwise)):
        return lambda wc: fn(wc)
    return fn(None)


_NODEFAULT = object()


def _dpath_lookup(mapping, dpath_str):
    parts = [p for p in dpath_str.strip("/").split("/") if p]
    cur = [mapping]
    for part in parts:
        nxt = []
        for item in cur:
            if isinstance(item, Mapping):
                if part == "*":
                    nxt.extend(item.values())
                elif part in item:
                    nxt.append(item[part])
            elif isinstance(item, list):
                if part == "*":
                    nxt.extend(item)
                elif part.isdigit() and int(part) < len(item):
                    nxt.append(item[int(part)])
        cur = nxt
    if not cur:
        raise KeyError(dpath_str)
    return cur[0] if len(cur) == 1 else cur


def lookup(query=None, dpath=None, cols=None, is_nrows=None, within=None, default=_NODEFAULT, **namespace):
    if within is None:
        raise LookupError(msg="Must provide within")
    if query is not None and isinstance(within, Mapping):
        dpath = query
        query = None
    if dpath is not None:
        def eval_dpath(wc=None):
            ns = dict(namespace)
            for k, v in list(ns.items()):
                if callable(v):
                    ns[k] = v(wc)
            if wc:
                for k, v in wc.items():
                    ns.setdefault(k, v)
            expr = dpath(wc) if callable(dpath) else dpath
            if ns:
                expr = snakemake_format(expr, **ns)
            try:
                return _dpath_lookup(within, expr)
            except KeyError:
                if default is not _NODEFAULT:
                    return default
                raise LookupError(dpath=expr)
        if callable(dpath) or any(n not in namespace for n in get_wildcard_names(dpath)) or any(callable(v) for v in namespace.values()):
            return eval_dpath
        return eval_dpath(None)
    raise LookupError(msg="Query lookup requires pandas DataFrame or dict dpath")


def exists(path):
    return os.path.exists(str(path))


def evaluate(expr):
    return expr


def flatten(list_of_lists):
    res = []
    for i in list_of_lists:
        if isinstance(i, list):
            res.extend(flatten(i))
        else:
            res.append(str(i))
    return res


def parse_input(infile, parser, **kwargs):
    def _parse(wildcards, input):
        _inf = infile(wildcards, input) if callable(infile) else infile
        with open(str(_inf), "r", encoding="utf-8") as fh:
            return fh.read().strip() if parser is None else parser(fh, **kwargs)
    return _parse


def extract_checksum(infile, **kwargs):
    target = kwargs.get("file", "").lstrip("./")
    for line in infile:
        parts = line.rstrip("\r\n").split("  ", 1)
        if len(parts) == 2 and parts[1].lstrip("./") == target:
            return parts[0]
    raise KeyError(target)


def prepend_param(*args, **kwargs):
    return ""


class _RuleItemProxy:
    def __init__(self, name):
        self.name = name

    def __getattr__(self, key):
        return self._make(lambda item: getattr(item, key))

    def __getitem__(self, key):
        return self._make(lambda item: item[key])

    def _make(self, getter):
        if self.name == "output":
            return lambda wildcards, output: getter(output)
        if self.name == "input":
            return lambda wildcards, input: getter(input)
        if self.name == "params":
            return lambda wildcards, params: getter(params)
        if self.name == "resources":
            return lambda wildcards, resources: getter(resources)
        return lambda wildcards: None


def _rule_item_factory(name):
    if name == "threads":
        return lambda _wildcards, threads: threads
    return _RuleItemProxy(name)


def update_config(config, overwrite_config):
    for k, v in overwrite_config.items():
        if isinstance(v, Mapping) and isinstance(config.get(k), Mapping):
            update_config(config[k], v)
        else:
            config[k] = v
    return config


def simplify_path(path):
    p = str(path)
    try:
        rel = os.path.relpath(p)
        return rel if not rel.startswith("..") else p
    except Exception:
        return p


def min_version(v):
    return True


def validate(data, schema, set_default=True):
    frame = inspect.currentframe().f_back
    wf = frame.f_globals.get("workflow") if frame else None
    if wf and wf.modifier.skip_validation:
        return data
    schema_path = str(schema)
    if not os.path.isabs(schema_path) and wf and wf.included_stack:
        schema_path = os.path.join(wf.current_basedir, schema_path)
    resp = _call_rust({
        "req": "validate_schema",
        "data": data,
        "schema": schema_path,
        "set_default": bool(set_default),
    })
    if isinstance(data, dict) and isinstance(resp.get("data"), dict):
        data.clear()
        data.update(resp["data"])
    return data


class Scatter:
    pass


class Gather:
    pass


class Rules:
    def __init__(self):
        self._rules = {}

    def _register_rule(self, name, rule):
        self._rules[name] = rule
        setattr(self, name, _RuleProxy(rule))


class _RuleProxy:
    def __init__(self, rule):
        self._rule = rule

    @property
    def input(self):
        return self._rule.input

    @property
    def output(self):
        return self._rule.output

    @property
    def params(self):
        return self._rule.params


class CheckpointJob:
    def __init__(self, rule, output):
        self.rule = rule
        self.output = output


class Checkpoint:
    def __init__(self, rule, checkpoints):
        self.rule = rule
        self.checkpoints = checkpoints

    def get(self, **wildcards):
        missing = self.rule.wildcard_names - set(wildcards.keys())
        if missing:
            raise WorkflowError(f"Missing wildcard values for {', '.join(sorted(missing))}")
        out = self.rule.expand_output_for_checkpoint(wildcards)
        if self.checkpoints.created_output:
            if set(str(x) for x in out).issubset(self.checkpoints.created_output):
                return CheckpointJob(self.rule, out)
        target = checkpoint_target(str(out[0]))
        raise IncompleteCheckpointException(self.rule, target)


class CheckpointsProxy:
    def __init__(self, root):
        self.parent = root

    @property
    def created_output(self):
        return self.parent.created_output

    def register(self, rule, fallback_name=None):
        cp = Checkpoint(rule, self)
        if fallback_name:
            setattr(self, fallback_name, cp)
        setattr(self, rule.name, cp)


class Checkpoints:
    def __init__(self):
        self.created_output = set()

    def spawn_new_namespace(self):
        return CheckpointsProxy(self)


class PathModifier:
    def __init__(self, replace_prefix=None, prefix=None, inner=None):
        self.inner = inner
        self.prefix = None
        if prefix:
            p = str(prefix)
            if not p.endswith("/"):
                p += "/"
            self.prefix = p
        self.replace_prefix_items = sorted(
            list((replace_prefix or {}).items()), key=lambda kv: len(kv[0]), reverse=True
        )

    def modify(self, path, prop=None):
        if (
            is_flagged(path, "path_modified")
            or is_flagged(path, "local")
            or is_flagged(path, "storage_object")
            or callable(path)
        ):
            return path
        if isinstance(path, Path):
            path = str(path.as_posix())
        mod = self._replace(path)
        if self.inner is not None:
            mod = self.inner._replace(mod)
        if mod == path:
            return path
        res = AnnotatedString(mod)
        if hasattr(path, "flags"):
            res.flags.update(path.flags)
        res.flags["path_modified"] = True
        return res

    def _replace(self, path):
        if not isinstance(path, str) or os.path.isabs(path) or path.startswith(".."):
            return path
        if self.replace_prefix_items:
            for pfx, repl in self.replace_prefix_items:
                if path.startswith(pfx):
                    return repl + path[len(pfx):]
            return path
        if self.prefix:
            return self.prefix + path
        return path


class Pathvars:
    PATHVAR_RE = re.compile(r"<(?P<name>[a-z][a-z0-9_]*)>")

    def __init__(self, items=None, level=None):
        self.items = dict(items or {})
        self.level = dict(level or {})

    @classmethod
    def with_defaults(cls):
        d = {k: k for k in ("results", "stats", "reports", "temp", "resources", "logs", "benchmarks")}
        return cls(d, {k: 4 for k in d})

    @classmethod
    def from_other(cls, other):
        return cls(other.items, other.level)

    def update(self, items_dict, lvl):
        for k, v in items_dict.items():
            if k not in self.items or lvl <= self.level.get(k, 99):
                self.items[k] = v
                self.level[k] = lvl
        self._check_cycles()

    def _check_cycles(self):
        visiting = set()
        visited = set()
        def dfs(k):
            if k in visited:
                return
            if k in visiting:
                raise WorkflowError(f"Cyclic pathvar definition involving '{k}'.")
            visiting.add(k)
            val = self.items.get(k, "")
            if isinstance(val, str):
                for m in self.PATHVAR_RE.finditer(val):
                    dep = m.group("name")
                    if dep in self.items:
                        dfs(dep)
            visiting.remove(k)
            visited.add(k)
        for key in list(self.items.keys()):
            dfs(key)

    def apply(self, path):
        cur = path
        steps = 0
        while self.PATHVAR_RE.search(cur):
            steps += 1
            if steps > 32:
                raise WorkflowError(f"Cyclic pathvar definition detected in '{path}'")
            def repl(m):
                k = m.group("name")
                if k not in self.items:
                    raise UndefinedPathvarException(f"Undefined pathvar '{k}' in {path}")
                return self.items[k]
            cur = self.PATHVAR_RE.sub(repl, cur)
        return cur


class InOutput:
    def __init__(self, paths, kwpaths, modifier):
        self.paths = paths if isinstance(paths, (tuple, list)) else (paths,)
        self.kwpaths = kwpaths or {}
        self.modifier = modifier


class RuleInfo:
    def __init__(self, func):
        self.func = func
        self.name = None
        self.input = None
        self.output = None
        self.params = None
        self.threads = None
        self.resources = None
        self.retries = 0
        self.priority = 0
        self.log = None
        self.message = None
        self.benchmark = None
        self.conda_env = None
        self.container_img = None
        self.is_containerized = False
        self.env_modules = None
        self.wildcard_constraints = None
        self.shadow_depth = None
        self.group = None
        self.cache = None
        self.handover = False
        self.default_target = False
        self.localrule = False
        self.pathvars = None
        self.docstring = None
        self.norun = False
        self.shellcmd = None
        self.script = None
        self.notebook = None
        self.wrapper = None
        self.template_engine = None
        self.cwl = None

    def apply_overwrite(self, ovr):
        for attr in (
            "input", "output", "params", "threads", "resources", "retries", "priority",
            "log", "message", "benchmark", "conda_env", "container_img", "env_modules",
            "wildcard_constraints", "shadow_depth", "group", "cache", "handover",
            "default_target", "localrule", "pathvars", "docstring",
        ):
            val = getattr(ovr, attr)
            if val is not None and val is not False and val != 0:
                setattr(self, attr, val)


def _flatten_inoutput(inoutput, path_modifier=None, prop=None, pathvars=None):
    items = []
    if inoutput is None:
        return items
    def handle_one(name, val):
        if _not_iterable(val) or callable(val):
            vals = [val]
            is_list = False
        else:
            vals = list(val)
            is_list = True
        for v in vals:
            if isinstance(v, Path):
                v = str(v.as_posix())
            if path_modifier is not None and not callable(v):
                v = path_modifier.modify(v, prop)
            if pathvars is not None and isinstance(v, str) and not callable(v):
                applied = pathvars.apply(v)
                if hasattr(v, "flags") and v.flags:
                    ann = AnnotatedString(applied)
                    ann.flags.update(v.flags)
                    v = ann
                else:
                    v = applied
            items.append((name if not is_list else name, v, is_list))

    for p in inoutput.paths:
        if isinstance(p, Mapping):
            for k, v in p.items():
                handle_one(k, v)
        else:
            handle_one(None, p)
    for k, v in inoutput.kwpaths.items():
        handle_one(k, v)
    return items


def _sanitize_report_flag(v):
    if not isinstance(v, dict):
        return _json_safe_value(v)
    out = {}
    for rk, rv in v.items():
        if rk == "labels":
            if isinstance(rv, dict):
                labels = {}
                for lk, lv in rv.items():
                    if lv is None or callable(lv):
                        continue
                    labels[str(lk)] = lv if isinstance(lv, (bool, int, float, str)) else str(lv)
                out["labels"] = labels
            elif callable(rv) or rv is None:
                out["labels"] = None
            else:
                out["labels"] = _json_safe_value(rv)
        elif callable(rv):
            out[rk] = None
        else:
            out[rk] = _json_safe_value(rv)
    return out


def _json_safe_value(v):
    if v is None or isinstance(v, (bool, int, float, str)):
        return v
    if callable(v):
        return None
    if isinstance(v, dict):
        return {str(k): _json_safe_value(val) for k, val in v.items() if not callable(val)}
    if isinstance(v, (list, tuple)):
        return [_json_safe_value(x) for x in v if not callable(x)]
    return str(v)


def _serialize_flags(val):
    fl = getattr(val, "flags", {})
    out = {}
    for k, v in fl.items():
        if k == "multiext":
            out[k] = getattr(v, "prefix", str(v))
        elif k == "ensure":
            out[k] = v
        elif k == "report":
            out[k] = _sanitize_report_flag(v)
        elif k in ("repeat",):
            out[k] = int(v)
        elif isinstance(v, (bool, int, float, str)):
            out[k] = v
        else:
            out[k] = True
    return out


class RuleObj:
    def __init__(self, rule_id, name, lineno, snakefile, basedir, is_checkpoint, ruleinfo, workflow, pathvars):
        self.id = rule_id
        self.name = name
        self.lineno = lineno
        self.snakefile = snakefile
        self.basedir = basedir
        self.is_checkpoint = is_checkpoint
        self.ruleinfo = ruleinfo
        self.workflow = workflow
        self.pathvars = pathvars
        self.mod_globals = workflow.modifier.globals
        self.norun = ruleinfo.norun
        self.is_handover = bool(ruleinfo.handover)
        self.default_target = bool(ruleinfo.default_target)
        self.localrule = bool(ruleinfo.localrule)
        self.shellcmd = ruleinfo.shellcmd if isinstance(ruleinfo.shellcmd, str) else None
        self.run_func = ruleinfo.func
        self.run_code_len = len(ruleinfo.func.__code__.co_code) if (ruleinfo.func and not ruleinfo.norun) else 0
        self.input_items = []
        self.output_items = []
        self.log_items = []
        self.benchmark_item = None
        self.params_items = []
        self.wildcard_constraints = {}
        self.wildcard_names = set()
        self.input = InputFiles()
        self.output = OutputFiles()
        self.params = Params()
        self._build_items()

    def _build_items(self):
        wc_c = dict(self.workflow.modifier.wildcard_constraints)
        if self.ruleinfo.wildcard_constraints:
            args, kw = self.ruleinfo.wildcard_constraints
            for a in args:
                if isinstance(a, Mapping):
                    wc_c.update(a)
            wc_c.update(kw)
        self.wildcard_constraints = wc_c

        self.output_items = _flatten_inoutput(
            self.ruleinfo.output, self.ruleinfo.output.modifier if self.ruleinfo.output else None, "output", self.pathvars
        )
        for name, val, _ in self.output_items:
            if isinstance(val, str):
                self.wildcard_names.update(get_wildcard_names(val))
                ann = AnnotatedString(strip_wildcard_constraints(val))
                if hasattr(val, "flags") and val.flags.get("path_modified"):
                    ann.flags["path_modified"] = True
                ann.flags["from_rule"] = self.name
                self.output.append(ann)
                if name:
                    self.output._set_name(name, len(self.output) - 1)

        self.input_items = _flatten_inoutput(
            self.ruleinfo.input, self.ruleinfo.input.modifier if self.ruleinfo.input else None, "input", self.pathvars
        )
        for name, val, _ in self.input_items:
            if isinstance(val, str) and not callable(val):
                self.input.append(val)
                if name:
                    self.input._set_name(name, len(self.input) - 1)

        self.log_items = _flatten_inoutput(
            self.ruleinfo.log, self.ruleinfo.log.modifier if self.ruleinfo.log else None, "log", self.pathvars
        )
        if self.ruleinfo.benchmark:
            bms = _flatten_inoutput(
                self.ruleinfo.benchmark, self.ruleinfo.benchmark.modifier, "benchmark", self.pathvars
            )
            if bms:
                self.benchmark_item = bms[0]

        if self.ruleinfo.params:
            p_args, p_kw = self.ruleinfo.params
            for a in p_args:
                self.params_items.append((None, a))
                self.params.append(a)
            for k, v in p_kw.items():
                self.params_items.append((k, v))
                idx = len(self.params)
                self.params.append(v)
                self.params._set_name(k, idx)

    def expand_output_for_checkpoint(self, wildcards):
        res = OutputFiles()
        for name, val, _ in self.output_items:
            s = apply_wildcards(strip_wildcard_constraints(str(val)), wildcards)
            iof = _IOFile(AnnotatedString(s), rule=self)
            res.append(iof)
            if name:
                res._set_name(name, len(res) - 1)
        return res

    def to_descriptor(self):
        def ser_io(items, io_kind=None):
            out = []
            for idx, (name, val, is_list) in enumerate(items):
                is_cb = callable(val) or (isinstance(val, AnnotatedString) and val.is_callable())
                cb_id = None
                if is_cb:
                    cb_id = f"cb_{self.id}_{len(self.workflow._callbacks)}"
                    self.workflow._callbacks[cb_id] = val
                    self.workflow._callback_rules[cb_id] = (self, io_kind)
                flags_dict = _serialize_flags(val)
                if "ensure" in flags_dict and isinstance(flags_dict["ensure"], dict):
                    ens = dict(flags_dict["ensure"])
                    if callable(ens.get("checksum")):
                        ens_cb_id = f"cb_{self.id}_ens_{len(self.workflow._callbacks)}"
                        self.workflow._callbacks[ens_cb_id] = ens["checksum"]
                        self.workflow._callback_rules[ens_cb_id] = (self, None)
                        ens["checksum"] = None
                        ens["checksum_cb"] = ens_cb_id
                    flags_dict["ensure"] = ens
                out.append({
                    "name": name,
                    "is_list": is_list,
                    "value": None if is_cb else str(val),
                    "callable_id": cb_id,
                    "is_unpack": is_flagged(val, "unpack"),
                    "flags": flags_dict,
                })
            return out

        def ser_val(v, tag):
            if callable(v) or (isinstance(v, AnnotatedString) and v.is_callable()):
                cb_id = f"cb_{self.id}_{tag}_{len(self.workflow._callbacks)}"
                self.workflow._callbacks[cb_id] = v
                self.workflow._callback_rules[cb_id] = (self, None)
                return {"callable_id": cb_id, "value": None}
            return {"callable_id": None, "value": _to_json_compat(v)}

        res_map = {}
        if self.ruleinfo.resources:
            _, rkw = self.ruleinfo.resources
            for k, v in rkw.items():
                res_map[k] = ser_val(v, f"res_{k}")

        threads_val = ser_val(self.ruleinfo.threads if self.ruleinfo.threads is not None else 1, "threads")
        group_val = ser_val(self.ruleinfo.group, "group") if self.ruleinfo.group is not None else None
        prio_val = ser_val(self.ruleinfo.priority, "priority")
        msg_val = ser_val(self.ruleinfo.message, "message") if self.ruleinfo.message is not None else None
        shell_val = ser_val(self.ruleinfo.shellcmd, "shell") if self.ruleinfo.shellcmd is not None else None
        script_val = ser_val(self.ruleinfo.script, "script") if self.ruleinfo.script is not None else None

        params_list = []
        string_params = []
        for idx, (name, val) in enumerate(self.params_items):
            s = ser_val(val, f"param_{idx}")
            if isinstance(val, str) and not callable(val):
                string_params.append([name or str(idx), val])
            params_list.append({"name": name, "val": s})

        bm_desc = ser_io([self.benchmark_item], "benchmark")[0] if self.benchmark_item else None

        container = self.ruleinfo.container_img
        if container is None and self.workflow.modifier.global_container_img:
            container = self.workflow.modifier.global_container_img

        return {
            "id": self.id,
            "name": self.name,
            "lineno": self.lineno,
            "snakefile": self.snakefile,
            "basedir": self.basedir,
            "is_checkpoint": self.is_checkpoint,
            "norun": self.norun,
            "is_run": not self.norun and self.ruleinfo.shellcmd is None and self.ruleinfo.script is None and self.ruleinfo.wrapper is None and self.ruleinfo.notebook is None and self.ruleinfo.template_engine is None and self.ruleinfo.cwl is None,
            "run_code_len": self.run_code_len,
            "is_handover": self.is_handover,
            "default_target": self.default_target,
            "localrule": self.localrule,
            "input": ser_io(self.input_items, "input"),
            "output": ser_io(self.output_items, "output"),
            "log": ser_io(self.log_items, "log"),
            "benchmark": bm_desc,
            "params": params_list,
            "string_params": string_params,
            "threads": threads_val,
            "resources": res_map,
            "retries": int(self.ruleinfo.retries or 0),
            "priority": prio_val,
            "shadow": "full" if self.ruleinfo.shadow_depth is True else self.ruleinfo.shadow_depth,
            "group": group_val,
            "cache": self.ruleinfo.cache,
            "message": msg_val,
            "shellcmd": shell_val,
            "script": script_val,
            "wrapper": self.ruleinfo.wrapper,
            "notebook": self.ruleinfo.notebook,
            "template_engine": self.ruleinfo.template_engine,
            "cwl": self.ruleinfo.cwl,
            "conda_env": self.ruleinfo.conda_env,
            "container_img": container if container is not False else None,
            "envmodules": list(self.ruleinfo.env_modules) if self.ruleinfo.env_modules else [],
            "wildcard_constraints": self.wildcard_constraints,
            "shell_exec": shell.get_executable(),
            "shell_prefix": shell._prefix,
            "shell_suffix": shell._suffix,
            "docstring": self.ruleinfo.docstring,
            "config": _to_json_compat(self.mod_globals.get("config", {})),
        }


def _to_json_compat(v):
    if v is None or isinstance(v, (bool, int, float, str)):
        return v
    if isinstance(v, Path):
        return str(v.as_posix())
    if isinstance(v, (list, tuple, set)):
        return [_to_json_compat(x) for x in v]
    if isinstance(v, Mapping):
        return {str(k): _to_json_compat(val) for k, val in v.items()}
    return str(v)


class WorkflowModifier:
    def __init__(
        self,
        wf,
        is_module=True,
        globals_dict=None,
        config=None,
        base_snakefile=None,
        skip_validation=False,
        resolved_rulename_modifier=None,
        local_rulename_modifier=None,
        rule_whitelist=None,
        rule_exclude_list=None,
        ruleinfo_overwrite=None,
        allow_rule_overwrite=False,
        path_modifier=None,
        namespace=None,
        rule_proxies=None,
        pathvars=None,
    ):
        self.workflow = wf
        self.is_module = is_module
        self.base_snakefile = base_snakefile
        self.skip_configfile = config is not None
        self.skip_validation = skip_validation
        self.resolved_rulename_modifier = resolved_rulename_modifier
        self.local_rulename_modifier = local_rulename_modifier
        self.rule_whitelist = rule_whitelist
        self.rule_exclude_list = rule_exclude_list
        self.ruleinfo_overwrite = ruleinfo_overwrite
        self.allow_rule_overwrite = allow_rule_overwrite
        self.namespace = namespace

        if is_module:
            if globals_dict is None:
                self.globals = dict(wf.vanilla_globals)
                self.parent_modifier = wf.modifier
                self.allow_rule_overwrite = allow_rule_overwrite or self.parent_modifier.allow_rule_overwrite
            else:
                self.globals = globals_dict
                self.parent_modifier = None
            self.globals["__name__"] = namespace or "__main__"
            self.rule_proxies = rule_proxies or Rules()
            self.globals["rules"] = self.rule_proxies
            self.globals["checkpoints"] = self.globals["checkpoints"].spawn_new_namespace() if "checkpoints" in self.globals else wf._checkpoints
            self.globals["config"] = copy.deepcopy(config) if config is not None else {}
            self.wildcard_constraints = {}
            self.pathvars = pathvars or Pathvars.with_defaults()
            self.rules = set()
            self.path_modifier = path_modifier or PathModifier()
            self.global_container_img = None
        else:
            pm = wf.modifier
            self.parent_modifier = pm
            self.globals = pm.globals
            self.wildcard_constraints = pm.wildcard_constraints
            self.pathvars = pm.pathvars
            self.rules = pm.rules
            self.rule_proxies = pm.rule_proxies
            self.path_modifier = pm.path_modifier
            self.allow_rule_overwrite = allow_rule_overwrite or pm.allow_rule_overwrite
            self.global_container_img = pm.global_container_img

    def is_main_snakefile(self):
        return self.base_snakefile is None

    def avail_rulename(self, rulename):
        if (self.rule_whitelist is not None and rulename not in self.rule_whitelist) or (
            self.rule_exclude_list is not None and rulename in self.rule_exclude_list
        ):
            return False
        if self.parent_modifier is not None:
            nm = self.resolved_rulename_modifier(rulename) if self.resolved_rulename_modifier else rulename
            return self.parent_modifier.avail_rulename(nm)
        return True

    def modify_rulename(self, rulename):
        if self.resolved_rulename_modifier is not None:
            rulename = self.resolved_rulename_modifier(rulename)
        if self.parent_modifier is not None:
            rulename = self.parent_modifier.modify_rulename(rulename)
        return rulename

    def inherit_rule_proxies(self, child):
        for name, rule in child.rule_proxies._rules.items():
            if child.local_rulename_modifier is not None:
                name = child.local_rulename_modifier(name)
            self.rule_proxies._register_rule(name, rule)

    def __enter__(self):
        self.workflow.modifier_stack.append(self)

    def __exit__(self, *args):
        self.workflow.modifier_stack.pop()
        if self.namespace:
            ns = types.ModuleType(self.namespace)
            ns.__dict__.update(self.globals)
            self.workflow.globals[self.namespace] = ns


def _get_name_modifier(rules, name_modifier):
    if name_modifier is None:
        return None
    if "*" in name_modifier:
        return lambda rn: name_modifier.replace("*", rn)
    if "*" in rules or len(rules) > 1:
        raise SyntaxError("Multiple rules in 'use rule' but name modification lacks '*'.")
    return lambda rn: name_modifier


class ModuleInfo:
    def __init__(self, wf, name, pathvars, snakefile=None, meta_wrapper=None, config=None, skip_validation=False, replace_prefix=None, prefix=None):
        self.workflow = wf
        self.name = name
        self.snakefile = snakefile
        self.meta_wrapper = meta_wrapper
        self.config = config
        self.skip_validation = skip_validation
        self.parent_modifier = wf.modifier
        self.rule_proxies = Rules()
        self.pathvars = pathvars
        self.path_modifier = PathModifier(replace_prefix, prefix, self.parent_modifier.path_modifier)
        self.wildcards_modifier_overwrited = {}

    def use_rules(self, rules, name_modifier=None, exclude_rules=None, ruleinfo=None):
        whitelist = None if "*" in rules else set(rules)
        nm_fn = _get_name_modifier(rules, name_modifier)
        allow_ovr = False
        if whitelist is None:
            self.wildcards_modifier_overwrited[nm_fn] = set()
        else:
            for r in whitelist:
                mod_this = nm_fn(r) if nm_fn else r
                for prev_fn in self.wildcards_modifier_overwrited:
                    if (prev_fn(r) if prev_fn else r) == mod_this:
                        allow_ovr = True
        mod = WorkflowModifier(
            self.workflow,
            config=self.config,
            base_snakefile=self.snakefile,
            skip_validation=self.skip_validation,
            rule_exclude_list=exclude_rules,
            rule_whitelist=whitelist,
            resolved_rulename_modifier=nm_fn,
            local_rulename_modifier=nm_fn,
            ruleinfo_overwrite=ruleinfo,
            allow_rule_overwrite=allow_ovr,
            namespace=self.name,
            path_modifier=self.path_modifier,
            rule_proxies=self.rule_proxies,
            pathvars=self.pathvars,
        )
        with mod:
            self.workflow.include(self.snakefile, overwrite_default_target=True)
            self.parent_modifier.inherit_rule_proxies(mod)


class Workflow:
    def __init__(self, overwrite_shellcmd=None, cores=1):
        self.cores = cores
        self._rules = OrderedDict()
        self.rules_by_id = {}
        self.default_target = None
        self._ruleorder = []
        self._localrules = set()
        self._linemaps = {}
        self._included = OrderedDict()
        self.included_stack = []
        self.snakefile_sources = []
        self._rulecount = 0
        self._next_rule_id = 0
        self._callbacks = {}
        self._callback_rules = {}
        self._envvars = set()
        self._scatter = {}
        self._resource_scopes = {}
        self._onstart = None
        self._onsuccess = None
        self._onerror = None
        self.modules = {}
        self._checkpoints = Checkpoints()
        self.overwrite_shellcmd = overwrite_shellcmd
        self.overwrite_workdir = False
        self.check_envvars = False
        self.configfiles = []

        g = dict(globals())
        g["workflow"] = self
        g["shell"] = shell
        g["checkpoints"] = self._checkpoints
        g["scatter"] = Scatter()
        g["gather"] = Gather()
        g["expand"] = expand
        g["collect"] = collect
        g["temp"] = temp
        g["temporary"] = temporary
        g["protected"] = protected
        g["directory"] = directory
        g["ancient"] = ancient
        g["touch"] = touch
        g["pipe"] = pipe
        g["service"] = service
        g["repeat"] = repeat
        g["report"] = report
        g["multiext"] = multiext
        g["ensure"] = ensure
        g["unpack"] = unpack
        g["local"] = local
        g["glob_wildcards"] = glob_wildcards
        g["subpath"] = subpath
        g["branch"] = branch
        g["lookup"] = lookup
        g["exists"] = exists
        g["evaluate"] = evaluate
        g["flatten"] = flatten
        g["parse_input"] = parse_input
        g["extract_checksum"] = extract_checksum
        g["update_config"] = update_config
        g["simplify_path"] = simplify_path
        g["format"] = snakemake_format
        g["input"] = _rule_item_factory("input")
        g["output"] = _rule_item_factory("output")
        g["params"] = _rule_item_factory("params")
        g["resources"] = _rule_item_factory("resources")
        g["threads"] = _rule_item_factory("threads")
        g["rules"] = Rules()
        g["config"] = {}
        g["Paramspace"] = _Paramspace
        class _StorageRegistry:
            def __call__(self, query, *args, _provider=None, **kwargs):
                q = str(query)
                if q.startswith("https://"):
                    local_p = ".snakemake/storage/http/" + q[len("https://"):]
                elif q.startswith("http://"):
                    local_p = ".snakemake/storage/http/" + q[len("http://"):]
                elif "://" in q:
                    scheme, rest = q.split("://", 1)
                    local_p = f".snakemake/storage/{scheme}/{rest}"
                elif _provider:
                    local_p = f".snakemake/storage/{_provider}/{q.lstrip('/')}"
                else:
                    local_p = q
                ann = AnnotatedString(local_p)
                ann.flags["storage_object"] = q
                return ann

            def __getattr__(self, provider):
                if provider.startswith("_"):
                    raise AttributeError(provider)
                return lambda query, *args, **kwargs: self(query, *args, _provider=provider, **kwargs)

        g["storage"] = _StorageRegistry()

        self.vanilla_globals = dict(g)
        self.modifier_stack = [WorkflowModifier(self, globals_dict=g, pathvars=Pathvars.with_defaults())]
        self.modifier.globals["rules"] = self.modifier.rule_proxies

    @property
    def modifier(self):
        return self.modifier_stack[-1]

    @property
    def globals(self):
        return self.modifier.globals

    @property
    def pathvars(self):
        return self.modifier.pathvars

    @property
    def current_basedir(self):
        return os.path.dirname(os.path.abspath(self.included_stack[-1]))

    @property
    def basedir(self):
        if self.included_stack:
            return os.path.dirname(os.path.abspath(self.included_stack[0]))
        return os.getcwd()

    def source_path(self, rel_path):
        frame = inspect.currentframe().f_back
        calling_file = frame.f_code.co_filename
        calling_dir = os.path.dirname(os.path.abspath(calling_file)) if os.path.exists(calling_file) else self.current_basedir
        joined = os.path.normpath(os.path.join(calling_dir, str(rel_path)))
        return sourcecache_entry(joined, str(rel_path))

    def register_envvars(self, *envvars):
        self._envvars.update(envvars)

    def workdir(self, d):
        if self.overwrite_workdir:
            return
        p = str(d)
        os.makedirs(p, exist_ok=True)
        os.chdir(p)

    def register_pathvars(self, **items):
        self.pathvars.update(items, 3)

    def configfile(self, fp):
        if self.modifier.skip_configfile:
            return
        p = str(fp)
        if not os.path.isabs(p) and self.included_stack:
            cand = os.path.join(self.current_basedir, p)
            if os.path.exists(cand):
                p = cand
        self.configfiles.append(p)
        resp = _call_rust({"req": "load_yaml", "path": p})
        loaded = resp["data"]
        if isinstance(loaded, dict):
            update_config(self.globals["config"], loaded)
            if "pathvars" in loaded and isinstance(loaded["pathvars"], dict):
                self.pathvars.update(loaded["pathvars"], 2)

    def set_pepfile(self, fp):
        pass

    def pepschema(self, fp):
        pass

    def report(self, fp):
        pass

    def ruleorder(self, *rulenames):
        self._ruleorder.append(list(rulenames))

    def localrules(self, *rulenames):
        for r in rulenames:
            self._localrules.add(self.modifier.modify_rulename(r))

    def global_wildcard_constraints(self, **content):
        self.modifier.wildcard_constraints.update(content)

    def global_container(self, img):
        self.modifier.global_container_img = img

    def global_containerized(self, img):
        self.modifier.global_container_img = img

    def global_conda(self, env):
        pass

    def scattergather(self, **content):
        self._scatter.update(content)
        self.global_wildcard_constraints(scatteritem=r"\d+-of-\d+")
        for k, n in content.items():
            def mk(n_items):
                def fn(*args, **wc):
                    return expand(*args, scatteritem=[f"{i}-of-{n_items}" for i in range(1, n_items + 1)], **wc)
                return fn
            setattr(self.globals["scatter"], k, mk(n))
            setattr(self.globals["gather"], k, mk(n))

    def resource_scopes(self, **content):
        self._resource_scopes.update(content)

    def set_default_input_flags(self, *flags):
        pass

    def set_default_output_flags(self, *flags):
        pass

    def onstart(self, func):
        if self.modifier.is_main_snakefile():
            self._onstart = func

    def onsuccess(self, func):
        if self.modifier.is_main_snakefile():
            self._onsuccess = func

    def onerror(self, func):
        if self.modifier.is_main_snakefile():
            self._onerror = func

    def include(self, snakefile, overwrite_default_target=False):
        sf = str(snakefile)
        if not os.path.isabs(sf) and self.included_stack:
            sf = os.path.normpath(os.path.join(self.current_basedir, sf))
        if not self.modifier.is_module and sf in self._included:
            return
        self._included[sf] = sf
        self.included_stack.append(sf)
        saved_default = self.default_target

        resp = _call_rust({
            "req": "parse_snakefile",
            "path": sf,
            "overwrite_shellcmd": self.overwrite_shellcmd,
            "rulecount": self._rulecount,
        })
        code = resp["code"]
        self._linemaps[sf] = {int(k): v for k, v in resp["linemap"].items()}
        self._rulecount = resp["rulecount"]
        self.snakefile_sources.append((sf, resp["raw_source"]))

        sf_dir = os.path.dirname(os.path.abspath(sf))
        if sf_dir not in sys.path:
            sys.path.insert(0, sf_dir)

        exec(compile(code, sf, "exec"), self.globals)
        if not overwrite_default_target:
            self.default_target = saved_default
        self.included_stack.pop()

    def module(self, name=None, snakefile=None, meta_wrapper=None, config=None, skip_validation=False, replace_prefix=None, prefix=None, pathvars=None):
        mpv = Pathvars.from_other(self.pathvars)
        if pathvars:
            mpv.update(pathvars, 1)
        if isinstance(config, dict) and isinstance(config.get("pathvars"), dict):
            mpv.update(config["pathvars"], 1)
        if snakefile and not os.path.isabs(str(snakefile)) and self.included_stack:
            snakefile = os.path.normpath(os.path.join(self.current_basedir, str(snakefile)))
        self.modules[name] = ModuleInfo(
            self, name, mpv, snakefile=snakefile, meta_wrapper=meta_wrapper, config=config,
            skip_validation=skip_validation, replace_prefix=replace_prefix, prefix=prefix
        )

    def userule(self, rules, from_module=None, exclude_rules=None, name_modifier=None, lineno=None):
        caller_frame = inspect.currentframe().f_back

        def decorate(maybe_ruleinfo):
            if from_module is not None:
                mod_nm = name_modifier
                if from_module in self.modules:
                    module = self.modules[from_module]
                else:
                    # Dynamic module resolution
                    scope = {}
                    if caller_frame:
                        scope.update(caller_frame.f_globals)
                        scope.update(caller_frame.f_locals)
                    if from_module in scope and scope[from_module] in self.modules:
                        module = self.modules[scope[from_module]]
                        if name_modifier is not None:
                            if name_modifier.endswith("*") and name_modifier[:-1] in scope:
                                mod_nm = f"{scope[name_modifier[:-1]]}*"
                            elif name_modifier in scope:
                                mod_nm = str(scope[name_modifier])
                    else:
                        raise WorkflowError(f"Module {from_module} has not been registered before 'use rule'.")
                ovr = None if callable(maybe_ruleinfo) else maybe_ruleinfo
                module.use_rules(rules, mod_nm, exclude_rules=exclude_rules, ruleinfo=ovr)
            else:
                if not self.modifier.avail_rulename(name_modifier):
                    return
                orig_rule = self._rules[self.modifier.modify_rulename(rules[0])]
                ovr = None if callable(maybe_ruleinfo) else maybe_ruleinfo
                with WorkflowModifier(
                    self, is_module=False,
                    resolved_rulename_modifier=_get_name_modifier(rules, name_modifier),
                    ruleinfo_overwrite=ovr,
                ):
                    cp = copy.copy(orig_rule.ruleinfo)
                    self.rule(name=name_modifier, lineno=lineno, snakefile=self.included_stack[-1])(cp)
        return decorate

    def rule(self, name=None, lineno=None, snakefile=None, checkpoint=False):
        def decorate(ruleinfo):
            orig_name = name or (ruleinfo.name if ruleinfo.name else f"__rule_{self._next_rule_id}")
            if not self.modifier.avail_rulename(orig_name):
                return ruleinfo.func
            if self.modifier.ruleinfo_overwrite is not None:
                ruleinfo.apply_overwrite(self.modifier.ruleinfo_overwrite)
            rname = self.modifier.modify_rulename(orig_name)
            if rname in self._rules and not self.modifier.allow_rule_overwrite:
                raise CreateRuleException(f"The name {rname} is already used by another rule")

            pv = Pathvars.from_other(self.pathvars)
            if ruleinfo.pathvars:
                pv.update(ruleinfo.pathvars, 0)

            self._next_rule_id += 1
            rid = self._next_rule_id
            bd = os.path.dirname(os.path.abspath(snakefile)) if snakefile else os.getcwd()
            robj = RuleObj(rid, rname, lineno or 1, snakefile or "", bd, checkpoint, ruleinfo, self, pv)
            self._rules[rname] = robj
            self.rules_by_id[rid] = robj
            self.modifier.rules.add(robj)
            self.modifier.rule_proxies._register_rule(orig_name, robj)
            if checkpoint:
                self.globals["checkpoints"].register(robj, fallback_name=orig_name)
            if self.default_target is None or ruleinfo.default_target is True:
                self.default_target = rname
            if ruleinfo.localrule is True:
                self._localrules.add(rname)
            return ruleinfo.func
        return decorate

    def docstring(self, s):
        def dec(ri):
            ri.docstring = s.strip()
            return ri
        return dec

    def name(self, n):
        def dec(ri):
            ri.name = n
            return ri
        return dec

    def input(self, *paths, **kwpaths):
        def dec(ri):
            ri.input = InOutput(paths, kwpaths, self.modifier.path_modifier)
            return ri
        return dec

    def output(self, *paths, **kwpaths):
        def dec(ri):
            ri.output = InOutput(paths, kwpaths, self.modifier.path_modifier)
            return ri
        return dec

    def params(self, *args, **kw):
        def dec(ri):
            ri.params = (args, kw)
            return ri
        return dec

    def threads(self, v):
        def dec(ri):
            ri.threads = v
            return ri
        return dec

    def resources(self, *args, **kw):
        def dec(ri):
            ri.resources = (args, kw)
            return ri
        return dec

    def retries(self, v):
        def dec(ri):
            ri.retries = v
            return ri
        return dec

    def priority(self, v):
        def dec(ri):
            ri.priority = v
            return ri
        return dec

    def log(self, *args, **kw):
        def dec(ri):
            ri.log = InOutput(args, kw, self.modifier.path_modifier)
            return ri
        return dec

    def message(self, v):
        def dec(ri):
            ri.message = v
            return ri
        return dec

    def benchmark(self, v):
        def dec(ri):
            ri.benchmark = InOutput((v,), {}, self.modifier.path_modifier)
            return ri
        return dec

    def conda(self, v):
        def dec(ri):
            ri.conda_env = v
            return ri
        return dec

    def software(self, v):
        def dec(ri):
            ri.conda_env = v
            return ri
        return dec

    def container(self, v):
        def dec(ri):
            ri.container_img = v if v is not None else False
            return ri
        return dec

    def containerized(self, v):
        def dec(ri):
            ri.container_img = v
            ri.is_containerized = True
            return ri
        return dec

    def envmodules(self, *args):
        def dec(ri):
            ri.env_modules = args
            return ri
        return dec

    def register_wildcard_constraints(self, *args, **kw):
        def dec(ri):
            ri.wildcard_constraints = (args, kw)
            return ri
        return dec

    def shadow(self, v):
        def dec(ri):
            ri.shadow_depth = v
            return ri
        return dec

    def group(self, v):
        def dec(ri):
            ri.group = v
            return ri
        return dec

    def cache_rule(self, v):
        def dec(ri):
            ri.cache = v
            return ri
        return dec

    def handover(self, v):
        def dec(ri):
            ri.handover = v
            return ri
        return dec

    def default_target_rule(self, v):
        def dec(ri):
            ri.default_target = v
            return ri
        return dec

    def localrule(self, v):
        def dec(ri):
            ri.localrule = v
            return ri
        return dec

    def rule_pathvars(self, **kw):
        def dec(ri):
            ri.pathvars = kw
            return ri
        return dec

    def shellcmd(self, cmd):
        def dec(ri):
            ri.shellcmd = cmd
            return ri
        return dec

    def script(self, s):
        def dec(ri):
            ri.script = s
            return ri
        return dec

    def notebook(self, s):
        def dec(ri):
            ri.notebook = s
            return ri
        return dec

    def wrapper(self, s):
        def dec(ri):
            ri.wrapper = s
            return ri
        return dec

    def template_engine(self, s):
        def dec(ri):
            ri.template_engine = s
            return ri
        return dec

    def cwl(self, s):
        def dec(ri):
            ri.cwl = s
            return ri
        return dec

    def norun(self):
        def dec(ri):
            ri.norun = True
            return ri
        return dec

    def run(self, func):
        return RuleInfo(func)


class _Paramspace:
    def __init__(self, dataframe, filename_params=None, param_sep="~", filename_sep="_", single_wildcard=None):
        self.dataframe = dataframe
        self.param_sep = param_sep
        self.filename_sep = filename_sep
        self.single_wildcard = single_wildcard
        cols = list(dataframe.columns)
        if not filename_params:
            self.ordered_columns = cols
            self.pattern = "/".join(["{}"] * len(cols))
        else:
            fp = cols if filename_params == "*" else list(filename_params)
            self.ordered_columns = [c for c in cols if c not in fp] + fp
            self.pattern = "/".join(["{}"] * (len(cols) - len(fp) + 1))
            self.pattern = filename_sep.join([self.pattern] + ["{}"] * (len(fp) - 1))

    @property
    def wildcard_pattern(self):
        if self.single_wildcard:
            return f"{{{self.single_wildcard}}}"
        return self.pattern.format(*[f"{c}{self.param_sep}{{{c}}}" for c in self.ordered_columns])


# Register synthetic `snakemake` modules in sys.modules
def _install_synthetic_modules():
    def mk_mod(name, attrs):
        m = types.ModuleType(name)
        m.__dict__.update(attrs)
        sys.modules[name] = m
        return m

    logger = types.SimpleNamespace(
        info=lambda msg, **kw: print(msg, file=sys.stderr),
        warning=lambda msg, **kw: print(f"WARNING: {msg}", file=sys.stderr),
        error=lambda msg, **kw: print(f"ERROR: {msg}", file=sys.stderr),
        debug=lambda msg, **kw: None,
    )
    mk_mod("snakemake.logging", {"logger": logger})
    mk_mod("snakemake.exceptions", {
        "WorkflowError": WorkflowError,
        "WildcardError": WildcardError,
        "InputFunctionException": InputFunctionException,
        "RuleException": RuleException,
        "AmbiguousRuleException": AmbiguousRuleException,
        "MissingInputException": MissingInputException,
        "MissingOutputException": MissingOutputException,
        "ProtectedOutputException": ProtectedOutputException,
        "IncompleteCheckpointException": IncompleteCheckpointException,
        "CreateRuleException": CreateRuleException,
    })
    mk_mod("snakemake.io", {
        "WILDCARD_REGEX": WILDCARD_REGEX,
        "AnnotatedString": AnnotatedString,
        "IOFile": IOFile,
        "_IOFile": _IOFile,
        "expand": expand,
        "collect": collect,
        "glob_wildcards": glob_wildcards,
        "temp": temp,
        "temporary": temporary,
        "protected": protected,
        "directory": directory,
        "ancient": ancient,
        "touch": touch,
        "pipe": pipe,
        "service": service,
        "local": local,
        "unpack": unpack,
        "repeat": repeat,
        "report": report,
        "multiext": multiext,
        "ensure": ensure,
        "flag": flag,
        "is_flagged": is_flagged,
        "get_flag_value": get_flag_value,
        "apply_wildcards": apply_wildcards,
        "get_wildcard_names": get_wildcard_names,
        "strip_wildcard_constraints": strip_wildcard_constraints,
    })
    mk_mod("snakemake.utils", {
        "min_version": min_version,
        "validate": validate,
        "update_config": update_config,
        "format": snakemake_format,
        "simplify_path": simplify_path,
        "Paramspace": _Paramspace,
    })
    mk_mod("snakemake.shell", {"shell": shell})
    mk_mod("snakemake.ioutils", {
        "subpath": subpath,
        "branch": branch,
        "lookup": lookup,
        "exists": exists,
        "evaluate": evaluate,
        "flatten": flatten,
        "parse_input": parse_input,
        "extract_checksum": extract_checksum,
        "collect": collect,
    })
    mk_mod("snakemake.iocontainers", {
        "Namedlist": Namedlist,
        "InputFiles": InputFiles,
        "OutputFiles": OutputFiles,
        "Params": Params,
        "Log": Log,
        "Wildcards": Wildcards,
        "Resources": Resources,
    })
    mk_mod("snakemake", {
        "__version__": "10.0.0a-rustmake",
        "shell": shell,
        "workflow": None,
    })


_install_synthetic_modules()

_ACTIVE_WORKFLOW = None


def _build_namedlist(cls, items_spec):
    nl = cls()
    if not items_spec:
        return nl
    for entry in items_spec:
        if isinstance(entry, dict):
            name = entry.get("name")
            vals = entry.get("values")
            if vals is not None:
                start = len(nl)
                for v in vals:
                    nl.append(_IOFile(v) if cls in (InputFiles, OutputFiles) else v)
                if name:
                    nl._set_name(name, start, len(nl))
            else:
                v = entry.get("value")
                start = len(nl)
                nl.append(_IOFile(v) if cls in (InputFiles, OutputFiles) else v)
                if name:
                    nl._set_name(name, start)
        else:
            nl.append(_IOFile(entry) if cls in (InputFiles, OutputFiles) else entry)
    return nl


def _handle_cmd(req):
    global _ACTIVE_WORKFLOW
    cmd = req.get("cmd")

    if cmd == "ping":
        return {"ok": True}

    if cmd == "load_workflow":
        workdir = req.get("workdir")
        shell._executable = None
        shell._prefix = ""
        shell._suffix = ""
        wf = Workflow(overwrite_shellcmd=req.get("overwrite_shellcmd"), cores=req.get("cores", 1))
        if workdir:
            os.makedirs(workdir, exist_ok=True)
            os.chdir(workdir)
            wf.overwrite_workdir = True
        _ACTIVE_WORKFLOW = wf
        sys.modules["snakemake"].workflow = wf

        # Load CLI configfiles first
        for cf in req.get("configfiles", []):
            resp = _call_rust({"req": "load_yaml", "path": cf})
            if isinstance(resp["data"], dict):
                update_config(wf.globals["config"], resp["data"])
        # Apply CLI config overrides
        cli_cfg = req.get("config")
        if isinstance(cli_cfg, dict):
            update_config(wf.globals["config"], cli_cfg)
            if isinstance(cli_cfg.get("pathvars"), dict):
                wf.pathvars.update(cli_cfg["pathvars"], 2)

        snakefile = req["snakefile"]
        wf.include(snakefile, overwrite_default_target=True)

        # Re-apply CLI config overrides on top of configfile: in Snakefile
        if isinstance(cli_cfg, dict) and cli_cfg:
            update_config(wf.globals["config"], cli_cfg)

        serializable_globals = {}
        for k, v in wf.globals.items():
            if k.startswith("_") or k in ("config", "rules", "workflow", "checkpoints", "shell", "scatter", "gather"):
                continue
            if isinstance(v, (str, int, float, bool)):
                serializable_globals[k] = v
            elif isinstance(v, (list, tuple)) and all(isinstance(x, (str, int, float, bool)) for x in v):
                serializable_globals[k] = list(v)
            elif isinstance(v, dict) and all(isinstance(kk, str) and isinstance(vv, (str, int, float, bool)) for kk, vv in v.items()):
                serializable_globals[k] = v

        return {
            "ok": True,
            "rules": [r.to_descriptor() for r in wf._rules.values()],
            "ruleorder": wf._ruleorder,
            "localrules": sorted(wf._localrules),
            "default_target": wf.default_target,
            "envvars": sorted(wf._envvars),
            "global_wildcard_constraints": wf.modifier.wildcard_constraints,
            "pathvars": wf.pathvars.items,
            "resource_scopes": wf._resource_scopes,
            "scatter": wf._scatter,
            "snakefiles": wf.snakefile_sources,
            "linemaps": wf._linemaps,
            "serializable_globals": serializable_globals,
            "config": _to_json_compat(wf.globals.get("config", {})),
            "has_onstart": wf._onstart is not None,
            "has_onsuccess": wf._onsuccess is not None,
            "has_onerror": wf._onerror is not None,
            "workdir": os.getcwd(),
        }

    if cmd == "eval_callback":
        wf = _ACTIVE_WORKFLOW
        cb_id = req["callable_id"]
        cb = wf._callbacks[cb_id]
        wc = Wildcards(fromdict=req.get("wildcards") or {})
        aux = {}
        if req.get("input") is not None:
            aux["input"] = _build_namedlist(InputFiles, req["input"])
        if req.get("output") is not None:
            aux["output"] = _build_namedlist(OutputFiles, req["output"])
        if req.get("params") is not None:
            aux["params"] = _build_namedlist(Params, req["params"])
        if req.get("resources") is not None:
            aux["resources"] = _build_namedlist(Resources, req["resources"])
        if req.get("threads") is not None:
            aux["threads"] = req["threads"]
        if req.get("attempt") is not None:
            aux["attempt"] = req["attempt"]
        if req.get("rulename") is not None:
            aux["rulename"] = req["rulename"]

        try:
            call_fn = cb.callable if isinstance(cb, AnnotatedString) and cb.callable else cb
            kept = get_input_function_aux_params(call_fn, aux)
            res = call_fn(wc, **kept)
        except IncompleteCheckpointException as ic:
            return {
                "ok": True,
                "incomplete_checkpoint": True,
                "checkpoint_target": str(ic.targetfile),
            }

        # Coerce non-string/non-dict iterables (e.g. generators) into lists
        if isinstance(res, Iterable) and not isinstance(res, (str, bytes, dict, list, tuple, _IOFile, AnnotatedString, Path)):
            res = list(res)

        cb_meta = wf._callback_rules.get(cb_id)
        if cb_meta is not None and cb_meta[1] in ("input", "log"):
            robj, io_kind = cb_meta
            pm = robj.workflow.modifier.path_modifier
            pv = robj.pathvars
            def _post_io(v):
                if isinstance(v, Path):
                    v = str(v.as_posix())
                if isinstance(v, str):
                    fl = dict(getattr(v, "flags", {}))
                    v = pm.modify(v, io_kind)
                    if isinstance(v, str) and pv is not None:
                        applied = pv.apply(v)
                        if fl or getattr(v, "flags", None):
                            ann = AnnotatedString(applied)
                            ann.flags.update(fl)
                            ann.flags.update(getattr(v, "flags", {}))
                            v = ann
                        else:
                            v = applied
                return v
            if isinstance(res, dict):
                res = {k: ([_post_io(x) for x in v] if isinstance(v, (list, tuple)) else _post_io(v)) for k, v in res.items()}
            elif isinstance(res, (list, tuple)):
                res = [_post_io(x) for x in res]
            else:
                res = _post_io(res)

        # Convert result to list/dict/scalar descriptor preserving flags
        if isinstance(res, dict):
            return {
                "ok": True,
                "incomplete_checkpoint": False,
                "kind": "dict",
                "entries": [
                    {"name": str(k), "value": _to_json_compat(v), "flags": _serialize_flags(v)}
                    for k, v in res.items()
                ],
            }
        if isinstance(res, (list, tuple)):
            return {
                "ok": True,
                "incomplete_checkpoint": False,
                "kind": "list",
                "entries": [
                    {"value": _to_json_compat(v), "flags": _serialize_flags(v)}
                    for v in res
                ],
            }
        return {
            "ok": True,
            "incomplete_checkpoint": False,
            "kind": "scalar",
            "value": _to_json_compat(res),
            "flags": _serialize_flags(res),
        }

    if cmd == "eval_cli_expr":
        expr = req["expr"]
        wc = Wildcards(fromdict=req.get("wildcards") or {})
        inp = _build_namedlist(InputFiles, req.get("input") or [])
        ns = {
            "wildcards": wc,
            "input": inp,
            "attempt": req.get("attempt", 1),
            "threads": req.get("threads", 1),
            "system_tmpdir": tempfile.gettempdir(),
            "shutil": shutil,
            "os": os,
            "max": max,
            "min": min,
        }
        try:
            val = eval(expr, ns)
            return {"ok": True, "value": _to_json_compat(val)}
        except (NameError, SyntaxError):
            return {"ok": True, "value": expr}

    if cmd == "register_checkpoint_outputs":
        wf = _ACTIVE_WORKFLOW
        for path in req.get("outputs", []):
            wf._checkpoints.created_output.add(str(path))
        return {"ok": True}

    if cmd == "exec_run":
        wf = _ACTIVE_WORKFLOW
        robj = wf.rules_by_id[req["rule_id"]]
        inp = _build_namedlist(InputFiles, req.get("input") or [])
        outp = _build_namedlist(OutputFiles, req.get("output") or [])
        prm = _build_namedlist(Params, req.get("params") or [])
        wc = Wildcards(fromdict=req.get("wildcards") or {})
        res_nl = _build_namedlist(Resources, req.get("resources") or [])
        log_nl = _build_namedlist(Log, req.get("log") or [])
        threads = req.get("threads", 1)

        prev_exec = shell._executable
        prev_prefix = shell._prefix
        prev_suffix = shell._suffix
        if req.get("shell_exec"):
            shell._executable = req["shell_exec"]
        if req.get("shell_prefix") is not None:
            shell._prefix = req["shell_prefix"]
        if req.get("shell_suffix") is not None:
            shell._suffix = req["shell_suffix"]

        prev_cwd = os.getcwd()
        shadow_dir = req.get("shadow_dir")
        try:
            if shadow_dir:
                os.chdir(shadow_dir)
            robj.run_func(
                inp, outp, prm, wc, threads, res_nl, log_nl,
                None, robj.name, robj.ruleinfo.conda_env, robj.ruleinfo.container_img,
                "", False, robj.ruleinfo.env_modules, None, req.get("jobid", 0),
                False, 0, True, shadow_dir, None, None, robj.basedir, None, None, None,
            )
        finally:
            os.chdir(prev_cwd)
            shell._executable = prev_exec
            shell._prefix = prev_prefix
            shell._suffix = prev_suffix
        return {"ok": True}

    if cmd == "exec_hook":
        wf = _ACTIVE_WORKFLOW
        hook = req["hook"]
        logfile = req.get("logfile", "")
        fn = getattr(wf, f"_{hook}", None)
        if fn is not None:
            fn(logfile)
        return {"ok": True}

    if cmd == "shutdown":
        return {"ok": True, "shutdown": True}

    return {"error": f"Unknown command: {cmd}"}


def main():
    while True:
        try:
            req = _rpc_recv()
        except EOFError:
            break
        try:
            resp = _handle_cmd(req)
        except Exception as e:
            resp = {
                "error": f"{type(e).__name__}: {e}",
                "exc_type": type(e).__name__,
                "traceback": traceback.format_exc(),
            }
        _rpc_send(resp)
        if resp.get("shutdown"):
            break


if __name__ == "__main__":
    main()
