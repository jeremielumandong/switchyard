//! Docker listings: `docker ps / images / volume ls / network ls --format '{{json .}}'`,
//! one JSON object per line.

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::{ContainersError, Result, Tool};

const JSON_FORMAT: &str = "{{json .}}";

/// `docker ps` arguments.
pub fn ps_args(all: bool) -> Vec<String> {
    let mut v = vec!["ps".to_owned()];
    if all {
        v.push("-a".into());
    }
    v.extend(["--format".to_owned(), JSON_FORMAT.to_owned()]);
    v
}

/// `docker images` arguments.
pub fn images_args() -> Vec<String> {
    vec!["images".into(), "--format".into(), JSON_FORMAT.into()]
}

/// `docker volume ls` arguments.
pub fn volumes_args() -> Vec<String> {
    vec![
        "volume".into(),
        "ls".into(),
        "--format".into(),
        JSON_FORMAT.into(),
    ]
}

/// `docker network ls` arguments.
pub fn networks_args() -> Vec<String> {
    vec![
        "network".into(),
        "ls".into(),
        "--format".into(),
        JSON_FORMAT.into(),
    ]
}

/// A container's state (`State` in `docker ps`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerState {
    /// Running.
    Running,
    /// Paused.
    Paused,
    /// Restarting.
    Restarting,
    /// Created, never started.
    Created,
    /// Exited.
    Exited,
    /// Being removed.
    Removing,
    /// Dead.
    Dead,
    /// Something newer.
    Other,
}

impl ContainerState {
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "running" => Self::Running,
            "paused" => Self::Paused,
            "restarting" => Self::Restarting,
            "created" => Self::Created,
            "exited" => Self::Exited,
            "removing" => Self::Removing,
            "dead" => Self::Dead,
            _ => Self::Other,
        }
    }

    /// Whether its processes run (stop / restart / shell / follow logs apply).
    pub fn is_running(self) -> bool {
        matches!(self, Self::Running | Self::Restarting | Self::Paused)
    }
}

/// One row of `docker ps`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Container {
    /// Short id.
    pub id: String,
    /// First name (without the leading `/`).
    pub name: String,
    /// Image reference.
    pub image: String,
    /// Command, as docker shows it.
    pub command: String,
    /// State.
    pub state: ContainerState,
    /// Status text (`Up 3 hours (healthy)`, `Exited (0) 2 days ago`).
    pub status: String,
    /// Published ports (`0.0.0.0:8080->80/tcp, …`).
    pub ports: String,
    /// When it was created, as docker prints it.
    pub created: String,
    /// Networks.
    pub networks: String,
    /// Labels.
    pub labels: Vec<(String, String)>,
    /// The Compose project, from `com.docker.compose.project`.
    pub compose_project: Option<String>,
}

/// One row of `docker images`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    /// Short id.
    pub id: String,
    /// Repository (`<none>` for dangling images).
    pub repository: String,
    /// Tag.
    pub tag: String,
    /// Size, as docker prints it.
    pub size: String,
    /// Age (`5 months ago`).
    pub created_since: String,
    /// Containers using it, when docker reports it.
    pub containers: String,
}

impl Image {
    /// `repository:tag`, or the id for a dangling image.
    pub fn reference(&self) -> String {
        if self.repository.is_empty() || self.repository == "<none>" {
            self.id.clone()
        } else if self.tag.is_empty() || self.tag == "<none>" {
            self.repository.clone()
        } else {
            format!("{}:{}", self.repository, self.tag)
        }
    }
}

/// One row of `docker volume ls`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    /// Name.
    pub name: String,
    /// Driver.
    pub driver: String,
    /// Scope.
    pub scope: String,
    /// Mount point on the Docker host.
    pub mountpoint: String,
}

/// One row of `docker network ls`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    /// Short id.
    pub id: String,
    /// Name.
    pub name: String,
    /// Driver.
    pub driver: String,
    /// Scope.
    pub scope: String,
    /// Internal (no outside access).
    pub internal: bool,
    /// IPv6 enabled.
    pub ipv6: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PsLine {
    #[serde(rename = "ID", default)]
    id: String,
    #[serde(default)]
    names: String,
    #[serde(default)]
    image: String,
    #[serde(default)]
    command: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    ports: String,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    networks: String,
    #[serde(default)]
    labels: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImageLine {
    #[serde(rename = "ID", default)]
    id: String,
    #[serde(default)]
    repository: String,
    #[serde(default)]
    tag: String,
    #[serde(default)]
    size: String,
    #[serde(default)]
    created_since: String,
    #[serde(default, deserialize_with = "string_or_number")]
    containers: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct VolumeLine {
    #[serde(default)]
    name: String,
    #[serde(default)]
    driver: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    mountpoint: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct NetworkLine {
    #[serde(rename = "ID", default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    driver: String,
    #[serde(default)]
    scope: String,
    #[serde(default, deserialize_with = "string_or_number")]
    internal: String,
    #[serde(rename = "IPv6", default, deserialize_with = "string_or_number")]
    ipv6: String,
}

/// Docker versions differ on whether some fields are strings or numbers / booleans.
fn string_or_number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::String(s) => s,
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    })
}

/// Parse one JSON object per non-empty line.
fn json_lines<T: DeserializeOwned>(stdout: &str) -> Result<Vec<T>> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            serde_json::from_str(l).map_err(|e| ContainersError::Parse {
                tool: Tool::Docker,
                message: e.to_string(),
            })
        })
        .collect()
}

