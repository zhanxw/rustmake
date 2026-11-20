use std::collections::{HashMap, HashSet};
use std::path::Path;

use rustmake::error::RustmakeError;
use rustmake::io::{
    AnnotatedString, CompiledPattern, ExpandValue, IOFile, InputSizesCache, Combinator,
    apply_wildcards, directory, expand, match_wildcard_regex, multiext, temp,
};
use rustmake::ioutils::{is_local_file, subpath};
use rustmake::output_index::{OutputIndex, ProductPattern};
use rustmake::pathvars::Pathvars;
use rustmake::prefix_lookup::PrefixLookup;
use rustmake::profiles::{ProfileValue, parse_profile_yaml};
use rustmake::resources::{ResourceValue, is_ordinary_string, parse_set_resources};
use rustmake::schema::validate;
use rustmake::settings::{Batch, JobRateLimiter, MaxJobsPerTimespan};
use rustmake::sourcecache::{SourceFile, infer_source_file};
use rustmake::yaml::{parse_yaml_or_json, update_config};
use serde_json::json;
use tempfile::TempDir;

// ---- test_expand.py ----

#[test]
fn test_simple_expand() {
    let mut vals = HashMap::new();
    vals.insert("a".into(), ExpandValue::Single(json!("test")));
    let res = expand(&[AnnotatedString::new("{a}.out")], Combinator::Product, &vals, false)
        .unwrap()
        .into_iter()
        .map(|s| s.value)
        .collect::<Vec<_>>();
    assert_eq!(res, vec!["test.out"]);

    let mut vals = HashMap::new();
    vals.insert("a".into(), ExpandValue::Single(json!("a")));
    vals.insert("b".into(), ExpandValue::Single(json!("b")));
    let res = expand(
        &[AnnotatedString::new("{a}.out"), AnnotatedString::new("{b}.out")],
        Combinator::Product,
        &vals,
        false,
    )
    .unwrap()
    .into_iter()
    .map(|s| s.value)
    .collect::<Vec<_>>();
    assert_eq!(res, vec!["a.out", "b.out"]);

    let mut vals = HashMap::new();
    vals.insert(
        "a".into(),
        ExpandValue::List(vec![json!("1"), json!("2"), json!("3")]),
    );
    let res = expand(&[AnnotatedString::new("{a}.out")], Combinator::Product, &vals, false)
        .unwrap()
        .into_iter()
        .map(|s| s.value)
        .collect::<Vec<_>>();
    assert_eq!(res, vec!["1.out", "2.out", "3.out"]);

    let mut vals = HashMap::new();
    vals.insert("a".into(), ExpandValue::List(vec![json!("1"), json!("2")]));
    vals.insert("b".into(), ExpandValue::List(vec![json!("3"), json!("4")]));
    let res_prod = expand(
        &[AnnotatedString::new("{a}_{b}.ab"), AnnotatedString::new("{b}.b")],
        Combinator::Product,
        &vals,
        false,
    )
    .unwrap()
    .into_iter()
    .map(|s| s.value)
    .collect::<Vec<_>>();
    assert_eq!(
        res_prod,
        vec!["1_3.ab", "1_4.ab", "2_3.ab", "2_4.ab", "3.b", "4.b"]
    );

    let res_zip = expand(
        &[AnnotatedString::new("{a}_{b}.ab"), AnnotatedString::new("{b}.b")],
        Combinator::Zip,
        &vals,
        false,
    )
    .unwrap()
    .into_iter()
    .map(|s| s.value)
    .collect::<Vec<_>>();
    assert_eq!(res_zip, vec!["1_3.ab", "2_4.ab", "3.b", "4.b"]);
}

