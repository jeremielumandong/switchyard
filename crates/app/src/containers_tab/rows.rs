//! The containers tab's list: which views a platform has, their columns, and the rows
//! of a listing (filtered by the filter box), as plain data so it is unit-tested.

use switchyard_core::containers::docker::ContainerState;
use switchyard_core::containers::kube::Health;
use switchyard_core::containers::{Listing, Query, format_age};
use switchyard_core::store::ContainerPlatform;

/// A list view of the tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum View {
    /// `docker ps`.
    Containers,
    /// `docker images`.
    Images,
    /// `docker volume ls`.
    Volumes,
    /// `docker network ls`.
    Networks,
    /// Pods.
    Pods,
    /// Deployments.
    Deployments,
    /// Services.
    Services,
    /// This connection's port forwards.
    Forwards,
}

impl View {
    /// The views of a platform, in tab order.
    pub fn of(platform: ContainerPlatform) -> &'static [View] {
        match platform {
            ContainerPlatform::Docker => &[
                View::Containers,
                View::Images,
                View::Volumes,
                View::Networks,
            ],
            ContainerPlatform::Kubernetes => &[
                View::Pods,
                View::Deployments,
                View::Services,
                View::Forwards,
            ],
        }
    }

    /// Label.
    pub fn label(self) -> &'static str {
        match self {
            View::Containers => "Containers",
            View::Images => "Images",
            View::Volumes => "Volumes",
            View::Networks => "Networks",
            View::Pods => "Pods",
            View::Deployments => "Deployments",
            View::Services => "Services",
            View::Forwards => "Port forwards",
        }
    }

    /// The listing behind it (`None` for port forwards, which come from core's events).
    pub fn query(self, all: bool, namespace: Option<&str>) -> Option<Query> {
        let namespace = namespace.map(str::to_owned);
        Some(match self {
            View::Containers => Query::Containers { all },
            View::Images => Query::Images,
            View::Volumes => Query::Volumes,
            View::Networks => Query::Networks,
            View::Pods => Query::Pods { namespace },
            View::Deployments => Query::Deployments { namespace },
            View::Services => Query::Services { namespace },
            View::Forwards => return None,
        })
    }

    /// Column headers and widths (0: takes the rest). Kubernetes views get a NAMESPACE
    /// column when listing all namespaces.
    pub fn columns(self, all_namespaces: bool) -> Vec<(&'static str, f32)> {
        let mut v: Vec<(&'static str, f32)> = match self {
            View::Containers => vec![
                ("NAME", 200.),
                ("IMAGE", 220.),
                ("STATUS", 170.),
                ("PORTS", 0.),
                ("ID", 110.),
            ],
            View::Images => vec![
                ("REPOSITORY", 0.),
                ("TAG", 140.),
                ("ID", 120.),
                ("CREATED", 130.),
                ("SIZE", 90.),
            ],
            View::Volumes => vec![("NAME", 0.), ("DRIVER", 100.), ("SCOPE", 80.)],
            View::Networks => vec![("NAME", 0.), ("DRIVER", 100.), ("SCOPE", 80.), ("ID", 120.)],
            View::Pods => vec![
                ("NAME", 0.),
                ("READY", 60.),
                ("STATUS", 150.),
                ("RESTARTS", 76.),
                ("NODE", 170.),
                ("AGE", 64.),
            ],
            View::Deployments => vec![
                ("NAME", 0.),
                ("READY", 70.),
                ("UP-TO-DATE", 90.),
                ("AVAILABLE", 84.),
                ("AGE", 64.),
            ],
            View::Services => vec![
                ("NAME", 200.),
                ("TYPE", 110.),
                ("CLUSTER-IP", 120.),
                ("EXTERNAL", 160.),
                ("PORTS", 0.),
                ("AGE", 64.),
            ],
            View::Forwards => vec![("TARGET", 0.), ("LOCAL", 160.), ("STATE", 200.)],
        };
        if all_namespaces && matches!(self, View::Pods | View::Deployments | View::Services) {
            v.insert(0, ("NAMESPACE", 130.));
        }
        v
    }
}