/// `a=1,b=2` into pairs (a value may hold `=`; labels with commas inside a value are
/// split as docker prints them).
fn parse_labels(s: &str) -> Vec<(String, String)> {
    s.split(',')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| match l.split_once('=') {
            Some((k, v)) => (k.to_owned(), v.to_owned()),
            None => (l.to_owned(), String::new()),
        })
        .collect()
}

/// Rows of `docker ps --format '{{json .}}'`.
pub fn parse_containers(stdout: &str) -> Result<Vec<Container>> {
    let mut out: Vec<Container> = json_lines::<PsLine>(stdout)?
        .into_iter()
        .map(|l| {
            let labels = parse_labels(&l.labels);
            let compose_project = labels
                .iter()
                .find(|(k, _)| k == "com.docker.compose.project")
                .map(|(_, v)| v.clone());
            Container {
                id: l.id,
                name: l
                    .names
                    .split(',')
                    .next()
                    .unwrap_or_default()
                    .trim_start_matches('/')
                    .to_owned(),
                image: l.image,
                command: l.command.trim_matches('"').to_owned(),
                state: ContainerState::parse(&l.state),
                status: l.status,
                ports: l.ports,
                created: l.created_at,
                networks: l.networks,
                labels,
                compose_project,
            }
        })
        .collect();
    // Running first, then by name.
    out.sort_by(|a, b| {
        b.state
            .is_running()
            .cmp(&a.state.is_running())
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

/// Rows of `docker images --format '{{json .}}'`.
pub fn parse_images(stdout: &str) -> Result<Vec<Image>> {
    Ok(json_lines::<ImageLine>(stdout)?
        .into_iter()
        .map(|l| Image {
            id: l.id,
            repository: l.repository,
            tag: l.tag,
            size: l.size,
            created_since: l.created_since,
            containers: if l.containers == "N/A" {
                String::new()
            } else {
                l.containers
            },
        })
        .collect())
}

/// Rows of `docker volume ls --format '{{json .}}'`.
pub fn parse_volumes(stdout: &str) -> Result<Vec<Volume>> {
    let mut out: Vec<Volume> = json_lines::<VolumeLine>(stdout)?
        .into_iter()
        .map(|l| Volume {
            name: l.name,
            driver: l.driver,
            scope: l.scope,
            mountpoint: l.mountpoint,
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Rows of `docker network ls --format '{{json .}}'`.
pub fn parse_networks(stdout: &str) -> Result<Vec<Network>> {
    let mut out: Vec<Network> = json_lines::<NetworkLine>(stdout)?
        .into_iter()
        .map(|l| Network {
            id: l.id,
            name: l.name,
            driver: l.driver,
            scope: l.scope,
            internal: l.internal == "true",
            ipv6: l.ipv6 == "true",
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containers_from_docker_29() {
        let rows = parse_containers(include_str!("../testdata/docker_ps.jsonl"))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(rows.len(), 3);
        // Running first, by name; the created one last.
        let names: Vec<&str> = rows.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["swy-demo", "swy-ports", "swy-created"]);
        let web = &rows[1];
        assert_eq!(web.state, ContainerState::Running);
        assert_eq!(web.ports, "0.0.0.0:18080->80/tcp");
        assert_eq!(web.image, "mirror.gcr.io/library/alpine:3.20");
        assert_eq!(web.labels, [("app".to_owned(), "web".to_owned())]);
        assert_eq!(web.command, "sleep 3600");
        assert_eq!(web.id.len(), 12);
        assert_eq!(rows[2].state, ContainerState::Created);
        assert_eq!(rows[2].status, "Created");
    }

    #[test]
    fn compose_project_and_older_field_types() {
        let line = r#"{"ID":"1","Names":"/shop-db-1,alias","State":"exited","Status":"Exited (0) 2 days ago","Labels":"com.docker.compose.project=shop,com.docker.compose.service=db"}"#;
        let rows = parse_containers(line).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(rows[0].name, "shop-db-1");
        assert_eq!(rows[0].compose_project.as_deref(), Some("shop"));
        assert!(!rows[0].state.is_running());
        // Some versions print numbers / booleans unquoted.
        let net = r#"{"ID":"a","Name":"n","Internal":true,"IPv6":false}"#;
        let nets = parse_networks(net).unwrap_or_else(|e| panic!("{e}"));
        assert!(nets[0].internal && !nets[0].ipv6);
        let img = r#"{"ID":"x","Repository":"<none>","Tag":"<none>","Containers":2}"#;
        let imgs = parse_images(img).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(imgs[0].reference(), "x");
        assert_eq!(imgs[0].containers, "2");
    }

    #[test]
    fn images_volumes_networks() {
        let images = parse_images(include_str!("../testdata/docker_images.jsonl"))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(images[0].reference(), "mirror.gcr.io/library/alpine:3.20");
        assert_eq!(images[0].size, "12.2MB");
        let volumes = parse_volumes(include_str!("../testdata/docker_volumes.jsonl"))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(volumes[0].name, "swy-vol");
        assert_eq!(volumes[0].driver, "local");
        let nets = parse_networks(include_str!("../testdata/docker_networks.jsonl"))
            .unwrap_or_else(|e| panic!("{e}"));
        let names: Vec<&str> = nets.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["bridge", "host", "none"]);
    }

    #[test]
    fn bad_output_is_a_parse_error() {
        assert!(matches!(
            parse_containers("not json"),
            Err(ContainersError::Parse { .. })
        ));
        assert_eq!(parse_containers("\n\n").map(|v| v.len()), Ok(0));
    }
}