#[test]
fn test_allow_missing() {
    let empty = HashMap::new();
    let res = expand(&[AnnotatedString::new("{a}_{b}.out")], Combinator::Product, &empty, true)
        .unwrap()
        .into_iter()
        .map(|s| s.value)
        .collect::<Vec<_>>();
    assert_eq!(res, vec!["{a}_{b}.out"]);

    let mut vals = HashMap::new();
    vals.insert("a".into(), ExpandValue::Single(json!("test")));
    let res = expand(&[AnnotatedString::new("{a}_{b}.out")], Combinator::Product, &vals, true)
        .unwrap()
        .into_iter()
        .map(|s| s.value)
        .collect::<Vec<_>>();
    assert_eq!(res, vec!["test_{b}.out"]);

    let mut vals2 = HashMap::new();
    vals2.insert("allow_missing".into(), ExpandValue::Single(json!(true)));
    let res = expand(
        &[AnnotatedString::new("{allow_missing}.out")],
        Combinator::Product,
        &vals2,
        true,
    )
    .unwrap()
    .into_iter()
    .map(|s| s.value)
    .collect::<Vec<_>>();
    assert_eq!(res, vec!["True.out"]);

    let err = expand(&[AnnotatedString::new("{a}.out")], Combinator::Product, &empty, false)
        .unwrap_err();
    assert!(matches!(err, RustmakeError::Wildcard(_)));

    let mut vals3 = HashMap::new();
    vals3.insert("a".into(), ExpandValue::List(vec![json!("1"), json!("2")]));
    vals3.insert("b".into(), ExpandValue::List(vec![json!("3"), json!("4")]));
    let res = expand(
        &[
            AnnotatedString::new("{a}_{b}_{C}.ab"),
            AnnotatedString::new("{b}_{c}.b"),
        ],
        Combinator::Zip,
        &vals3,
        true,
    )
    .unwrap()
    .into_iter()
    .map(|s| s.value)
    .collect::<Vec<_>>();
    assert_eq!(res, vec!["1_3_{C}.ab", "2_4_{C}.ab", "3_{c}.b", "4_{c}.b"]);
}

// ---- test_io.py ----

#[test]
fn test_wildcard_regex() {
    let m = |s: &str| {
        match_wildcard_regex(s)
            .into_iter()
            .map(|w| (w.name, w.constraint))
            .collect::<Vec<_>>()
    };

    assert_eq!(m(""), vec![]);
    assert_eq!(m("{"), vec![]);
    assert_eq!(m("}"), vec![]);
    assert_eq!(m("{}"), vec![]);
    assert_eq!(m("{0}"), vec![("0".into(), None)]);
    assert_eq!(m("{abc}"), vec![("abc".into(), None)]);
    assert_eq!(m("abc{def}{ghi}"), vec![("def".into(), None), ("ghi".into(), None)]);
    assert_eq!(m("{w,constraint}"), vec![("w".into(), Some("constraint".into()))]);
    assert_eq!(m("{w , constraint}"), vec![("w".into(), Some("constraint".into()))]);
    assert_eq!(
        m("abc { w , constraint} def"),
        vec![("w".into(), Some("constraint".into()))]
    );
    assert_eq!(
        m("{a,1} {b,2} {c,3}"),
        vec![
            ("a".into(), Some("1".into())),
            ("b".into(), Some("2".into())),
            ("c".into(), Some("3".into())),
        ]
    );
    assert_eq!(
        m(r"{w,([a-z]+|pat\|t*ern)}"),
        vec![("w".into(), Some(r"([a-z]+|pat\|t*ern)".into()))]
    );
    assert_eq!(
        m(r"{w,([a-z]+|pat\|te{1,3}rn){5,7}}"),
        vec![("w".into(), Some(r"([a-z]+|pat\|te{1,3}rn){5,7}".into()))]
    );
    assert_eq!(m("{w, long constraint without closing brace"), vec![]);
}

#[test]
fn test_duplicate_wildcard_pattern_compile() {
    let pat = CompiledPattern::compile("{sample}/{sample}.txt").unwrap();
    let ok = pat.match_target("foo/foo.txt").unwrap();
    assert_eq!(ok.get("sample").map(|s| s.as_str()), Some("foo"));
    assert!(pat.match_target("foo/bar.txt").is_none());
}

