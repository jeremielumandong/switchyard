//! Terraform / OpenTofu state (format version 4), read-only: parsing, a resource tree by
//! module and type, flattened instance attributes with sensitive values marked, search,
//! and a comparison of two states.
//!
//! Nothing here writes state. Sensitive values are kept (the viewer reveals them on
//! request) but every row says whether it is sensitive, so callers mask by default.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde_json::Value as Json;

use crate::error::{CloudError, Result};

/// Attribute names that are masked even when the provider did not mark them sensitive.
const SECRET_HINTS: [&str; 8] = [
    "password",
    "secret",
    "private_key",
    "token",
    "connection_string",
    "access_key",
    "passphrase",
    "credentials",
];

/// A parsed state file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TfState {
    /// State format version (always 4 here).
    pub version: u64,
    /// Terraform (or OpenTofu) version that wrote it.
    pub terraform_version: String,
    /// Increases with every write.
    pub serial: u64,
    /// Identifies one state's history; two states with different lineages are unrelated.
    pub lineage: String,
    /// Root module outputs, sorted by name.
    pub outputs: Vec<TfOutput>,
    /// Resources and data sources, in file order.
    pub resources: Vec<TfResource>,
}

/// A root module output.
#[derive(Clone, Debug, PartialEq)]
pub struct TfOutput {
    /// Name.
    pub name: String,
    /// Value as JSON.
    pub value: Json,
    /// Terraform type, as written (`"string"`, `["list","string"]`).
    pub type_label: String,
    /// Marked `sensitive = true`.
    pub sensitive: bool,
}

/// `resource` or `data`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TfMode {
    /// A managed resource.
    Managed,
    /// A data source.
    Data,
}

/// One `resource` or `data` block (all of its instances).
#[derive(Clone, Debug, PartialEq)]
pub struct TfResource {
    /// `module.a.module.b`, or empty for the root module.
    pub module: String,
    /// Managed resource or data source.
    pub mode: TfMode,
    /// Resource type (`aws_instance`).
    pub rtype: String,
    /// Resource name (`web`).
    pub name: String,
    /// Provider address (`provider["registry.terraform.io/hashicorp/aws"]`).
    pub provider: String,
    /// Instances (`count` / `for_each` make several).
    pub instances: Vec<TfInstance>,
}

impl TfResource {
    /// `module.x.aws_instance.web` / `data.aws_ami.ubuntu`, without instance keys.
    pub fn address(&self) -> String {
        let local = match self.mode {
            TfMode::Managed => format!("{}.{}", self.rtype, self.name),
            TfMode::Data => format!("data.{}.{}", self.rtype, self.name),
        };
        if self.module.is_empty() {
            local
        } else {
            format!("{}.{local}", self.module)
        }
    }

    /// The provider's short name (`hashicorp/aws`).
    pub fn provider_short(&self) -> String {
        let inner = self
            .provider
            .split_once("[\"")
            .and_then(|(_, r)| r.split_once("\"]"))
            .map_or(self.provider.as_str(), |(i, _)| i);
        inner
            .strip_prefix("registry.terraform.io/")
            .or_else(|| inner.strip_prefix("registry.opentofu.org/"))
            .unwrap_or(inner)
            .to_owned()
    }
}

/// One instance of a resource.
#[derive(Clone, Debug, PartialEq)]
pub struct TfInstance {
    /// `count` index or `for_each` key, as written in an address (`0`, `"a"`).
    pub index_key: Option<String>,
    /// Deposed object id (left over from create-before-destroy).
    pub deposed: Option<String>,
    /// `tainted`, when set.
    pub status: Option<String>,
    /// Provider schema version of the attributes.
    pub schema_version: u64,
    /// Attributes as written.
    pub attributes: Json,
    /// Paths (as [`AttrRow::path`]) the provider marked sensitive.
    pub sensitive_paths: Vec<String>,
    /// Resource addresses this one depends on.
    pub dependencies: Vec<String>,
}

/// One flattened attribute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttrRow {
    /// `tags.Name`, `ingress[0].cidr_blocks[1]`.
    pub path: String,
    /// Display value: strings unquoted, `null`, numbers, `true`; empty `[]` / `{}`.
    pub value: String,
    /// Marked sensitive by the provider, or its name looks like a secret.
    pub sensitive: bool,
}

