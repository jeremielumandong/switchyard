//! Against a real Docker daemon (`cargo test -p switchyard-containers --test docker --
//! --ignored`). Starts and removes its own `alpine` container.

use std::time::Duration;

use switchyard_containers::docker::ContainerState;
use switchyard_containers::{
    Action, DockerKind, Inspect, Listing, LogOptions, LogSource, Query, Target, Tool, local,
};

const IMAGE: &str = "mirror.gcr.io/library/alpine:3.20";
const NAME: &str = "swy-containers-test";

async fn docker(args: &[&str]) -> Result<String, String> {
    let program = local::find_program(Tool::Docker, None).ok_or("docker not installed")?;
    let inv = Target::default().invocation(Tool::Docker, args.iter().copied());
    local::run(&program, &inv, Duration::from_secs(120))
        .await
        .map_err(|e| e.to_string())
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn lists_acts_on_and_tails_a_container() {
    let program =
        local::find_program(Tool::Docker, None).unwrap_or_else(|| panic!("docker not installed"));
    let target = Target::default();
    let _ = docker(&["rm", "-f", NAME]).await;
    docker(&[
        "run",
        "-d",
        "--name",
        NAME,
        "-l",
        "swy=test",
        IMAGE,
        "sh",
        "-c",
        "i=0; while true; do echo line $i; i=$((i+1)); sleep 0.2; done",
    ])
    .await
    .unwrap_or_else(|e| panic!("{e}"));

    let q = Query::Containers { all: true };
    let out = local::run(&program, &q.invocation(&target), Duration::from_secs(30))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let Ok(Listing::Containers(rows)) = q.parse(&out) else {
        panic!("not a container listing")
    };
    let row = rows
        .iter()
        .find(|c| c.name == NAME)
        .unwrap_or_else(|| panic!("{NAME} not listed"));
    assert_eq!(row.state, ContainerState::Running);
    assert_eq!(row.labels, [("swy".to_owned(), "test".to_owned())]);

    // Logs: the first lines, then follow until a later line shows up.
    let source = LogSource::Docker { id: row.id.clone() };
    let mut lines: Vec<String> = Vec::new();
    let followed = tokio::time::timeout(
        Duration::from_secs(3),
        local::stream(
            &program,
            &source.invocation(LogOptions::default(), &target),
            |l| lines.extend(l),
        ),
    )
    .await;
    assert!(followed.is_err(), "a followed log runs until cancelled");
    assert!(lines.iter().any(|l| l == "line 0"));
    assert!(lines.len() >= 5, "{lines:?}");

    let inspect = Inspect::Docker {
        kind: DockerKind::Container,
        id: row.id.clone(),
    };
    let json = local::run(
        &program,
        &inspect.invocation(&target),
        Duration::from_secs(30),
    )
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    assert!(json.contains(NAME));

    for action in [
        Action::Stop {
            id: row.id.clone(),
            name: NAME.into(),
        },
        Action::Start {
            id: row.id.clone(),
            name: NAME.into(),
        },
        Action::Remove {
            id: row.id.clone(),
            name: NAME.into(),
        },
    ] {
        local::run(
            &program,
            &action.invocation(&target),
            Duration::from_secs(60),
        )
        .await
        .unwrap_or_else(|e| panic!("{}: {e}", action.label()));
    }
    let out = local::run(&program, &q.invocation(&target), Duration::from_secs(30))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let Ok(Listing::Containers(rows)) = q.parse(&out) else {
        panic!("not a container listing")
    };
    assert!(rows.iter().all(|c| c.name != NAME));

    // A missing container is the CLI's own message.
    let err = local::run(
        &program,
        &Action::Stop {
            id: NAME.into(),
            name: NAME.into(),
        }
        .invocation(&target),
        Duration::from_secs(30),
    )
    .await;
    let Err(e) = err else {
        panic!("stopping a removed container worked")
    };
    assert!(e.to_string().contains("No such container"), "{e}");

    for q in [Query::Images, Query::Volumes, Query::Networks] {
        let out = local::run(&program, &q.invocation(&target), Duration::from_secs(30))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        q.parse(&out).unwrap_or_else(|e| panic!("{e}"));
    }
}
