use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt,
};

use petgraph::visit::{depth_first_search, Reversed};
use serde::Serialize;
use turbopath::{AbsoluteSystemPath, AnchoredSystemPath, AnchoredSystemPathBuf};
use turborepo_graph_utils as graph;
use turborepo_lockfiles::Lockfile;

use crate::{
    discovery::LocalPackageDiscoveryBuilder, package_json::PackageJson,
    package_manager::PackageManager,
};

pub mod builder;

pub use builder::{Error, PackageGraphBuilder};

pub const ROOT_PKG_NAME: &str = "//";

#[derive(Debug)]
pub struct PackageGraph {
    workspace_graph: petgraph::Graph<WorkspaceNode, ()>,
    #[allow(dead_code)]
    node_lookup: HashMap<WorkspaceNode, petgraph::graph::NodeIndex>,
    workspaces: HashMap<WorkspaceName, WorkspaceInfo>,
    package_manager: PackageManager,
    lockfile: Option<Box<dyn Lockfile>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkspaceInfo {
    pub package_json: PackageJson,
    pub package_json_path: AnchoredSystemPathBuf,
    pub unresolved_external_dependencies: Option<BTreeMap<PackageName, PackageVersion>>,
    pub transitive_dependencies: Option<HashSet<turborepo_lockfiles::Package>>,
}

impl WorkspaceInfo {
    pub fn package_json_path(&self) -> &AnchoredSystemPath {
        &self.package_json_path
    }

    /// Get the path to this workspace.
    ///
    /// note: This is infallible because `package_json_path` is guaranteed to
    /// have       at least one segment
    pub fn package_path(&self) -> &AnchoredSystemPath {
        self.package_json_path
            .parent()
            .expect("at least one segment")
    }
}

type PackageName = String;
type PackageVersion = String;

/// Name of workspaces with a special marker for the workspace root
#[derive(Debug, Clone, Hash, Eq, PartialEq, Ord, PartialOrd)]
pub enum WorkspaceName {
    Root,
    Other(String),
}

impl Serialize for WorkspaceName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            WorkspaceName::Root => serializer.serialize_str(ROOT_PKG_NAME),
            WorkspaceName::Other(other) => serializer.serialize_str(other),
        }
    }
}

#[derive(Debug, Clone, Hash, Eq, PartialEq, Ord, PartialOrd)]
pub enum WorkspaceNode {
    Root,
    Workspace(WorkspaceName),
}

impl WorkspaceNode {
    pub fn as_workspace(&self) -> &WorkspaceName {
        match self {
            WorkspaceNode::Workspace(name) => name,
            WorkspaceNode::Root => &WorkspaceName::Root,
        }
    }
}

impl PackageGraph {
    pub fn builder(
        repo_root: &AbsoluteSystemPath,
        root_package_json: PackageJson,
    ) -> PackageGraphBuilder<LocalPackageDiscoveryBuilder> {
        PackageGraphBuilder::new(repo_root, root_package_json)
    }

    #[tracing::instrument(skip(self))]
    pub fn validate(&self) -> Result<(), Error> {
        graph::validate_graph(&self.workspace_graph).map_err(Error::InvalidPackageGraph)
    }

    pub fn remove_workspace_dependencies(&mut self) {
        let root_index = self
            .node_lookup
            .get(&WorkspaceNode::Root)
            .expect("graph should have root workspace node");
        self.workspace_graph.retain_edges(|graph, index| {
            let Some((_src, dst)) = graph.edge_endpoints(index) else {
                return false;
            };
            dst == *root_index
        });
    }

    /// Returns the number of workspaces in the repo
    /// *including* the root workspace.
    pub fn len(&self) -> usize {
        self.workspaces.len()
    }

    pub fn is_empty(&self) -> bool {
        self.workspaces.is_empty()
    }

    pub fn package_manager(&self) -> &PackageManager {
        &self.package_manager
    }

    pub fn lockfile(&self) -> Option<&dyn Lockfile> {
        self.lockfile.as_deref()
    }

    pub fn package_json(&self, workspace: &WorkspaceName) -> Option<&PackageJson> {
        let entry = self.workspaces.get(workspace)?;
        Some(&entry.package_json)
    }

    pub fn workspace_dir(&self, workspace: &WorkspaceName) -> Option<&AnchoredSystemPath> {
        let entry = self.workspaces.get(workspace)?;
        Some(
            entry
                .package_json_path()
                .parent()
                .unwrap_or_else(|| AnchoredSystemPath::new("").unwrap()),
        )
    }