/// What a sensitive value shows until revealed.
pub const MASK: &str = "••••••••";

impl TfInstance {
    /// The instance's address under `resource`.
    pub fn address(&self, resource: &TfResource) -> String {
        let mut a = resource.address();
        if let Some(k) = &self.index_key {
            a.push('[');
            a.push_str(k);
            a.push(']');
        }
        if let Some(d) = &self.deposed {
            a.push_str(&format!(" (deposed {d})"));
        }
        a
    }

    /// Attributes flattened to leaf rows, in path order.
    pub fn rows(&self) -> Vec<AttrRow> {
        let mut out = Vec::new();
        flatten("", &self.attributes, &mut out);
        for r in &mut out {
            r.sensitive =
                self.sensitive_paths.iter().any(|s| covers(s, &r.path)) || looks_secret(&r.path);
        }
        out
    }

    /// The `id` attribute, when there is one.
    pub fn id(&self) -> Option<&str> {
        self.attributes.get("id").and_then(Json::as_str)
    }
}

/// Whether sensitive path `s` covers `path` (itself or anything below it).
fn covers(s: &str, path: &str) -> bool {
    path == s
        || (path.starts_with(s) && matches!(path.as_bytes().get(s.len()), Some(b'.') | Some(b'[')))
}

/// Whether the last named step of `path` looks like a secret (`password`, `auth_token`,
/// `private_key_pem`), leaving references and metadata (`secret_arn`, `token_expiry_days`).
fn looks_secret(path: &str) -> bool {
    let leaf = path
        .rsplit('.')
        .find(|s| !s.starts_with('['))
        .unwrap_or(path);
    let leaf = leaf.split('[').next().unwrap_or(leaf).to_ascii_lowercase();
    const METADATA: [&str; 9] = [
        "_arn", "_id", "_ids", "_name", "_version", "_expiry", "_days", "_enabled", "_type",
    ];
    if METADATA.iter().any(|m| leaf.ends_with(m)) {
        return false;
    }
    SECRET_HINTS.iter().any(|h| {
        leaf == *h || leaf.ends_with(&format!("_{h}")) || leaf.starts_with(&format!("{h}_"))
    })
}

fn scalar(v: &Json) -> String {
    match v {
        Json::Null => "null".into(),
        Json::String(s) => s.clone(),
        Json::Array(a) if a.is_empty() => "[]".into(),
        Json::Object(o) if o.is_empty() => "{}".into(),
        other => other.to_string(),
    }
}

fn flatten(prefix: &str, v: &Json, out: &mut Vec<AttrRow>) {
    match v {
        Json::Object(o) if !o.is_empty() => {
            for (k, v) in o {
                let p = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(&p, v, out);
            }
        }
        Json::Array(a) if !a.is_empty() => {
            for (i, v) in a.iter().enumerate() {
                flatten(&format!("{prefix}[{i}]"), v, out);
            }
        }
        other => out.push(AttrRow {
            path: prefix.to_owned(),
            value: scalar(other),
            sensitive: false,
        }),
    }
}

/// One step of a `sensitive_attributes` path as text.
fn step_text(step: &Json, out: &mut String) {
    let kind = step.get("type").and_then(Json::as_str).unwrap_or_default();
    let value = step.get("value");
    match (kind, value) {
        ("get_attr", Some(Json::String(name))) => {
            if !out.is_empty() {
                out.push('.');
            }
            out.push_str(name);
        }
        ("index", Some(v)) => {
            // `{"value": 0, "type": "number"}` or a bare value.
            let inner = v.get("value").unwrap_or(v);
            match inner {
                Json::Number(n) => out.push_str(&format!("[{n}]")),
                // Map keys flatten as `.key` (objects are flattened that way).
                Json::String(s) => {
                    if !out.is_empty() {
                        out.push('.');
                    }
                    out.push_str(s);
                }
                _ => {}
            }
        }
        _ => {}
    }
}

