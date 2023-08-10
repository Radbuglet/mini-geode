use std::{any::TypeId, fmt::Write, rc::Rc};

use petgraph::{
    algo::toposort,
    graph::{EdgeIndex, NodeIndex},
    visit::EdgeRef,
    Graph,
};
use rustc_hash::FxHashMap;

// === Helpers === //

#[derive(Debug, Copy, Clone)]
pub enum Mutability {
    Mutable,
    Immutable,
}

impl Mutability {
    pub fn adjective(self) -> &'static str {
        match self {
            Mutability::Mutable => "mutably",
            Mutability::Immutable => "immutably",
        }
    }

    pub fn is_compatible_with(self, other: Mutability) -> bool {
        use Mutability::*;
        matches!((self, other), (Immutable, Immutable))
    }

    pub fn strictest(self, other: Mutability) -> Self {
        use Mutability::*;
        if matches!((self, other), (Immutable, Immutable)) {
            Immutable
        } else {
            Mutable
        }
    }
}

// === Validator === //

#[derive(Debug)]
pub struct Validator {
    /// The graph of behavior namespaces connected by the behaviors which could possibly call into
    /// other namespaces.
    graph: Graph<Namespace, Rc<Behavior>>,

    /// A map from namespace types to namespace nodes.
    namespace_ty_map: FxHashMap<TypeId, NodeIndex>,

    /// A map from component type IDs to component type names.
    component_names: FxHashMap<TypeId, &'static str>,
}

#[derive(Debug)]
struct Namespace {
    /// The location where the namespace's universe was defined.
    universe_def_loc: &'static str,

    /// The location where this namespace was defined.
    my_def_loc: &'static str,

    /// The set of behaviors which borrow data in the namespace but don't actually call into any other
    /// behaviors.
    terminal_behaviors: Vec<Behavior>,
}

#[derive(Debug)]
struct Behavior {
    /// The location where the behavior was defined.
    def_path: &'static str,

    /// The set of components borrowed by the behavior.
    borrows: FxHashMap<TypeId, Mutability>,
}

impl Validator {
    fn get_namespace(
        &mut self,
        universe: &'static str,
        namespace: (TypeId, &'static str),
    ) -> NodeIndex {
        match self.namespace_ty_map.entry(namespace.0) {
            std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let graph = self.graph.add_node(Namespace {
                    universe_def_loc: universe,
                    my_def_loc: namespace.1,
                    terminal_behaviors: Vec::new(),
                });
                *entry.insert(graph)
            }
        }
    }