    pub fn workspace_info(&self, workspace: &WorkspaceName) -> Option<&WorkspaceInfo> {
        self.workspaces.get(workspace)
    }

    pub fn workspaces(&self) -> impl Iterator<Item = (&WorkspaceName, &WorkspaceInfo)> {
        self.workspaces.iter()
    }

    pub fn root_package_json(&self) -> &PackageJson {
        self.package_json(&WorkspaceName::Root)
            .expect("package graph was built without root package.json")
    }

    /// Gets all the nodes that directly depend on this one, that is to say
    /// have a edge to `workspace`.
    ///
    /// Example:
    ///
    /// a -> b -> c
    ///
    /// immediate_dependencies(a) -> {b}
    pub fn immediate_dependencies(
        &self,
        workspace: &WorkspaceNode,
    ) -> Option<HashSet<&WorkspaceNode>> {
        let index = self.node_lookup.get(workspace)?;
        Some(
            self.workspace_graph
                .neighbors_directed(*index, petgraph::Outgoing)
                .map(|index| {
                    self.workspace_graph
                        .node_weight(index)
                        .expect("node index from neighbors should be present")
                })
                .collect(),
        )
    }

    /// Gets all the nodes that directly depend on this one, that is to say
    /// have a edge to `workspace`.
    ///
    /// Example:
    ///
    /// a -> b -> c
    ///
    /// immediate_ancestors(c) -> {b}
    #[allow(dead_code)]
    pub fn immediate_ancestors(
        &self,
        workspace: &WorkspaceNode,
    ) -> Option<HashSet<&WorkspaceNode>> {
        let index = self.node_lookup.get(workspace)?;
        Some(
            self.workspace_graph
                .neighbors_directed(*index, petgraph::Incoming)
                .map(|index| {
                    self.workspace_graph
                        .node_weight(index)
                        .expect("node index from neighbors should be present")
                })
                .collect(),
        )
    }