/// Sensitive paths in either layout Terraform has used: a list of step lists, or a list
/// of `{"path": [steps]}` / single steps.
fn sensitive_paths(v: Option<&Json>) -> Vec<String> {
    let Some(Json::Array(items)) = v else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let steps: Vec<&Json> = match item {
            Json::Array(steps) => steps.iter().collect(),
            Json::Object(o) => match o.get("path") {
                Some(Json::Array(steps)) => steps.iter().collect(),
                _ => vec![item],
            },
            _ => continue,
        };
        let mut p = String::new();
        for s in steps {
            step_text(s, &mut p);
        }
        if !p.is_empty() {
            out.push(p);
        }
    }
    out
}

fn index_key(v: Option<&Json>) -> Option<String> {
    match v? {
        Json::Null => None,
        Json::String(s) => Some(format!("{s:?}")),
        other => Some(other.to_string()),
    }
}

fn type_label(v: Option<&Json>) -> String {
    match v {
        Some(Json::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// Parse a state file.
pub fn parse(bytes: &[u8]) -> Result<TfState> {
    let json: Json = serde_json::from_slice(bytes)
        .map_err(|e| CloudError::Invalid(format!("not a Terraform state file (JSON): {e}")))?;
    let version = json.get("version").and_then(Json::as_u64).ok_or_else(|| {
        CloudError::Invalid("not a Terraform state file: no \"version\" field".into())
    })?;
    if version != 4 {
        return Err(CloudError::Invalid(format!(
            "state format version {version} is not supported (Terraform 0.12 and later write version 4)"
        )));
    }
    let s = |k: &str| {
        json.get(k)
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let mut outputs: Vec<TfOutput> = json
        .get("outputs")
        .and_then(Json::as_object)
        .map(|o| {
            o.iter()
                .map(|(name, out)| TfOutput {
                    name: name.clone(),
                    value: out.get("value").cloned().unwrap_or(Json::Null),
                    type_label: type_label(out.get("type")),
                    sensitive: out
                        .get("sensitive")
                        .and_then(Json::as_bool)
                        .unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default();
    outputs.sort_by(|a, b| a.name.cmp(&b.name));
    let resources = json
        .get("resources")
        .and_then(Json::as_array)
        .map(|rs| rs.iter().map(resource).collect())
        .unwrap_or_default();
    Ok(TfState {
        version,
        terraform_version: s("terraform_version"),
        serial: json.get("serial").and_then(Json::as_u64).unwrap_or(0),
        lineage: s("lineage"),
        outputs,
        resources,
    })
}

fn resource(r: &Json) -> TfResource {
    let s = |k: &str| {
        r.get(k)
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    TfResource {
        module: s("module"),
        mode: if s("mode") == "data" {
            TfMode::Data
        } else {
            TfMode::Managed
        },
        rtype: s("type"),
        name: s("name"),
        provider: s("provider"),
        instances: r
            .get("instances")
            .and_then(Json::as_array)
            .map(|is| {
                is.iter()
                    .map(|i| TfInstance {
                        index_key: index_key(i.get("index_key")),
                        deposed: i.get("deposed").and_then(Json::as_str).map(str::to_owned),
                        status: i.get("status").and_then(Json::as_str).map(str::to_owned),
                        schema_version: i.get("schema_version").and_then(Json::as_u64).unwrap_or(0),
                        attributes: i
                            .get("attributes")
                            .cloned()
                            .unwrap_or(Json::Object(Default::default())),
                        sensitive_paths: sensitive_paths(i.get("sensitive_attributes")),
                        dependencies: i
                            .get("dependencies")
                            .and_then(Json::as_array)
                            .map(|d| {
                                d.iter()
                                    .filter_map(Json::as_str)
                                    .map(str::to_owned)
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// An instance's position: (resource index, instance index).
pub type InstanceIx = (usize, usize);

impl TfState {
    /// Every instance with its address, in file order.
    pub fn instances(&self) -> impl Iterator<Item = (InstanceIx, String)> + '_ {
        self.resources.iter().enumerate().flat_map(|(ri, r)| {
            r.instances
                .iter()
                .enumerate()
                .map(move |(ii, i)| ((ri, ii), i.address(r)))
        })
    }

    /// The instance at `ix`.
    pub fn instance(&self, ix: InstanceIx) -> Option<(&TfResource, &TfInstance)> {
        let r = self.resources.get(ix.0)?;
        Some((r, r.instances.get(ix.1)?))
    }

    /// Instances matching `query` (case-insensitive) in their address, type, provider or
    /// a non-sensitive attribute value. An empty query matches everything.
    pub fn search(&self, query: &str) -> Vec<InstanceIx> {
        let q = query.trim().to_lowercase();
        self.instances()
            .filter(|(ix, addr)| {
                if q.is_empty() || addr.to_lowercase().contains(&q) {
                    return true;
                }
                let Some((r, i)) = self.instance(*ix) else {
                    return false;
                };
                r.provider.to_lowercase().contains(&q)
                    || i.rows().iter().any(|row| {
                        !row.sensitive
                            && (row.value.to_lowercase().contains(&q)
                                || row.path.to_lowercase().contains(&q))
                    })
            })
            .map(|(ix, _)| ix)
            .collect()
    }

    /// Managed resource and data source instance counts.
    pub fn counts(&self) -> (usize, usize) {
        let mut managed = 0;
        let mut data = 0;
        for r in &self.resources {
            match r.mode {
                TfMode::Managed => managed += r.instances.len(),
                TfMode::Data => data += r.instances.len(),
            }
        }
        (managed, data)
    }

    /// Resources that depend on `address` (a resource address without instance key).
    pub fn dependents(&self, address: &str) -> Vec<String> {
        let mut out = BTreeSet::new();
        for r in &self.resources {
            for i in &r.instances {
                if i.dependencies.iter().any(|d| d == address) {
                    out.insert(r.address());
                }
            }
        }
        out.into_iter().collect()
    }

    /// Rows of the resource tree: modules (root first), then types, then instances, with
    /// collapsed group keys (from [`TreeRow::Group::key`]) hidden. Only `only` instances
    /// are shown when given (a search result); groups without any are left out.
    pub fn tree(&self, only: Option<&[InstanceIx]>, collapsed: &HashSet<String>) -> Vec<TreeRow> {
        let allowed: Option<HashSet<InstanceIx>> = only.map(|o| o.iter().copied().collect());
        // module → type label → instances.
        let mut modules: BTreeMap<String, BTreeMap<String, Vec<(String, InstanceIx)>>> =
            BTreeMap::new();
        for (ri, r) in self.resources.iter().enumerate() {
            let tlabel = match r.mode {
                TfMode::Managed => r.rtype.clone(),
                TfMode::Data => format!("data.{}", r.rtype),
            };
            for (ii, i) in r.instances.iter().enumerate() {
                if allowed.as_ref().is_some_and(|a| !a.contains(&(ri, ii))) {
                    continue;
                }
                let mut label = r.name.clone();
                if let Some(k) = &i.index_key {
                    label.push_str(&format!("[{k}]"));
                }
                if let Some(d) = &i.deposed {
                    label.push_str(&format!(" (deposed {d})"));
                }
                modules
                    .entry(r.module.clone())
                    .or_default()
                    .entry(tlabel.clone())
                    .or_default()
                    .push((label, (ri, ii)));
            }
        }
        let mut out = Vec::new();
        for (module, types) in modules {
            let mkey = format!("m:{module}");
            let count = types.values().map(Vec::len).sum();
            let mopen = !collapsed.contains(&mkey);
            out.push(TreeRow::Group {
                key: mkey.clone(),
                label: if module.is_empty() {
                    "root module".into()
                } else {
                    module.clone()
                },
                depth: 0,
                count,
                open: mopen,
            });
            if !mopen {
                continue;
            }
            for (tlabel, mut items) in types {
                let tkey = format!("{mkey}/{tlabel}");
                let topen = !collapsed.contains(&tkey);
                out.push(TreeRow::Group {
                    key: tkey,
                    label: tlabel,
                    depth: 1,
                    count: items.len(),
                    open: topen,
                });
                if !topen {
                    continue;
                }
                items.sort_by(|a, b| a.0.cmp(&b.0));
                for (label, ix) in items {
                    out.push(TreeRow::Instance {
                        ix,
                        label,
                        depth: 2,
                    });
                }
            }
        }
        out
    }
}

/// A row of [`TfState::tree`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeRow {
    /// A module or a resource type.
    Group {
        /// Stable key for collapsing.
        key: String,
        /// Shown name.
        label: String,
        /// 0 for modules, 1 for types.
        depth: usize,
        /// Instances below.
        count: usize,
        /// Expanded.
        open: bool,
    },
    /// One resource instance.
    Instance {
        /// Its position in the state.
        ix: InstanceIx,
        /// `name[key]`.
        label: String,
        /// Indent level.
        depth: usize,
    },
}

// ------------------------------------------------------------------- compare

/// How an attribute changed between two states.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttrChange {
    /// Attribute path.
    pub path: String,
    /// Value in the first state (`None`: absent).
    pub old: Option<String>,
    /// Value in the second state (`None`: absent).
    pub new: Option<String>,
    /// Sensitive in either state.
    pub sensitive: bool,
}

/// One changed instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstanceChange {
    /// Instance address.
    pub address: String,
    /// Changed attributes (and `(dependencies)` / `(status)` pseudo-attributes).
    pub attrs: Vec<AttrChange>,
}

/// Differences between state A (older) and state B (newer).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StateDiff {
    /// Instances only in B.
    pub added: Vec<String>,
    /// Instances only in A.
    pub removed: Vec<String>,
    /// Instances in both whose attributes differ.
    pub changed: Vec<InstanceChange>,
    /// Outputs that were added, removed or changed (`path` is the output name).
    pub outputs: Vec<AttrChange>,
    /// Serials (A, B).
    pub serials: (u64, u64),
    /// The lineages differ: the states are not versions of one another.
    pub lineage_differs: bool,
}

impl StateDiff {
    /// Nothing differs.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.changed.is_empty()
            && self.outputs.is_empty()
    }

    /// `+2 −1 ~3` style summary.
    pub fn summary(&self) -> String {
        format!(
            "{} added · {} removed · {} changed · {} output change{}",
            self.added.len(),
            self.removed.len(),
            self.changed.len(),
            self.outputs.len(),
            if self.outputs.len() == 1 { "" } else { "s" }
        )
    }
}

fn instance_map(s: &TfState) -> BTreeMap<String, InstanceIx> {
    s.instances().map(|(ix, a)| (a, ix)).collect()
}

/// Compare state `a` (older) with `b` (newer).
pub fn diff(a: &TfState, b: &TfState) -> StateDiff {
    let ma = instance_map(a);
    let mb = instance_map(b);
    let mut out = StateDiff {
        serials: (a.serial, b.serial),
        lineage_differs: !a.lineage.is_empty() && !b.lineage.is_empty() && a.lineage != b.lineage,
        ..StateDiff::default()
    };
    for addr in mb.keys() {
        if !ma.contains_key(addr) {
            out.added.push(addr.clone());
        }
    }
    for (addr, ixa) in &ma {
        let Some(ixb) = mb.get(addr) else {
            out.removed.push(addr.clone());
            continue;
        };
        let (Some((_, ia)), Some((_, ib))) = (a.instance(*ixa), b.instance(*ixb)) else {
            continue;
        };
        let attrs = instance_changes(ia, ib);
        if !attrs.is_empty() {
            out.changed.push(InstanceChange {
                address: addr.clone(),
                attrs,
            });
        }
    }
    let oa: BTreeMap<&str, &TfOutput> = a.outputs.iter().map(|o| (o.name.as_str(), o)).collect();
    let ob: BTreeMap<&str, &TfOutput> = b.outputs.iter().map(|o| (o.name.as_str(), o)).collect();
    let names: BTreeSet<&str> = oa.keys().chain(ob.keys()).copied().collect();
    for name in names {
        let (x, y) = (oa.get(name), ob.get(name));
        let old = x.map(|o| scalar(&o.value));
        let new = y.map(|o| scalar(&o.value));
        if old != new {
            out.outputs.push(AttrChange {
                path: name.to_owned(),
                old,
                new,
                sensitive: x.is_some_and(|o| o.sensitive) || y.is_some_and(|o| o.sensitive),
            });
        }
    }
    out
}

fn instance_changes(a: &TfInstance, b: &TfInstance) -> Vec<AttrChange> {
    let ra: BTreeMap<String, AttrRow> = a.rows().into_iter().map(|r| (r.path.clone(), r)).collect();
    let rb: BTreeMap<String, AttrRow> = b.rows().into_iter().map(|r| (r.path.clone(), r)).collect();
    let paths: BTreeSet<&String> = ra.keys().chain(rb.keys()).collect();
    let mut out = Vec::new();
    for p in paths {
        let (x, y) = (ra.get(p), rb.get(p));
        let old = x.map(|r| r.value.clone());
        let new = y.map(|r| r.value.clone());
        if old != new {
            out.push(AttrChange {
                path: p.clone(),
                old,
                new,
                sensitive: x.is_some_and(|r| r.sensitive) || y.is_some_and(|r| r.sensitive),
            });
        }
    }
    let deps = |i: &TfInstance| {
        let mut d = i.dependencies.clone();
        d.sort();
        d.join(", ")
    };
    if deps(a) != deps(b) {
        out.push(AttrChange {
            path: "(dependencies)".into(),
            old: Some(deps(a)),
            new: Some(deps(b)),
            sensitive: false,
        });
    }
    if a.status != b.status {
        out.push(AttrChange {
            path: "(status)".into(),
            old: a.status.clone(),
            new: b.status.clone(),
            sensitive: false,
        });
    }
    out
}

/// A display value of an output (JSON for lists and objects).
pub fn output_text(o: &TfOutput) -> String {
    match &o.value {
        Json::String(s) => s.clone(),
        Json::Array(_) | Json::Object(_) => {
            serde_json::to_string_pretty(&o.value).unwrap_or_else(|_| o.value.to_string())
        }
        other => scalar(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1: &[u8] = include_bytes!("../testdata/tfstate/serial-7.tfstate");
    const V2: &[u8] = include_bytes!("../testdata/tfstate/serial-9.tfstate");

    #[test]
    fn parses_header_outputs_and_resources() {
        let s = parse(V1).unwrap();
        assert_eq!(s.version, 4);
        assert_eq!(s.serial, 7);
        assert_eq!(s.terraform_version, "1.9.8");
        assert_eq!(s.lineage, "3f0c1d2e-8a8b-4c4d-9e9f-0a1b2c3d4e5f");
        assert_eq!(s.counts(), (5, 1));
        let names: Vec<&str> = s.outputs.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["db_password", "vpc_id", "web_ips"]);
        assert!(s.outputs[0].sensitive);
        assert_eq!(output_text(&s.outputs[1]), "vpc-0abc");
        let addrs: Vec<String> = s.instances().map(|(_, a)| a).collect();
        assert_eq!(
            addrs,
            [
                "data.aws_ami.ubuntu",
                "aws_instance.web[0]",
                "aws_instance.web[1]",
                "module.db.aws_db_instance.main",
                "module.vpc.aws_subnet.private[\"a\"]",
                "module.vpc.aws_vpc.this",
            ]
        );
        let (r, _) = s.instance((0, 0)).unwrap();
        assert_eq!(r.provider_short(), "hashicorp/aws");
    }

    #[test]
    fn sensitive_values_are_marked() {
        let s = parse(V1).unwrap();
        let (_, db) = s.instance((2, 0)).unwrap();
        let rows = db.rows();
        let get = |p: &str| rows.iter().find(|r| r.path == p).unwrap();
        // Marked by the provider.
        assert!(get("password").sensitive);
        // Below a marked path.
        assert!(get("master_user_secret[0].secret_arn").sensitive);
        // Not marked.
        assert!(!get("engine").sensitive);
        assert_eq!(get("port").value, "5432");
        assert_eq!(get("tags.Name").value, "main-db");
        assert_eq!(get("replicas").value, "[]");
        // A secret-looking name is masked even when not marked.
        assert!(looks_secret("auth_token"));
        assert!(looks_secret("connection_string"));
        assert!(!looks_secret("kms_key_id"));
        assert!(!looks_secret("secret_arn"));
        assert!(looks_secret("private_key_pem"));
        assert!(!looks_secret("token_expiry_days"));
    }

    #[test]
    fn newer_sensitive_layout_is_read() {
        let p = sensitive_paths(Some(&serde_json::json!([
            {"path": [{"type": "get_attr", "value": "tags"}, {"type": "index", "value": {"value": "k", "type": "string"}}]},
            [{"type": "get_attr", "value": "list"}, {"type": "index", "value": {"value": 2, "type": "number"}}]
        ])));
        assert_eq!(p, ["tags.k", "list[2]"]);
    }

    #[test]
    fn search_skips_sensitive_values() {
        let s = parse(V1).unwrap();
        assert_eq!(s.search("").len(), 6);
        assert_eq!(s.search("web").len(), 2);
        // An attribute value.
        assert_eq!(s.search("10.0.1.0/24"), [(3, 0)]);
        // The password's value is never searched.
        assert!(s.search("hunter2").is_empty());
    }

    #[test]
    fn tree_groups_by_module_and_type() {
        let s = parse(V1).unwrap();
        let rows = s.tree(None, &HashSet::new());
        let labels: Vec<String> = rows
            .iter()
            .map(|r| match r {
                TreeRow::Group { label, depth, .. } => format!("{depth}:{label}"),
                TreeRow::Instance { label, .. } => format!("  {label}"),
            })
            .collect();
        assert_eq!(
            labels,
            [
                "0:root module",
                "1:aws_instance",
                "  web[0]",
                "  web[1]",
                "1:data.aws_ami",
                "  ubuntu",
                "0:module.db",
                "1:aws_db_instance",
                "  main",
                "0:module.vpc",
                "1:aws_subnet",
                "  private[\"a\"]",
                "1:aws_vpc",
                "  this",
            ]
        );
        let collapsed: HashSet<String> = ["m:".to_owned(), "m:module.vpc/aws_subnet".to_owned()]
            .into_iter()
            .collect();
        let rows = s.tree(None, &collapsed);
        assert_eq!(rows.len(), 8);
        let only = s.search("web[1]");
        assert_eq!(s.tree(Some(&only), &HashSet::new()).len(), 3);
    }

    #[test]
    fn dependencies_and_dependents() {
        let s = parse(V1).unwrap();
        let (_, web) = s.instance((1, 0)).unwrap();
        assert_eq!(
            web.dependencies,
            ["data.aws_ami.ubuntu", "module.vpc.aws_subnet.private"]
        );
        assert_eq!(
            s.dependents("module.vpc.aws_subnet.private"),
            ["aws_instance.web", "module.db.aws_db_instance.main"]
        );
    }

    #[test]
    fn compares_two_serials() {
        let a = parse(V1).unwrap();
        let b = parse(V2).unwrap();
        let d = diff(&a, &b);
        assert_eq!(d.serials, (7, 9));
        assert!(!d.lineage_differs);
        assert_eq!(d.added, ["aws_s3_bucket.logs"]);
        assert_eq!(d.removed, ["aws_instance.web[1]"]);
        let changed: Vec<&str> = d.changed.iter().map(|c| c.address.as_str()).collect();
        assert_eq!(
            changed,
            ["aws_instance.web[0]", "module.db.aws_db_instance.main"]
        );
        let web = &d.changed[0];
        let paths: Vec<&str> = web.attrs.iter().map(|a| a.path.as_str()).collect();
        assert_eq!(paths, ["instance_type", "tags.Env", "(status)"]);
        assert_eq!(web.attrs[0].old.as_deref(), Some("t3.micro"));
        assert_eq!(web.attrs[0].new.as_deref(), Some("t3.small"));
        assert_eq!(web.attrs[1].old, None);
        let db = &d.changed[1];
        assert_eq!(db.attrs.len(), 1);
        assert_eq!(db.attrs[0].path, "password");
        assert!(db.attrs[0].sensitive);
        let outs: Vec<&str> = d.outputs.iter().map(|o| o.path.as_str()).collect();
        assert_eq!(outs, ["db_password", "web_ips"]);
        assert!(d.outputs[0].sensitive);
        assert_eq!(
            d.summary(),
            "1 added · 1 removed · 2 changed · 2 output changes"
        );
        assert!(diff(&a, &a).is_empty());
    }

    #[test]
    fn rejects_other_files() {
        assert!(parse(b"{}").is_err());
        assert!(parse(b"not json").is_err());
        let e = parse(br#"{"version": 3}"#).unwrap_err().to_string();
        assert!(e.contains("version 3"), "{e}");
    }
}