/// How a row reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// Running / healthy.
    Good,
    /// Starting, stopping.
    Pending,
    /// Failing.
    Bad,
    /// Stopped or neutral.
    Muted,
}

/// One list row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// Stable key (id, or `namespace/name`).
    pub key: String,
    /// Cells, matching [`View::columns`].
    pub cells: Vec<String>,
    /// Status color.
    pub tone: Tone,
}

fn tone_of(h: Health) -> Tone {
    match h {
        Health::Good => Tone::Good,
        Health::Pending => Tone::Pending,
        Health::Bad => Tone::Bad,
    }
}

fn age(created: Option<i64>, now: i64) -> String {
    created.map(|c| format_age(now - c)).unwrap_or_default()
}

/// Whether every word of `filter` occurs (case-insensitive) in one of the cells.
fn matches(filter: &str, cells: &[String]) -> bool {
    let words: Vec<String> = filter.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return true;
    }
    let hay = cells.join(" ").to_lowercase();
    words.iter().all(|w| hay.contains(w.as_str()))
}

/// The rows of `listing` for `view`, filtered. `now` is Unix seconds (for ages).
pub fn rows(
    listing: &Listing,
    view: View,
    all_namespaces: bool,
    filter: &str,
    now: i64,
) -> Vec<Row> {
    let ns = |namespace: &str, mut cells: Vec<String>| {
        if all_namespaces {
            cells.insert(0, namespace.to_owned());
        }
        cells
    };
    let out: Vec<Row> = match (listing, view) {
        (Listing::Containers(list), View::Containers) => list
            .iter()
            .map(|c| Row {
                key: c.id.clone(),
                cells: vec![
                    c.name.clone(),
                    c.image.clone(),
                    c.status.clone(),
                    c.ports.clone(),
                    c.id.clone(),
                ],
                tone: match c.state {
                    ContainerState::Running => Tone::Good,
                    ContainerState::Restarting | ContainerState::Removing => Tone::Pending,
                    ContainerState::Dead => Tone::Bad,
                    ContainerState::Exited if !c.status.contains("(0)") => Tone::Bad,
                    _ => Tone::Muted,
                },
            })
            .collect(),
        (Listing::Images(list), View::Images) => list
            .iter()
            .map(|i| Row {
                key: format!("{}|{}:{}", i.id, i.repository, i.tag),
                cells: vec![
                    i.repository.clone(),
                    i.tag.clone(),
                    i.id.clone(),
                    i.created_since.clone(),
                    i.size.clone(),
                ],
                tone: Tone::Muted,
            })
            .collect(),
        (Listing::Volumes(list), View::Volumes) => list
            .iter()
            .map(|v| Row {
                key: v.name.clone(),
                cells: vec![v.name.clone(), v.driver.clone(), v.scope.clone()],
                tone: Tone::Muted,
            })
            .collect(),
        (Listing::Networks(list), View::Networks) => list
            .iter()
            .map(|n| Row {
                key: n.id.clone(),
                cells: vec![
                    n.name.clone(),
                    n.driver.clone(),
                    n.scope.clone(),
                    n.id.clone(),
                ],
                tone: Tone::Muted,
            })
            .collect(),
        (Listing::Pods(list), View::Pods) => list
            .iter()
            .map(|p| Row {
                key: format!("{}/{}", p.namespace, p.name),
                cells: ns(
                    &p.namespace,
                    vec![
                        p.name.clone(),
                        format!("{}/{}", p.ready, p.total),
                        p.status.clone(),
                        p.restarts.to_string(),
                        p.node.clone(),
                        age(p.created, now),
                    ],
                ),
                tone: tone_of(p.health()),
            })
            .collect(),
        (Listing::Deployments(list), View::Deployments) => list
            .iter()
            .map(|d| Row {
                key: format!("{}/{}", d.namespace, d.name),
                cells: ns(
                    &d.namespace,
                    vec![
                        d.name.clone(),
                        format!("{}/{}", d.ready, d.replicas),
                        d.up_to_date.to_string(),
                        d.available.to_string(),
                        age(d.created, now),
                    ],
                ),
                tone: if d.replicas == 0 {
                    Tone::Muted
                } else if d.ready >= d.replicas {
                    Tone::Good
                } else if d.ready == 0 {
                    Tone::Bad
                } else {
                    Tone::Pending
                },
            })
            .collect(),
        (Listing::Services(list), View::Services) => list
            .iter()
            .map(|s| Row {
                key: format!("{}/{}", s.namespace, s.name),
                cells: ns(
                    &s.namespace,
                    vec![
                        s.name.clone(),
                        s.kind.clone(),
                        s.cluster_ip.clone(),
                        if s.external.is_empty() {
                            "<none>".into()
                        } else {
                            s.external.join(",")
                        },
                        s.ports_text(),
                        age(s.created, now),
                    ],
                ),
                tone: Tone::Muted,
            })
            .collect(),
        _ => Vec::new(),
    };
    out.into_iter()
        .filter(|r| matches(filter, &r.cells))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_core::containers::kube::{Deployment, Pod};

    fn pod(ns: &str, name: &str, status: &str) -> Pod {
        Pod {
            namespace: ns.into(),
            name: name.into(),
            status: status.into(),
            phase: "Running".into(),
            ready: 1,
            total: 2,
            restarts: 3,
            node: "n1".into(),
            ip: String::new(),
            created: Some(1_000),
            containers: vec!["a".into()],
            ports: Vec::new(),
        }
    }

    #[test]
    fn pods_with_and_without_namespace_column() {
        let l = Listing::Pods(vec![
            pod("shop", "web-1", "Running"),
            pod("ops", "job-1", "CrashLoopBackOff"),
        ]);
        let r = rows(&l, View::Pods, false, "", 1_000 + 3 * 3600);
        assert_eq!(r[0].cells, ["web-1", "1/2", "Running", "3", "n1", "3h"]);
        assert_eq!(r[0].tone, Tone::Good);
        assert_eq!(r[1].tone, Tone::Bad);
        assert_eq!(r[0].key, "shop/web-1");
        assert_eq!(View::Pods.columns(false).len(), r[0].cells.len());
        let all = rows(&l, View::Pods, true, "", 1_000);
        assert_eq!(all[0].cells[0], "shop");
        assert_eq!(View::Pods.columns(true).len(), all[0].cells.len());
    }

    #[test]
    fn filter_words_match_any_cell() {
        let l = Listing::Pods(vec![
            pod("shop", "web-1", "Running"),
            pod("ops", "job-1", "CrashLoopBackOff"),
        ]);
        assert_eq!(rows(&l, View::Pods, true, "crash", 0).len(), 1);
        assert_eq!(rows(&l, View::Pods, true, "OPS job", 0).len(), 1);
        assert_eq!(rows(&l, View::Pods, true, "shop crash", 0).len(), 0);
        // A listing for another view shows nothing (a stale answer).
        assert!(rows(&l, View::Deployments, false, "", 0).is_empty());
    }

    #[test]
    fn deployment_tones() {
        let d = |ready: u32, replicas: u32| Deployment {
            namespace: "n".into(),
            name: "d".into(),
            replicas,
            ready,
            up_to_date: ready,
            available: ready,
            images: Vec::new(),
            created: None,
        };
        let l = Listing::Deployments(vec![d(3, 3), d(1, 3), d(0, 2), d(0, 0)]);
        let tones: Vec<Tone> = rows(&l, View::Deployments, false, "", 0)
            .into_iter()
            .map(|r| r.tone)
            .collect();
        assert_eq!(tones, [Tone::Good, Tone::Pending, Tone::Bad, Tone::Muted]);
    }

    #[test]
    fn every_view_has_columns_and_queries() {
        for p in [ContainerPlatform::Docker, ContainerPlatform::Kubernetes] {
            for v in View::of(p) {
                assert!(!v.columns(false).is_empty());
                assert_eq!(v.query(true, None).is_none(), *v == View::Forwards);
            }
        }
        assert_eq!(
            View::Pods.query(false, Some("shop")),
            Some(Query::Pods {
                namespace: Some("shop".into())
            })
        );
    }
}