    /// For a given workspace in the repo, returns the set of workspaces
    /// that this one depends on, excluding those that are unresolved.
    ///
    /// Example:
    ///
    /// a -> b -> c (external)
    ///
    /// dependencies(a) = {b, c}
    #[allow(dead_code)]
    pub fn dependencies<'a>(&'a self, node: &WorkspaceNode) -> HashSet<&'a WorkspaceNode> {
        let mut dependencies =
            self.transitive_closure_inner(Some(node), petgraph::Direction::Outgoing);
        dependencies.remove(node);
        dependencies
    }

    /// For a given workspace in the repo, returns the set of workspaces
    /// that depend on this one, excluding those that are unresolved.
    ///
    /// Example:
    ///
    /// a -> b -> c (external)
    ///
    /// ancestors(c) = {a, b}
    pub fn ancestors(&self, node: &WorkspaceNode) -> HashSet<&WorkspaceNode> {
        let mut dependents =
            self.transitive_closure_inner(Some(node), petgraph::Direction::Incoming);
        dependents.remove(node);
        dependents
    }

    /// Returns the transitive closure of the given nodes in the workspace
    /// graph. Note that this includes the nodes themselves. If you want just
    /// the dependencies, or the dependents, use `dependencies` or `ancestors`.
    /// Alternatively, if you need just direct dependents, use
    /// `immediate_dependents`.
    pub fn transitive_closure<'a, 'b, I: IntoIterator<Item = &'b WorkspaceNode>>(
        &'a self,
        nodes: I,
    ) -> HashSet<&'a WorkspaceNode> {
        self.transitive_closure_inner(nodes, petgraph::Direction::Outgoing)
    }

    fn transitive_closure_inner<'a, 'b, I: IntoIterator<Item = &'b WorkspaceNode>>(
        &'a self,
        nodes: I,
        direction: petgraph::Direction,
    ) -> HashSet<&'a WorkspaceNode> {
        let indices = nodes
            .into_iter()
            .filter_map(|node| self.node_lookup.get(node))
            .copied();

        let mut visited = HashSet::new();

        let visitor = |event| {
            if let petgraph::visit::DfsEvent::Discover(n, _) = event {
                visited.insert(
                    self.workspace_graph
                        .node_weight(n)
                        .expect("node index found during dfs doesn't exist"),
                );
            }
        };

        match direction {
            petgraph::Direction::Outgoing => {
                depth_first_search(&self.workspace_graph, indices, visitor)
            }
            petgraph::Direction::Incoming => {
                depth_first_search(Reversed(&self.workspace_graph), indices, visitor)
            }
        };

        visited
    }

    pub fn transitive_external_dependencies<'a, I: IntoIterator<Item = &'a WorkspaceName>>(
        &self,
        workspaces: I,
    ) -> HashSet<&turborepo_lockfiles::Package> {
        workspaces
            .into_iter()
            .filter_map(|workspace| self.workspaces.get(workspace))
            .filter_map(|entry| entry.transitive_dependencies.as_ref())
            .flatten()
            .collect()
    }

    /// Returns a list of changed packages based on the contents of a previous
    /// `Lockfile`. This assumes that none of the package.json in the workspace
    /// change, it is the responsibility of the caller to verify this.
    pub fn changed_packages_from_lockfile(
        &self,
        previous: &dyn Lockfile,
    ) -> Result<Vec<WorkspaceName>, ChangedPackagesError> {
        let current = self.lockfile().ok_or(ChangedPackagesError::NoLockfile)?;

        let external_deps = self
            .workspaces()
            .filter_map(|(_name, info)| {
                info.unresolved_external_dependencies.as_ref().map(|dep| {
                    (
                        info.package_path().to_unix().to_string(),
                        dep.iter()
                            .map(|(name, version)| (name.to_owned(), version.to_owned()))
                            .collect(),
                    )
                })
            })
            .collect::<HashMap<_, HashMap<_, _>>>();

        let closures = turborepo_lockfiles::all_transitive_closures(previous, external_deps)?;

        let global_change = current.global_change(previous);

        let changed = if global_change {
            None
        } else {
            self.workspaces
                .iter()
                .filter(|(_name, info)| {
                    closures.get(info.package_path().to_unix().as_str())
                        != info.transitive_dependencies.as_ref()
                })
                .map(|(name, _info)| match name {
                    WorkspaceName::Other(n) => Some(WorkspaceName::Other(n.to_owned())),
                    // if the root package has changed, then we should report `None`
                    // since all packages need to be revalidated
                    WorkspaceName::Root => None,
                })
                .collect::<Option<Vec<WorkspaceName>>>()
        };

        Ok(changed.unwrap_or_else(|| self.workspaces.keys().cloned().collect()))
    }

    #[allow(dead_code)]
    fn external_dependencies(
        &self,
        workspace: &WorkspaceName,
    ) -> Option<&BTreeMap<PackageName, PackageVersion>> {
        let entry = self.workspaces.get(workspace)?;
        entry.unresolved_external_dependencies.as_ref()
    }
}

#[derive(thiserror::Error, Debug)]
pub enum ChangedPackagesError {
    #[error("No lockfile")]
    NoLockfile,
    #[error("Lockfile error")]
    Lockfile(#[from] turborepo_lockfiles::Error),
}

impl fmt::Display for WorkspaceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkspaceName::Root => f.write_str("//"),
            WorkspaceName::Other(other) => f.write_str(other),
        }
    }
}

impl fmt::Display for WorkspaceNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkspaceNode::Root => f.write_str("___ROOT___"),
            WorkspaceNode::Workspace(workspace) => workspace.fmt(f),
        }
    }
}
impl From<String> for WorkspaceName {
    fn from(value: String) -> Self {
        match value == "//" {
            true => Self::Root,
            false => Self::Other(value),
        }
    }
}

impl<'a> From<&'a str> for WorkspaceName {
    fn from(value: &'a str) -> Self {
        Self::from(value.to_string())
    }
}

impl AsRef<str> for WorkspaceName {
    fn as_ref(&self) -> &str {
        match self {
            WorkspaceName::Root => "//",
            WorkspaceName::Other(workspace) => workspace,
        }
    }
}

#[cfg(test)]
mod test {
    use std::assert_matches::assert_matches;

    use serde_json::json;
    use turbopath::AbsoluteSystemPathBuf;

    use super::*;
    use crate::discovery::PackageDiscovery;

    struct MockDiscovery;
    impl PackageDiscovery for MockDiscovery {
        async fn discover_packages(
            &mut self,
        ) -> Result<crate::discovery::DiscoveryResponse, crate::discovery::Error> {
            Ok(crate::discovery::DiscoveryResponse {
                package_manager: PackageManager::Npm,
                workspaces: vec![],
            })
        }
    }

