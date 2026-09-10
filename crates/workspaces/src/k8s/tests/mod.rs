//! The tests for every builder in `k8s`: one file on purpose, since most of them share the
//! `ctx`/`svc`/`ws_spec` fixtures and assert on the same rendered objects from different angles.

use super::*;
use crate::crd::DesiredState;
use crate::model::Mount;

mod attach;
mod pod;
mod environment;
mod policies;


pub(super) const AGENT_RESOLV: &str = "search kube-system.svc.cluster.local svc.cluster.local cluster.local node.example.net\nnameserver 10.43.0.10\noptions ndots:5\n";


pub(super) fn owner_ref() -> OwnerReference {
    OwnerReference {
        api_version: "kloudlite.io/v1alpha1".into(),
        kind: "Volume".into(),
        name: "vol-1".into(),
        uid: "uid-1".into(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    }
}


pub(super) fn ctx() -> PodContext<'static> {
    PodContext { pool: "/mnt/wspool", node_name: "session-0", owner_ref: owner_ref(), runtime_class: Some("gvisor"), default_image: "ghcr.io/kloudlite/kloudlite-workspace:deadbeef", system: None, registry_host: "registry.kloudlite.io" }
}


pub(super) fn svc(folder: &str, path: &str) -> model::Service {
    model::Service {
        name: "web".into(),
        image: "nginx".into(),
        command: vec![],
        env: Default::default(),
        mounts: vec![Mount { folder: folder.into(), path: path.into() }],
        ports: vec![80],
        resources: None,
    }
}


pub(super) fn ws_spec() -> WorkspaceSpec {
    WorkspaceSpec {
        team: String::new(),
        owner: "alice".into(),
        name: "dev".into(),
        region: "centralindia".into(),
        image: "nginx:alpine".into(),
        storage: Some(crate::crd::WorkspaceStorage { quota_gb: 10, source: None }),
        desired_state: DesiredState::Running,
        resources: PodResources::default(),
        packages: vec![],
        locks: vec![],
        attached_environment: None,
    }
}


pub(super) fn intercept(ports: &[(u16, u16)]) -> crate::crd::Intercept {
    crate::crd::Intercept {
        service: "web".into(),
        workspace: "ws-1".into(),
        ports: ports.iter().map(|(s, w)| crate::crd::PortMap { service: *s, workspace: *w }).collect(),
    }
}


pub(super) fn two_port_svc() -> model::Service {
    let mut s = svc("data", "/data");
    s.ports = vec![80, 5432];
    s
}