#[test]
fn test_expand_formatting_and_flags() {
    let mut vals = HashMap::new();
    vals.insert(
        "samples".into(),
        ExpandValue::List(vec![json!("A123"), json!("B456"), json!("C789")]),
    );
    let res: Vec<String> = expand(
        &[AnnotatedString::new("first letter of sample: {samples[0]}")],
        Combinator::Product,
        &vals,
        false,
    )
    .unwrap()
    .into_iter()
    .map(|s| s.value)
    .collect();
    assert_eq!(
        res,
        vec![
            "first letter of sample: A",
            "first letter of sample: B",
            "first letter of sample: C",
        ]
    );

    let mut ans_vals = HashMap::new();
    ans_vals.insert("answer".into(), ExpandValue::List(vec![json!(41), json!(42)]));
    let res: Vec<String> = expand(
        &[AnnotatedString::new(
            "The answer to life, the universe, and everything: {answer:f}",
        )],
        Combinator::Product,
        &ans_vals,
        false,
    )
    .unwrap()
    .into_iter()
    .map(|s| s.value)
    .collect();
    assert_eq!(
        res,
        vec![
            "The answer to life, the universe, and everything: 41.000000",
            "The answer to life, the universe, and everything: 42.000000",
        ]
    );

    let mut fmt_vars = HashMap::new();
    fmt_vars.insert("sample".into(), json!("foo"));

    let flagged = IOFile::from_annotated(temp("results/{sample}.txt"), None);
    let formatted = flagged.format(&fmt_vars).unwrap();
    assert_eq!(formatted.file, "results/foo.txt");
    assert!(formatted.is_flagged("temp"));

    let multi = IOFile::from_annotated(
        AnnotatedString::with_flags(
            "results/{sample}",
            directory(temp("x").value).flags,
        ),
        None,
    );
    let mut merged = temp("results/{sample}");
    merged.flags.directory = true;
    let multi_io = IOFile::from_annotated(merged.clone(), None);
    let fmt_multi = multi_io.format(&fmt_vars).unwrap();
    assert_eq!(fmt_multi.file, "results/foo");
    assert!(fmt_multi.is_flagged("temp"));
    assert!(fmt_multi.is_flagged("directory"));
    let _ = multi;

    let mut storage_ann = AnnotatedString::new("results/{sample}.txt");
    storage_ann.flags.storage_object = Some("s3://bucket/{sample}.txt".into());
    let err_ann = storage_ann.format(&fmt_vars).unwrap_err();
    assert!(!err_ann.to_string().contains("apply_wildcards"));

    let storage_io = IOFile::from_annotated(storage_ann, None);
    let err_io = storage_io.format(&fmt_vars).unwrap_err();
    assert!(err_io.to_string().contains("apply_wildcards"));
}

#[test]
fn test_multiext_and_apply_wildcards() {
    let items = multiext("results/sample", &[".txt".into(), ".csv".into()], &[]).unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].value, "results/sample.txt");
    assert!(items[0].flags.multiext.is_some());

    let mut wc = HashMap::new();
    wc.insert("sample".into(), "S1".into());
    assert_eq!(apply_wildcards("a/{sample}.txt", &wc).unwrap(), "a/S1.txt");
}

// ---- test_prefix_lookup.py ----

