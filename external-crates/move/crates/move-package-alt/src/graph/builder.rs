// Copyright (c) The Diem Core Contributors
// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    dependency::PinnedDependencyInfo,
    errors::{PackageError, PackageResult},
    flavor::MoveFlavor,
    package::{EnvironmentName, Package, lockfile::Lockfiles, paths::PackagePath},
    schema::{Environment, PackageName},
};

use std::{
    collections::{BTreeMap, btree_map::Entry},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use petgraph::graph::{DiGraph, NodeIndex};
use tokio::sync::OnceCell;

use super::{PackageGraph, PackageNode};

struct PackageCache<F: MoveFlavor> {
    // TODO: better errors; I'm using Option for now because PackageResult doesn't have clone, but
    // it's too much effort to add clone everywhere; we should do this when we update the error
    // infra
    // TODO: would dashmap simplify this?
    cache: Mutex<BTreeMap<PathBuf, Arc<OnceCell<Option<Arc<Package<F>>>>>>>,
}

pub struct PackageGraphBuilder<F: MoveFlavor> {
    cache: PackageCache<F>,
}

impl<F: MoveFlavor> PackageGraphBuilder<F> {
    pub fn new() -> Self {
        Self {
            cache: PackageCache::new(),
        }
    }

    /// Loads the package graph for `env`. It checks whether the
    /// resolution graph in the lockfile is up-to-date (i.e., whether any of the
    /// manifests digests are out of date). If the resolution graph is up-to-date, it is returned.
    /// Otherwise a new resolution graph is constructed by traversing (only) the manifest files.
    pub async fn load(
        &self,
        path: &PackagePath,
        env: &Environment,
    ) -> PackageResult<PackageGraph<F>> {
        let lockfile = self.load_from_lockfile(path, env).await?;
        match lockfile {
            Some(result) => Ok(result),
            None => self.load_from_manifests(path, env).await,
        }
    }

    /// Load a [PackageGraph] from the lockfile at `path`. Returns [None] if the contents of the
    /// lockfile are out of date (i.e. if the lockfile doesn't exist or the manifest digests don't
    /// match).
    pub async fn load_from_lockfile(
        &self,
        path: &PackagePath,
        env: &Environment,
    ) -> PackageResult<Option<PackageGraph<F>>> {
        self.load_from_lockfile_impl(path, env, true).await
    }

    /// Load a [PackageGraph] from the lockfile at `path`. Returns [None] if there is no lockfile
    pub async fn load_from_lockfile_ignore_digests(
        &self,
        path: &PackagePath,
        env: &Environment,
    ) -> PackageResult<Option<PackageGraph<F>>> {
        self.load_from_lockfile_impl(path, env, false).await
    }

    /// Load a [PackageGraph] from the lockfile at `path`. Returns [None] if there is no lockfile.
    /// Also returns [None] if `check_digests` is true and any of the digests don't match.
    pub async fn load_from_lockfile_impl(
        &self,
        path: &PackagePath,
        env: &Environment,
        check_digests: bool,
    ) -> PackageResult<Option<PackageGraph<F>>> {
        let Some(lockfile) = Lockfiles::<F>::read_from_dir(path)? else {
            return Ok(None);
        };

        let mut inner = DiGraph::new();

        let mut package_nodes = BTreeMap::new();

        let Some(pins) = lockfile.pins_for_env(env.name()) else {
            return Ok(None);
        };

        // First pass: create nodes for all packages
        for (pkg_id, pin) in pins.iter() {
            let dep = PinnedDependencyInfo::from_pin(lockfile.file(), env.name(), pin);
            let package = self.cache.fetch(&dep, env).await?;
            let package_manifest_digest = package.digest();
            if check_digests && package_manifest_digest != &pin.manifest_digest {
                return Ok(None);
            }
            let index = inner.add_node(PackageNode {
                package,
                use_env: pin.use_environment.clone().unwrap_or(env.name().clone()),
            });
            package_nodes.insert(pkg_id.clone(), index);
        }

        // Second pass: add edges based on dependencies
        for (pkg_id, dep_info) in pins.iter() {
            let from_index = package_nodes.get(pkg_id).unwrap();
            for (dep_name, dep_id) in dep_info.deps.iter() {
                if let Some(to_index) = package_nodes.get(dep_id) {
                    inner.add_edge(*from_index, *to_index, dep_name.clone());
                }
            }
        }

        // TODO(manos): Add a proper error message here -- nothing to expect.
        let root_idx = inner
            .node_indices()
            .find(|pkg| {
                let node = &inner[*pkg];
                node.package.is_root()
            })
            .expect("A lockfile needs to have a root package");

        Ok(Some(PackageGraph { inner, root_idx }))
    }

    /// Construct a new package graph for `env` by recursively fetching and reading manifest files
    /// starting from the package at `path`.
    /// Lockfiles are ignored. See [PackageGraph::load]
    pub async fn load_from_manifests(
        &self,
        path: &PackagePath,
        env: &Environment,
    ) -> PackageResult<PackageGraph<F>> {
        // TODO: this is wrong - it is ignoring `path`
        let graph = Arc::new(Mutex::new(DiGraph::new()));
        let visited = Arc::new(Mutex::new(BTreeMap::new()));
        let root = Arc::new(Package::<F>::load_root(path, env).await?);

        let root_idx = self
            .add_transitive_manifest_deps(root, env, graph.clone(), visited)
            .await?;

        let graph = graph.lock().expect("unpoisoned").map(
            |_, node| {
                node.clone()
                    .expect("add_transitive_packages removes all `None`s before returning")
            },
            |_, e| e.clone(),
        );

        Ok(PackageGraph {
            inner: graph,
            root_idx,
        })
    }

    /// Adds nodes and edges for the graph rooted at `package` to `graph` and returns the node ID for
    /// `package`. Nodes are constructed by fetching the dependencies. If this function returns successfully,
    /// all nodes that it adds to `graph` will be set to `Some`.
    ///
    /// `visited` is used to short-circuit refetching - if a node is in `visited` then neither it nor its
    /// dependencies will be readded.
    pub async fn add_transitive_manifest_deps(
        &self,
        package: Arc<Package<F>>,
        env: &Environment,
        graph: Arc<Mutex<DiGraph<Option<PackageNode<F>>, PackageName>>>,
        visited: Arc<Mutex<BTreeMap<(EnvironmentName, PathBuf), NodeIndex>>>,
    ) -> PackageResult<NodeIndex> {
        // return early if node is cached; add empty node to graph and visited list otherwise
        let index = match visited
            .lock()
            .expect("unpoisoned")
            .entry((env.name().clone(), package.path().as_ref().to_path_buf()))
        {
            Entry::Occupied(entry) => return Ok(*entry.get()),
            Entry::Vacant(entry) => *entry.insert(graph.lock().expect("unpoisoned").add_node(None)),
        };

        // add outgoing edges for dependencies
        // Note: this loop could be parallel if we want parallel fetching:
        for (name, dep) in package.direct_deps().iter() {
            let fetched = self.cache.fetch(dep, env).await?;

            // We retain the defined environment name, but we assign a consistent chain id (environmentID).
            let new_env = Environment::new(dep.use_environment().clone(), env.id().clone());

            let future = self.add_transitive_manifest_deps(
                fetched.clone(),
                &new_env,
                graph.clone(),
                visited.clone(),
            );
            let dep_index = Box::pin(future).await?;

            // TODO(manos): re-check the implementation here --  to make sure nothing was missed.
            // TODO(manos)(2): Do we wanna error for missmatches on legacy packages? Will come on a follow-up.
            // TODO(manos)(3): Do we wanna rename only for legacy parents, and error out for modern parents?
            // If we're dealing with legacy packages, we are free to fix the naming in the outgoing edge, to match
            // our modern system names.
            let edge_name = if fetched.is_legacy() {
                fetched.name()
            } else {
                name
            };

            graph
                .lock()
                .expect("unpoisoned")
                .add_edge(index, dep_index, edge_name.clone());
        }

        graph
            .lock()
            .expect("unpoisoned")
            .node_weight_mut(index)
            .expect("node was added above")
            .replace(PackageNode {
                package,
                use_env: env.name().clone(),
            });

        Ok(index)
    }
}

impl<F: MoveFlavor> PackageCache<F> {
    /// Construct a new empty cache
    pub fn new() -> Self {
        Self {
            cache: Mutex::default(),
        }
    }

    /// Return a reference to a cached [Package], loading it if necessary
    pub async fn fetch(
        &self,
        dep: &PinnedDependencyInfo,
        env: &Environment,
    ) -> PackageResult<Arc<Package<F>>> {
        let cell = self
            .cache
            .lock()
            .expect("unpoisoned")
            .entry(dep.unfetched_path())
            .or_default()
            .clone();

        // TODO: this refetches if there was a previous error, it should save the error instead

        // First try to get cached result
        if let Some(Some(cached)) = cell.get() {
            return Ok(cached.clone());
        }

        // If not cached, load and cache
        match Package::load(dep.clone(), env).await {
            Ok(package) => {
                let node = Arc::new(package);
                cell.get_or_init(async || Some(node.clone())).await;
                Ok(node)
            }
            Err(e) => Err(PackageError::Generic(format!(
                "Failed to load package from {}: {}",
                dep.unfetched_path().display(),
                e
            ))),
        }
    }
}