    #[tokio::test]
    async fn test_single_package_is_depends_on_root() {
        let root =
            AbsoluteSystemPathBuf::new(if cfg!(windows) { r"C:\repo" } else { "/repo" }).unwrap();
        let pkg_graph = PackageGraph::builder(&root, PackageJson::default())
            .with_package_discovery(MockDiscovery)
            .with_single_package_mode(true)
            .build()
            .await
            .unwrap();

        let closure =
            pkg_graph.transitive_closure(Some(&WorkspaceNode::Workspace(WorkspaceName::Root)));
        assert!(closure.contains(&WorkspaceNode::Root));
        assert!(pkg_graph.validate().is_ok());
    }

    #[tokio::test]
    async fn test_internal_dependencies_get_split_out() {
        let root =
            AbsoluteSystemPathBuf::new(if cfg!(windows) { r"C:\repo" } else { "/repo" }).unwrap();
        let pkg_graph = PackageGraph::builder(
            &root,
            PackageJson::from_value(json!({ "name": "root" })).unwrap(),
        )
        .with_package_discovery(MockDiscovery)
        .with_package_jsons(Some({
            let mut map = HashMap::new();
            map.insert(
                root.join_component("package_a"),
                PackageJson::from_value(json!({
                    "name": "a",
                    "dependencies": {
                        "b": "workspace:*"
                    }
                }))
                .unwrap(),
            );
            map.insert(
                root.join_component("package_b"),
                PackageJson::from_value(json!({
                    "name": "b",
                    "dependencies": {
                        "c": "1.2.3",
                    }
                }))
                .unwrap(),
            );
            map
        }))
        .build()
        .await
        .unwrap();

        assert!(pkg_graph.validate().is_ok());
        let closure = pkg_graph.transitive_closure(Some(&WorkspaceNode::Workspace("a".into())));
        assert_eq!(
            closure,
            [
                WorkspaceNode::Root,
                WorkspaceNode::Workspace("a".into()),
                WorkspaceNode::Workspace("b".into())
            ]
            .iter()
            .collect::<HashSet<_>>()
        );
        let b_external = pkg_graph
            .workspaces
            .get(&WorkspaceName::from("b"))
            .unwrap()
            .unresolved_external_dependencies
            .as_ref()
            .unwrap();

        let pkg_version = b_external.get("c").unwrap();
        assert_eq!(pkg_version, "1.2.3");
    }

    #[derive(Debug)]
    struct MockLockfile {}
    impl turborepo_lockfiles::Lockfile for MockLockfile {
        fn resolve_package(
            &self,
            _workspace_path: &str,
            name: &str,
            _version: &str,
        ) -> std::result::Result<Option<turborepo_lockfiles::Package>, turborepo_lockfiles::Error>
        {
            Ok(match name {
                "a" => Some(turborepo_lockfiles::Package::new("key:a", "1")),
                "b" => Some(turborepo_lockfiles::Package::new("key:b", "1")),
                "c" => Some(turborepo_lockfiles::Package::new("key:c", "1")),
                _ => None,
            })
        }

        fn all_dependencies(
            &self,
            key: &str,
        ) -> std::result::Result<Option<HashMap<String, String>>, turborepo_lockfiles::Error>
        {
            match key {
                "key:a" => Ok(Some(
                    [("c", "1")]
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                )),
                "key:b" => Ok(Some(
                    [("c", "1")]
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                )),
                "key:c" => Ok(None),
                _ => Ok(None),
            }
        }

        fn subgraph(
            &self,
            _workspace_packages: &[String],
            _packages: &[String],
        ) -> std::result::Result<Box<dyn Lockfile>, turborepo_lockfiles::Error> {
            unreachable!("lockfile pruning not necessary for package graph construction")
        }

        fn encode(&self) -> std::result::Result<Vec<u8>, turborepo_lockfiles::Error> {
            unreachable!("lockfile encoding not necessary for package graph construction")
        }

        fn global_change(&self, _other: &dyn Lockfile) -> bool {
            unreachable!("global change detection not necessary for package graph construction")
        }
    }