    pub fn add_behavior(
        &mut self,
        universe: &'static str,
        namespace: (TypeId, &'static str),
        my_path: &'static str,
        borrows: impl IntoIterator<Item = (TypeId, &'static str, Mutability)>,
        calls: impl IntoIterator<Item = (TypeId, &'static str)>,
    ) {
        // Create the namespace node
        let src_idx = self.get_namespace(universe, namespace);

        // Construct the behavior
        let borrows = borrows
            .into_iter()
            .map(|(id, name, perms)| {
                self.component_names.entry(id).or_insert(name);
                (id, perms)
            })
            .collect();

        let behavior = Behavior {
            def_path: my_path,
            borrows,
        };

        // Construct an edge for every call or register the behavior as terminal
        {
            let mut iter = calls.into_iter();
            let mut curr = iter.next();

            if curr.is_some() {
                let behavior = Rc::new(behavior);

                // We have edges to connect
                while let Some(call) = curr {
                    let dst_idx = self.get_namespace(universe, call);
                    self.graph.add_edge(src_idx, dst_idx, behavior.clone());
                    curr = iter.next();
                }
            } else {
                // This is a terminal edge
                self.graph[src_idx].terminal_behaviors.push(behavior);
            }
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        // Assuming our graph is a DAG, toposort the namespaces.
        let Ok(topos) = toposort(&self.graph, None) else {
			// If the graph is not a DAG, we know that it is invalid since a dependency issue could
			// be induced by taking the same borrowing edge several times.
			//
			// We generate a list of offending namespaces using "Tarjan's strongly connected components
			// algorithm." A strongly connected component (or SCC) is a set of nodes in a graph
			// where each node in the set has a path to another node in that set. We know that
			// finding the SCCs in a graph is an effective way of finding portions of the graph
			// containing cycles because:
			//
			// 1. If the graph contains a cycle, that cycle will be part of an SCC (although the SCC may
			//    contain more nodes than just it).
			// 2. If the graph contains an SCC, within that SCC, we can construct many simple cycles
			//    by taking any of the paths from any of the nodes to itself.
			//
			// Hence, determining SCCs is an effective way of printing out portions of the graph with
			// offending cycles.
			//
			// We decided to list out SCCs rather than simple cycles because, in the worst case scenario,
			// the number of simple cycles in a graph grows factorially w.r.t the number of vertices.
			// This is because, in a K^n graph, our cycles would be at least all possible permutations of
			// those `n` nodes.
			let mut sccs = petgraph::algo::tarjan_scc(&self.graph);
			let mut f = String::new();
			write!(f, "Failed to validate behavior graph: behaviors may be called in a cycle, which could cause borrow violations.").unwrap();

			// TODO: Pretty-print this information.

			return Err(f);
		};

        // Working in topological order, we populate the set of all components which could possibly
        // be borrowed when a namespace is called.
        struct ValidationCx<'a> {
            validator: &'a Validator,
            potentially_borrowed: Vec<FxHashMap<TypeId, (Mutability, Vec<EdgeIndex>)>>,
            err_msg_or_empty: String,
        }

        impl<'a> ValidationCx<'a> {
            pub fn new(validator: &'a Validator, node_count: usize) -> Self {
                Self {
                    validator,
                    potentially_borrowed: (0..node_count).map(|_| FxHashMap::default()).collect(),
                    err_msg_or_empty: String::new(),
                }
            }

            pub fn validate_behavior(&mut self, node: NodeIndex, behavior: &Behavior) {
                let f = &mut self.err_msg_or_empty;
                let pbs = &self.potentially_borrowed[node.index()];

                for (&req_ty, &req_mut) in &behavior.borrows {
                    // If the request is compatible with the PBS, ignore it.
                    let Some((pre_mut, pre_contrib)) = pbs.get(&req_ty) else { continue };

                    if pre_mut.is_compatible_with(req_mut) {
                        return;
                    }

                    // Otherwise, log out the error chain.
                    // TODO: Pretty-print the chain of borrows.
                    write!(
                        f,
                        "Behavior in namespace {} defined at {} borrows component {} {} even though it may have already been borrowed {}.",
						self.validator.graph[node].my_def_loc,
						behavior.def_path,
						self.validator.component_names[&req_ty],
						req_mut.adjective(),
						pre_mut.adjective(),
                    )
                    .unwrap();
                }
            }

            pub fn extend_borrows(
                &mut self,
                calling_edge: EdgeIndex,
                calling_bhv: &Behavior,
                callee: NodeIndex,
            ) {
                let callee_pbs = &mut self.potentially_borrowed[callee.index()];

                for (&req_ty, &req_mut) in &calling_bhv.borrows {
                    match callee_pbs.entry(req_ty) {
                        std::collections::hash_map::Entry::Occupied(entry) => {
                            let (pbs_mut, pbs_requesters) = entry.into_mut();
                            *pbs_mut = pbs_mut.strictest(req_mut);
                            pbs_requesters.push(calling_edge);
                        }
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            entry.insert((req_mut, vec![calling_edge]));
                        }
                    }
                }
            }
        }

        let mut cx = ValidationCx::new(self, self.graph.node_count());

        for src_idx in topos {
            let src = &self.graph[src_idx];

            // For every terminal behavior, check it against the PBS.
            for terminal in &src.terminal_behaviors {
                cx.validate_behavior(src_idx, terminal);
            }

            // For every non-terminal behavior, check it against the PBS and then extend the future
            // nodes.
            for edge in self.graph.edges(src_idx) {
                let edge_bhv = &self.graph[edge.id()];
                cx.validate_behavior(src_idx, edge_bhv);
                cx.extend_borrows(edge.id(), edge_bhv, edge.target());
            }
        }

        // If we had any errors while validating this graph
        if !cx.err_msg_or_empty.is_empty() {
            return Err(format!(
                "Failed to validate behavior graph:\n\n{}",
                cx.err_msg_or_empty,
            ));
        }

        // Otherwise, the graph is fully valid.
        Ok(())
    }
}