#[test]
fn test_prefix_lookup_suite() {
    let lookup = PrefixLookup::new(vec![("a".into(), 1), ("b".into(), 2)]);
    assert_eq!(lookup.match_set("a"), HashSet::from([1]));
    assert_eq!(lookup.match_set("b"), HashSet::from([2]));
    assert_eq!(lookup.match_set("c"), HashSet::new());

    let lookup2 = PrefixLookup::new(vec![
        ("".into(), 0),
        ("a".into(), 1),
        ("ab".into(), 2),
        ("abc".into(), 3),
    ]);
    assert_eq!(lookup2.match_set("abcd"), HashSet::from([0, 1, 2, 3]));
    assert_eq!(lookup2.match_set("abc"), HashSet::from([0, 1, 2, 3]));
    assert_eq!(lookup2.match_set("ab"), HashSet::from([0, 1, 2]));
    assert_eq!(lookup2.match_set("a"), HashSet::from([0, 1]));
    assert_eq!(lookup2.match_set(""), HashSet::from([0]));

    let empty: PrefixLookup<i32> = PrefixLookup::new(Vec::<(String, i32)>::new());
    assert_eq!(empty.match_set("abc"), HashSet::new());

    let unicode = PrefixLookup::new(vec![
        ("é".into(), 1),
        ("ñ".into(), 2),
        ("🌟".into(), 3),
        ("é日本".into(), 4),
    ]);
    assert_eq!(unicode.match_set("étest"), HashSet::from([1]));
    assert_eq!(unicode.match_set("ñtest"), HashSet::from([2]));
    assert_eq!(unicode.match_set("🌟test"), HashSet::from([3]));
    assert_eq!(unicode.match_set("é日本語"), HashSet::from([1, 4]));

    let non_consec = PrefixLookup::new(vec![
        ("a".into(), 1),
        ("ab".into(), 2),
        ("abbcc".into(), 3),
        ("abbc".into(), 4),
        ("abbd".into(), 5),
    ]);
    assert_eq!(non_consec.match_set("abbz"), HashSet::from([1, 2]));
    assert_eq!(non_consec.match_set("abbc"), HashSet::from([1, 2, 4]));
    assert_eq!(non_consec.match_set("abbd"), HashSet::from([1, 2, 5]));
    assert_eq!(non_consec.match_set("abbcc"), HashSet::from([1, 2, 3, 4]));
}

// ---- test_output_index.py ----

#[derive(Clone)]
struct SimpleProd(String, String);
impl ProductPattern for SimpleProd {
    fn constant_prefix(&self) -> String { self.0.clone() }
    fn constant_suffix(&self) -> String { self.1.clone() }
}

#[test]
fn test_output_index_suite() {
    let prods = |r: &&str| -> Vec<SimpleProd> {
        match *r {
            "rule_0" => vec![
                SimpleProd("test".into(), "-something.txt".into()),
                SimpleProd("test".into(), "-something.csv".into()),
            ],
            "rule_1" => vec![SimpleProd("other/file-".into(), "-something.csv".into())],
            "rule_2" => vec![SimpleProd("other/file-dupe-".into(), "-something.csv".into())],
            "empty_suffix" => vec![SimpleProd("test".into(), "".into())],
            "empty_prefix" => vec![SimpleProd("".into(), "something.txt".into())],
            _ => vec![],
        }
    };
    let idx = OutputIndex::new(vec!["rule_0", "rule_1", "rule_2"], prods);
    assert_eq!(idx.match_rules("test-{wildcard}-something.txt"), HashSet::from(["rule_0"]));
    assert_eq!(idx.match_rules("test-something.txt"), HashSet::from(["rule_0"]));
    assert_eq!(idx.match_rules("other/file--something.csv"), HashSet::from(["rule_1"]));
    assert_eq!(idx.match_rules("other/not-file-something.csv"), HashSet::new());
    assert_eq!(
        idx.match_rules("other/file-dupe-something.csv"),
        HashSet::from(["rule_1", "rule_2"])
    );

    let idx2 = OutputIndex::new(vec!["empty_suffix", "empty_prefix"], prods);
    assert_eq!(idx2.match_rules("test-something"), HashSet::from(["empty_suffix"]));
    assert_eq!(idx2.match_rules("something"), HashSet::new());
    assert_eq!(idx2.match_rules("something.txt"), HashSet::from(["empty_prefix"]));
    assert_eq!(
        idx2.match_rules("test-something.txt"),
        HashSet::from(["empty_suffix", "empty_prefix"])
    );
}

// ---- test_args.py ----

