//! `current_repodata.json` derived from a served `repodata.json` (#4708).
//!
//! `current_repodata.json` is the document conda (classic solver and libmamba)
//! asks for first: a filter of `repodata.json` small enough to solve against
//! quickly, falling back to the full document only on a 404. A hosted channel
//! builds it from its own rows ([`super::build_repodata`] with
//! `latest_only`); a virtual channel has no rows of its own, so it derives the
//! document from the merged `repodata.json` it serves, which already carries
//! the hosted-name ownership guard and the allowlist. Deriving from the served
//! document rather than re-merging keeps the two in step by construction: a
//! record `current_repodata.json` lists is always one `repodata.json` lists.
//!
//! The filter is conda-index's (`build_current_repodata`):
//!
//! 1. per package name, every record (any build, either archive format) of
//!    the newest version, by conda's own version order;
//! 2. then, until nothing changes, for each dependency of a kept record whose
//!    package is in the document but none of whose kept records satisfies the
//!    spec, every record of the newest version that does satisfy it.
//!
//! A dependency on a package the document does not carry (a virtual package
//! such as `__glibc`, or one another channel provides) adds nothing. Records
//! are carried as raw JSON and never re-serialized, so each kept record is
//! byte-identical to the `repodata.json` record.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rattler_conda_types::{MatchSpec, PackageNameMatcher, ParseStrictness, Version};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

/// One package map of a repodata document, records kept raw.
type PackageMap<'a> = BTreeMap<String, &'a RawValue>;

/// A top-level value of the derived document: carried through from the input
/// unchanged, or one of the two filtered package maps.
#[derive(Serialize)]
#[serde(untagged)]
enum Part<'a> {
    Raw(&'a RawValue),
    Packages(BTreeMap<&'a str, &'a RawValue>),
}

/// Parse the package map under `key` of the top-level document, if any.
fn package_map<'a>(
    top: &BTreeMap<String, &'a RawValue>,
    key: &str,
) -> Result<PackageMap<'a>, String> {
    match top.get(key) {
        Some(raw) => serde_json::from_str(raw.get()).map_err(|e| format!("{key}: {e}")),
        None => Ok(BTreeMap::new()),
    }
}

/// The fields of a record the filter reads.
#[derive(Deserialize)]
struct RecordFields {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    build: Option<String>,
    #[serde(default)]
    depends: Vec<String>,
}

/// One record, located by map (`false`: `packages`, `true`: `packages.conda`)
/// and filename.
struct Record {
    v2: bool,
    filename: String,
    version: Option<Version>,
    build: String,
    depends: Vec<String>,
}

/// The kept set: record indices, plus each kept name's indices for the
/// satisfaction check.
#[derive(Default)]
struct Kept {
    records: BTreeSet<usize>,
    by_name: HashMap<String, Vec<usize>>,
}

impl Kept {
    fn insert(&mut self, name: &str, index: usize) -> bool {
        if !self.records.insert(index) {
            return false;
        }
        self.by_name
            .entry(name.to_string())
            .or_default()
            .push(index);
        true
    }
}

/// Whether `record` satisfies the version and build parts of `spec`. A
/// version-constrained spec is never satisfied by a record whose version does
/// not parse.
fn satisfies(spec: &MatchSpec, record: &Record) -> bool {
    let version_ok = match (&spec.version, &record.version) {
        (None, _) => true,
        (Some(vs), Some(v)) => vs.matches(v),
        (Some(_), None) => false,
    };
    version_ok && spec.build.as_ref().is_none_or(|b| b.matches(&record.build))
}

/// The indices among `candidates` with the newest version. When no candidate
/// version parses there is no order to pick by, and every candidate is kept.
fn newest(records: &[Record], candidates: impl Iterator<Item = usize> + Clone) -> Vec<usize> {
    let max = candidates
        .clone()
        .filter_map(|i| records[i].version.as_ref())
        .max()
        .cloned();
    candidates
        .filter(|&i| match &max {
            Some(max) => records[i].version.as_ref() == Some(max),
            None => true,
        })
        .collect()
}

/// Derive `current_repodata.json` from a `repodata.json` document, pretty
/// printed like every other JSON document the channel serves. Fails only when
/// the input is not a repodata document.
pub(super) fn derive(repodata: &[u8]) -> Result<Vec<u8>, String> {
    let top: BTreeMap<String, &RawValue> =
        serde_json::from_slice(repodata).map_err(|e| format!("repodata does not parse: {e}"))?;
    let packages_in = package_map(&top, "packages")?;
    let conda_in = package_map(&top, "packages.conda")?;

    let mut records = Vec::with_capacity(packages_in.len() + conda_in.len());
    let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
    for (v2, map) in [(false, &packages_in), (true, &conda_in)] {
        for (filename, raw) in map {
            let fields: RecordFields = serde_json::from_str(raw.get())
                .map_err(|e| format!("record {filename} does not parse: {e}"))?;
            let Some(name) = fields
                .name
                .or_else(|| super::conda_name_from_filename(filename).map(str::to_string))
            else {
                continue;
            };
            let version = fields
                .version
                .as_deref()
                .and_then(crate::services::conda_semantics::parse_version);
            by_name
                .entry(name.to_ascii_lowercase())
                .or_default()
                .push(records.len());
            records.push(Record {
                v2,
                filename: filename.clone(),
                version,
                build: fields.build.unwrap_or_default(),
                depends: fields.depends,
            });
        }
    }

    let mut kept = Kept::default();
    let mut queue = Vec::new();
    for (name, indices) in &by_name {
        for i in newest(&records, indices.iter().copied()) {
            kept.insert(name, i);
            queue.push(i);
        }
    }
    while let Some(i) = queue.pop() {
        for dep in records[i].depends.clone() {
            let Ok(spec) = MatchSpec::from_str(&dep, ParseStrictness::Lenient) else {
                continue;
            };
            let PackageNameMatcher::Exact(dep_name) = &spec.name else {
                continue;
            };
            let dep_name = dep_name.as_normalized().to_ascii_lowercase();
            let Some(all) = by_name.get(&dep_name) else {
                continue;
            };
            let already = kept
                .by_name
                .get(&dep_name)
                .is_some_and(|ks| ks.iter().any(|&k| satisfies(&spec, &records[k])));
            if already {
                continue;
            }
            let matching = all
                .iter()
                .copied()
                .filter(|&k| satisfies(&spec, &records[k]));
            for k in newest(&records, matching) {
                if kept.insert(&dep_name, k) {
                    queue.push(k);
                }
            }
        }
    }

    let mut packages: BTreeMap<&str, &RawValue> = BTreeMap::new();
    let mut packages_conda: BTreeMap<&str, &RawValue> = BTreeMap::new();
    for &i in &kept.records {
        let r = &records[i];
        let (source, target) = if r.v2 {
            (&conda_in, &mut packages_conda)
        } else {
            (&packages_in, &mut packages)
        };
        target.insert(r.filename.as_str(), source[&r.filename]);
    }

    let mut out: BTreeMap<&str, Part<'_>> = top
        .iter()
        .map(|(key, raw)| (key.as_str(), Part::Raw(raw)))
        .collect();
    out.insert("packages", Part::Packages(packages));
    out.insert("packages.conda", Part::Packages(packages_conda));
    serde_json::to_vec_pretty(&out).map_err(|e| e.to_string())
}
