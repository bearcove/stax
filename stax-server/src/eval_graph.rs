//! Isolated graph state: imports never select/stop/replace a profiler run.
use parking_lot::Mutex;
use stax_eval_graph::{Graph, Limits};
use stax_live_proto::*;
use std::collections::BTreeMap;
use std::sync::Arc;

const MAX_CAPTURES: usize = 4;
#[derive(Default)]
struct Registry {
    next: u64,
    graphs: BTreeMap<u64, Arc<Graph>>,
}
#[derive(Clone, Default)]
pub struct GraphService {
    registry: Arc<Mutex<Registry>>,
    /// One reader at a time; retained captures are never evicted to make room.
    import_lock: Arc<tokio::sync::Mutex<()>>,
}
impl GraphService {
    fn graph(&self, id: u64) -> Result<Arc<Graph>, String> {
        self.registry
            .lock()
            .graphs
            .get(&id)
            .cloned()
            .ok_or_else(|| {
                format!("unknown capture {id}; reopen its JSONL path after a server restart")
            })
    }
    async fn query<T: Send + 'static>(
        &self,
        id: u64,
        query: impl FnOnce(Arc<Graph>) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let graph = self.graph(id)?;
        tokio::task::spawn_blocking(move || query(graph))
            .await
            .map_err(|e| e.to_string())?
    }
}
impl EvalGraph for GraphService {
    async fn import_path(&self, path: String) -> Result<GraphSummary, String> {
        let _guard = self.import_lock.lock().await;
        let id = {
            let mut registry = self.registry.lock();
            if registry.graphs.len() >= MAX_CAPTURES {
                return Err(format!(
                    "at most {MAX_CAPTURES} imported captures; explicitly close one before importing another"
                ));
            }
            registry.next = registry
                .next
                .checked_add(1)
                .ok_or("capture IDs exhausted")?;
            registry.next
        };
        let graph = tokio::task::spawn_blocking(move || {
            Graph::from_path(std::path::Path::new(&path), id, Limits::default())
        })
        .await
        .map_err(|e| e.to_string())??;
        let summary = graph.summary.clone();
        self.registry.lock().graphs.insert(id, Arc::new(graph));
        Ok(summary)
    }
    async fn captures(&self) -> Vec<GraphSummary> {
        self.registry
            .lock()
            .graphs
            .values()
            .map(|g| g.summary.clone())
            .collect()
    }
    async fn close(&self, capture_id: u64) -> Result<(), String> {
        self.registry
            .lock()
            .graphs
            .remove(&capture_id)
            .map(|_| ())
            .ok_or_else(|| format!("unknown capture {capture_id}"))
    }
    async fn summary(&self, capture_id: u64) -> Result<GraphSummary, String> {
        Ok(self.graph(capture_id)?.summary.clone())
    }
    async fn search(&self, p: GraphSearchQuery) -> Result<GraphNodePage, String> {
        self.query(p.capture_id, move |graph| {
            graph.search(&p.query, &p.kind, p.offset, p.limit)
        })
        .await
    }
    async fn callables(
        &self,
        capture_id: u64,
        query: String,
        offset: u64,
        limit: u32,
    ) -> Result<GraphCallablePage, String> {
        self.query(capture_id, move |graph| {
            graph.callables(&query, offset, limit)
        })
        .await
    }
    async fn invocations(&self, p: GraphInvocationQuery) -> Result<GraphNodePage, String> {
        self.query(p.capture_id, move |graph| {
            graph.invocations(p.module, p.site, p.offset, p.limit)
        })
        .await
    }
    async fn inspect(&self, capture_id: u64, node: String) -> Result<GraphNode, String> {
        self.graph(capture_id)?.inspect(&node)
    }
    async fn neighborhood(&self, p: GraphNeighborhoodQuery) -> Result<GraphNeighborhood, String> {
        self.query(p.capture_id, move |graph| {
            graph.neighborhood(&p.node, &p.direction, p.offset, p.limit)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn graph_rpc_uses_production_router_and_is_independent_of_runs() {
        let server = crate::ServerState::new_for_tests();
        let socket = std::env::temp_dir().join(format!(
            "stax-graph-rpc-{}-{}.sock",
            std::process::id(),
            crate::now_unix_ns()
        ));
        let listener =
            vox::transport::local::LocalLinkAcceptor::bind(socket.to_string_lossy().into_owned())
                .unwrap();
        let accept = crate::spawn_accept_loop_local(server.clone(), listener);
        let client: EvalGraphClient = vox::connect_lane(&format!("local://{}", socket.display()))
            .await
            .unwrap();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../test-data/eval-graph/shared.jsonl");
        let summary = client
            .import_path(path.to_string_lossy().into_owned())
            .await
            .unwrap();
        assert!(summary.complete);
        assert_eq!(summary.invocations, 3);
        assert_eq!(summary.cache_computes, 2);
        let n = client
            .neighborhood(GraphNeighborhoodQuery {
                capture_id: summary.capture_id,
                node: "thunk:1".into(),
                direction: "incoming".into(),
                offset: 0,
                limit: 200,
            })
            .await
            .unwrap();
        assert_eq!(n.nodes.len(), 3);
        assert_eq!(n.total_relations, 4);
        // Counts mode travels through the same production router and generated schema.
        let counts_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../test-data/eval-graph/counts.jsonl");
        let counts = client
            .import_path(counts_path.to_string_lossy().into_owned())
            .await
            .unwrap();
        assert_eq!(counts.capture_mode, "counts");
        assert!(counts.complete && counts.counts_exact);
        assert_eq!(counts.counts.as_ref().unwrap().body_starts, 10);
        assert_eq!(counts.executions[0].execution, Some(1));
        assert_eq!((counts.nodes, counts.relations), (0, 0));
        let callables = client
            .callables(counts.capture_id, "twice".into(), 0, 10)
            .await
            .unwrap();
        assert_eq!(
            callables.callables[0].counts.as_ref().unwrap().body_starts,
            10
        );
        let error = client
            .search(GraphSearchQuery {
                capture_id: counts.capture_id,
                kind: "all".into(),
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no invocation/argument detail"));
        client.close(counts.capture_id).await.unwrap();
        assert!(server.inner.lock().active.is_none());
        assert!(server.inner.lock().history.is_empty());
        client.close(summary.capture_id).await.unwrap();
        assert!(client.summary(summary.capture_id).await.is_err());
        accept.abort();
        let _ = std::fs::remove_file(socket);
    }

    #[tokio::test]
    async fn registry_import_query_close_and_new_capture_isolation() {
        let path =
            std::env::temp_dir().join(format!("stax-graph-registry-{}.jsonl", std::process::id()));
        std::fs::write(&path,concat!("{\"event\":\"header\",\"seq\":1,\"ts_ns\":0,\"format\":\"vix-eval-graph\",\"version\":1,\"semantics\":\"lazy-dataflow\",\"records\":1}\n", "{\"event\":\"allocation\",\"seq\":2,\"ts_ns\":1,\"thunk\":1,\"words\":8}\n", "{\"event\":\"footer\",\"seq\":3,\"ts_ns\":2,\"complete\":true,\"records\":3}\n")).unwrap();
        let service = GraphService::default();
        let mut ids = Vec::new();
        for _ in 0..MAX_CAPTURES {
            let s = service
                .import_path(path.to_string_lossy().into_owned())
                .await
                .unwrap();
            assert!(s.complete);
            ids.push(s.capture_id);
        }
        assert_ne!(ids[0], ids[1]);
        assert!(
            service
                .import_path(path.to_string_lossy().into_owned())
                .await
                .is_err()
        );
        assert_eq!(service.captures().await.len(), MAX_CAPTURES);
        assert_eq!(
            service.inspect(ids[0], "thunk:1".into()).await.unwrap().id,
            "thunk:1"
        );
        service.close(ids[0]).await.unwrap();
        assert!(service.summary(ids[0]).await.is_err());
        assert!(service.summary(ids[1]).await.is_ok());
        let s = service
            .import_path(path.to_string_lossy().into_owned())
            .await
            .unwrap();
        assert!(s.capture_id > ids[1]);
        std::fs::remove_file(path).unwrap();
    }
}