#[test]
fn test_batch_and_max_jobs_per_timespan() {
    let b = Batch::parse("aggregate=1/2").unwrap();
    assert_eq!(b, Batch::new("aggregate", 1, 2).unwrap());
    assert_eq!(b.get_batch(&["c", "a", "b"]).unwrap(), vec!["a", "b"]);
    assert_eq!(
        Batch::new("aggregate", 2, 2).unwrap().get_batch(&["c", "a", "b"]).unwrap(),
        vec!["c"]
    );

    for (arg, mj, ts) in [
        ("100/1s", 100usize, 1.0f64),
        ("50/1m", 50, 60.0),
        ("10/2h", 10, 7200.0),
        ("100/1ms", 100, 0.001),
        ("0.5/1s", 1, 2.0),
        ("2.5/1m", 5, 120.0),
    ] {
        let lim = MaxJobsPerTimespan::parse_choice(arg).unwrap();
        assert_eq!(lim.max_jobs, mj, "for {arg}");
        assert!((lim.timespan - ts).abs() < 1e-9, "for {arg}");
    }

    for invalid in ["1/1x", "1/1msx", "1/s", "a/1s", ".5/1s", "1/1s "] {
        assert!(MaxJobsPerTimespan::parse_choice(invalid).is_err(), "expected error for {invalid}");
    }
}

// ---- test_internals.py ----

#[test]
fn test_subpath_and_local_file_and_pathvars() {
    assert_eq!(subpath("test.txt", Some(".txt"), None, false, false, None).unwrap(), "test");
    assert_eq!(subpath("test.txt", None, None, true, false, None).unwrap(), "test.txt");
    assert_eq!(subpath("test.txt", None, None, false, true, None).unwrap(), ".");
    assert_eq!(subpath("test.txt", None, None, false, false, Some(1)).unwrap(), ".");
    assert_eq!(subpath("results/foo/test.txt", None, None, false, true, None).unwrap(), "results/foo");
    assert_eq!(subpath("results/foo/test.txt", None, None, false, false, Some(2)).unwrap(), "results");
    assert_eq!(subpath("results/foo/test.txt", None, None, false, false, Some(3)).unwrap(), ".");

    assert!(is_local_file("Snakefile", false));
    assert!(is_local_file("/tmp/Snakefile", false));
    assert!(is_local_file("file:///tmp/Snakefile", false));
    assert!(is_local_file("file:Snakefile", false));
    assert!(is_local_file("C:/workflow/Snakefile", true));
    assert!(is_local_file(r"C:\workflow\Snakefile", true));
    assert!(!is_local_file("https://example.com/workflow/Snakefile", false));
    assert!(!is_local_file("gh:owner/repo@main", false));
    assert!(!is_local_file("gl:group/project@main", false));
    assert!(!is_local_file("s3://bucket/workflow/Snakefile", false));

    let pv_cfg = Pathvars::from_config(&json!({"pathvars": {"results": "myresults"}}), false).unwrap();
    assert!(matches!(
        pv_cfg.apply("<bar>/log.txt"),
        Err(RustmakeError::UndefinedPathvar(_))
    ));

    let mut pv = Pathvars::with_defaults();
    assert_eq!(pv.apply("<results>/foo.txt").unwrap(), "results/foo.txt");
    pv.update(&Pathvars::from_workflow(HashMap::from([("results".into(), "wf_results".into())])).unwrap()).unwrap();
    assert_eq!(pv.get("results"), Some("wf_results"));
    pv.update(&Pathvars::from_config(&json!({"pathvars": {"logs": "cfg_logs"}}), false).unwrap()).unwrap();
    assert_eq!(pv.get("logs"), Some("cfg_logs"));
    pv.update(&Pathvars::from_module(HashMap::from([("results".into(), "mod_results".into()), ("logs".into(), "mod_logs".into())])).unwrap()).unwrap();
    assert_eq!(pv.get("results"), Some("mod_results"));
    pv.update(&Pathvars::from_rule(HashMap::from([("logs".into(), "rule_logs".into())])).unwrap()).unwrap();
    assert_eq!(pv.get("logs"), Some("rule_logs"));
    assert_eq!(pv.apply("<results>/foo.txt").unwrap(), "mod_results/foo.txt");
    assert_eq!(pv.apply("<logs>/foo.txt").unwrap(), "rule_logs/foo.txt");
}