    #[tokio::test]
    async fn test_lockfile_traversal() {
        let root =
            AbsoluteSystemPathBuf::new(if cfg!(windows) { r"C:\repo" } else { "/repo" }).unwrap();
        let pkg_graph = PackageGraph::builder(
            &root,
            PackageJson::from_value(json!({ "name": "root" })).unwrap(),
        )
        .with_package_discovery(MockDiscovery)
        .with_package_jsons(Some({
            let mut map = HashMap::new();
            map.insert(
                root.join_components(&["package_a", "package.json"]),
                PackageJson::from_value(json!({
                    "name": "foo",
                    "dependencies": {
                        "a": "1"
                    }
                }))
                .unwrap(),
            );
            map.insert(
                root.join_components(&["package_b", "package.json"]),
                PackageJson::from_value(json!({
                    "name": "bar",
                    "dependencies": {
                        "b": "1",
                    }
                }))
                .unwrap(),
            );
            map
        }))
        .with_lockfile(Some(Box::new(MockLockfile {})))
        .build()
        .await
        .unwrap();

        assert!(pkg_graph.validate().is_ok());
        let foo = WorkspaceName::from("foo");
        let bar = WorkspaceName::from("bar");

        let foo_deps = pkg_graph
            .workspaces
            .get(&foo)
            .unwrap()
            .transitive_dependencies
            .as_ref()
            .unwrap();
        let bar_deps = pkg_graph
            .workspaces
            .get(&bar)
            .unwrap()
            .transitive_dependencies
            .as_ref()
            .unwrap();
        let a = turborepo_lockfiles::Package::new("key:a", "1");
        let b = turborepo_lockfiles::Package::new("key:b", "1");
        let c = turborepo_lockfiles::Package::new("key:c", "1");
        assert_eq!(foo_deps, &HashSet::from_iter(vec![a.clone(), c.clone(),]));
        assert_eq!(bar_deps, &HashSet::from_iter(vec![b.clone(), c.clone(),]));
        assert_eq!(
            pkg_graph.transitive_external_dependencies([&foo, &bar].iter().copied()),
            HashSet::from_iter(vec![&a, &b, &c,])
        );
    }

    #[tokio::test]
    async fn test_circular_dependency() {
        let root =
            AbsoluteSystemPathBuf::new(if cfg!(windows) { r"C:\repo" } else { "/repo" }).unwrap();
        let pkg_graph = PackageGraph::builder(
            &root,
            PackageJson::from_value(json!({ "name": "root" })).unwrap(),
        )
        .with_package_discovery(MockDiscovery)
        .with_package_jsons(Some({
            let mut map = HashMap::new();
            map.insert(
                root.join_component("package_a"),
                PackageJson::from_value(json!({
                    "name": "foo",
                    "dependencies": {
                        "bar": "*"
                    }
                }))
                .unwrap(),
            );
            map.insert(
                root.join_component("package_b"),
                PackageJson::from_value(json!({
                    "name": "bar",
                    "dependencies": {
                        "baz": "*",
                    }
                }))
                .unwrap(),
            );
            map.insert(
                root.join_component("package_c"),
                PackageJson::from_value(json!({
                    "name": "baz",
                    "dependencies": {
                        "foo": "*",
                    }
                }))
                .unwrap(),
            );
            map
        }))
        .with_lockfile(Some(Box::new(MockLockfile {})))
        .build()
        .await
        .unwrap();

        assert_matches!(
            pkg_graph.validate(),
            Err(builder::Error::InvalidPackageGraph(
                graph::Error::CyclicDependencies(_)
            ))
        );
    }

    #[tokio::test]
    async fn test_self_dependency() {
        let root =
            AbsoluteSystemPathBuf::new(if cfg!(windows) { r"C:\repo" } else { "/repo" }).unwrap();
        let pkg_graph = PackageGraph::builder(
            &root,
            PackageJson::from_value(json!({ "name": "root" })).unwrap(),
        )
        .with_package_discovery(MockDiscovery)
        .with_package_jsons(Some({
            let mut map = HashMap::new();
            map.insert(
                root.join_component("package_a"),
                PackageJson::from_value(json!({
                    "name": "foo",
                    "dependencies": {
                        "foo": "*"
                    }
                }))
                .unwrap(),
            );
            map
        }))
        .with_lockfile(Some(Box::new(MockLockfile {})))
        .build()
        .await
        .unwrap();

        assert_matches!(
            pkg_graph.validate(),
            Err(builder::Error::InvalidPackageGraph(
                graph::Error::SelfDependency(_)
            ))
        );
    }
}