#[test]
fn test_profile_parse_and_resources_and_rate_limiter() {
    let profile = "set-resources:\n    rule1:\n        slurm_partition: \"42\"\n";
    let parsed = parse_profile_yaml(profile, Path::new("foo/config.yaml")).unwrap();
    assert_eq!(
        parsed.get("set-resources"),
        Some(&ProfileValue::List(vec!["rule1:slurm_partition='42'".to_string()]))
    );
    if let Some(ProfileValue::List(items)) = parsed.get("set-resources") {
        let res = parse_set_resources(items).unwrap();
        let r = res.get("rule1").unwrap().data.get("slurm_partition").unwrap();
        assert_eq!(r.value, ResourceValue::Expression("'42'".to_string()));
    }

    assert!(is_ordinary_string("hello"));
    assert!(is_ordinary_string("123"));
    assert!(is_ordinary_string("simple_string"));
    assert!(!is_ordinary_string("func_name(arg1, arg2)"));
    assert!(!is_ordinary_string("{'key': 'value'}"));
    assert!(!is_ordinary_string("lambda x: x + 1"));
    assert!(!is_ordinary_string("2 * input.size"));

    let mut limiter = JobRateLimiter::new(MaxJobsPerTimespan::new(2, 10.0));
    assert!((limiter.time_until_free_at(100.0) - 10.0).abs() < 1e-9);
    limiter.register_jobs_at(1, 100.0);
    limiter.register_jobs_at(1, 104.0);
    assert_eq!(limiter.get_free_jobs_at(104.0), 0);
    assert!((limiter.time_until_free_at(104.0) - 6.0).abs() < 1e-9);
    assert_eq!(limiter.get_free_jobs_at(110.0), 1);
    assert!((limiter.time_until_free_at(110.0) - 4.0).abs() < 1e-9);

    // InputSizesCache
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a"), "abc").unwrap();
    std::fs::write(tmp.path().join("empty"), "").unwrap();
    let a = tmp.path().join("a").to_string_lossy().into_owned();
    let empty = tmp.path().join("empty").to_string_lossy().into_owned();
    let missing = tmp.path().join("missing").to_string_lossy().into_owned();
    let mut cache = InputSizesCache::new(false);
    cache.update_input_sizes(
        &[vec![a.clone(), empty.clone()], vec![a.clone(), missing.clone()]],
        InputSizesCache::measure_on_disk,
    );
    cache.update_input_sizes(
        &[vec![a.clone(), empty.clone()], vec![a.clone(), missing.clone()]],
        InputSizesCache::measure_on_disk,
    );
    assert_eq!(cache.stat_calls.get(&a), Some(&1));
    assert_eq!(cache.sizes.get(&a), Some(&Some(3)));
    assert_eq!(cache.sizes.get(&empty), Some(&Some(0)));
    assert_eq!(cache.sizes.get(&missing), Some(&None));
}

// ---- test_sourcecache.py ----

#[test]
fn test_sourcecache_shorthands() {
    let gl = SourceFile::Gitlab {
        repo: "owner/repo".into(),
        path: "parent/path".into(),
        branch: "tag".into(),
        host: "host.com".into(),
    };
    if let SourceFile::Gitlab { host, .. } = gl.get_basedir() {
        assert_eq!(host, "host.com");
    } else {
        panic!("expected gitlab");
    }
    if let SourceFile::Gitlab { host, .. } = gl.join("another/path") {
        assert_eq!(host, "host.com");
    } else {
        panic!("expected gitlab");
    }

    assert_eq!(
        infer_source_file("gh:snakemake/snakemake@main:workflow/Snakefile", None).unwrap(),
        SourceFile::Github {
            repo: "snakemake/snakemake".into(),
            path: "workflow/Snakefile".into(),
            branch: "main".into(),
            host: "github.com".into(),
        }
    );
    assert_eq!(
        infer_source_file("gh:snakemake/snakemake@main", None).unwrap(),
        SourceFile::Github {
            repo: "snakemake/snakemake".into(),
            path: "workflow/Snakefile".into(),
            branch: "main".into(),
            host: "github.com".into(),
        }
    );
    assert_eq!(
        infer_source_file("gh:github.example.com:snakemake/snakemake@main:workflow/Snakefile", None).unwrap(),
        SourceFile::Github {
            repo: "snakemake/snakemake".into(),
            path: "workflow/Snakefile".into(),
            branch: "main".into(),
            host: "github.example.com".into(),
        }
    );
    assert_eq!(
        infer_source_file("gl:gitlab.cern.ch:group/subgroup/project@main:workflow/Snakefile", None).unwrap(),
        SourceFile::Gitlab {
            repo: "group/subgroup/project".into(),
            path: "workflow/Snakefile".into(),
            branch: "main".into(),
            host: "gitlab.cern.ch".into(),
        }
    );
    assert!(matches!(
        infer_source_file("gh::snakemake/snakemake@main", None).unwrap(),
        SourceFile::Generic { .. }
    ));
    assert!(matches!(
        infer_source_file("gh:notarepo@main", None).unwrap(),
        SourceFile::Generic { .. }
    ));
    let f = infer_source_file("gh:owner/repo@perf/autobump:path/to/file.txt", None).unwrap();
    assert!(f.get_cache_path().contains("%2F"));
}

// ---- test_schema.py & yaml ----

#[test]
fn test_schema_validation_and_yaml() {
    let dir = TempDir::new().unwrap();
    let cfg_schema = dir.path().join("config.schema.yaml");
    std::fs::write(
        &cfg_schema,
        r#"$schema: "https://json-schema.org/draft/2020-12/schema#"
type: object
properties:
  param:
    type: object
    default: {}
    properties:
      foo:
        type: string
        default: bar
"#,
    )
    .unwrap();

    let mut cfg = json!({});
    validate(&mut cfg, &cfg_schema, false).unwrap();
    assert_eq!(cfg, json!({}));
    validate(&mut cfg, &cfg_schema, true).unwrap();
    assert_eq!(cfg, json!({"param": {"foo": "bar"}}));

    // Nested relative $ref with remote $id + fragment
    let bar_schema = dir.path().join("bar.schema.yaml");
    let baz_schema = dir.path().join("baz.schema.yaml");
    let rel_schema = dir.path().join("rel.schema.yaml");
    std::fs::write(
        &baz_schema,
        r#"type: object
properties:
  qux:
    type: integer
    default: 42
required: ["qux"]
"#,
    )
    .unwrap();
    std::fs::write(
        &bar_schema,
        r#"type: object
properties:
  baz:
    $ref: "baz.schema.yaml"
required: ["baz"]
"#,
    )
    .unwrap();
    std::fs::write(
        &rel_schema,
        r#"$id: "https://example.com/config.schema.yaml"
type: object
properties:
  bar:
    $ref: "bar.schema.yaml"
"#,
    )
    .unwrap();

    let mut bad = json!({"bar": {"baz": {}}});
    assert!(validate(&mut bad, &rel_schema, false).is_err());
    let mut good = json!({"bar": {"baz": {"qux": 19}}});
    validate(&mut good, &rel_schema, false).unwrap();

    // allOf default
    let allof_schema = dir.path().join("allof.schema.yaml");
    std::fs::write(
        &allof_schema,
        r#"type: object
properties:
  foo:
    allOf:
      - type: object
        properties:
          bar:
            type: integer
            default: 42
      - type: object
        properties:
          foo:
            type: string
            default: "foo"
        required: ["foo"]
"#,
    )
    .unwrap();
    let mut allof_cfg = json!({"foo": {}});
    validate(&mut allof_cfg, &allof_schema, true).unwrap();
    assert_eq!(allof_cfg["foo"]["bar"], json!(42));
    assert_eq!(allof_cfg["foo"]["foo"], json!("foo"));

    // YAML multiline plain scalar & update_config
    let yml = parse_yaml_or_json(
        "cluster:\n  mkdir -p logs && \n  bash run.sh\ncountries:\n  - fr\n  - at\n",
        None,
    )
    .unwrap();
    assert_eq!(yml["cluster"], json!("mkdir -p logs && bash run.sh"));
    assert_eq!(yml["countries"], json!(["fr", "at"]));

    let mut base = json!({"block": {"foo": "a", "bar": "b"}});
    let ovr = json!({"block": {"foo": "c", "baz": "d"}});
    update_config(&mut base, &ovr);
    assert_eq!(base, json!({"block": {"foo": "c", "bar": "b", "baz": "d"}}));
}
