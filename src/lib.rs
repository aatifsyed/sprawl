use async_recursion::async_recursion;
use futures::future::join_all;
use petgraph::graph::DiGraph;
use reqwest::Client;
use std::collections::{HashMap, HashSet};
use tokio::sync::{Mutex, RwLock};
use tracing::{info, instrument};
use url::Url;

async fn get_webpage(client: &Client, url: &Url) -> Result<String, reqwest::Error> {
    client.get(url.clone()).send().await?.text().await
}

pub async fn build_graph(
    client: &Client,
    root: Url,
    get_children: impl Fn(&Url, &str, usize) -> Option<HashSet<Url>> + 'static + Clone,
) -> (DiGraph<Url, ()>, HashMap<Url, Result<String, String>>) {
    let nodes = Default::default();
    let edges = Default::default();
    edit_graph(client, root, get_children, &nodes, &edges, 0).await;
    let nodes = nodes.into_inner();
    let edges = edges.into_inner();
    let mut graph = DiGraph::new();
    let mut indices = HashMap::new();
    for (url, _) in &nodes {
        indices.insert(url.clone(), graph.add_node(url.clone()));
    }
    for (from, to) in edges {
        graph.add_edge(indices[&from], indices[&to], ());
    }
    (graph, nodes)
}

#[async_recursion(?Send)]
#[instrument(skip_all, fields(parent))]
async fn edit_graph(
    client: &Client,
    parent: Url,
    get_children: impl Fn(&Url, &str, usize) -> Option<HashSet<Url>> + 'static + Clone,
    nodes: &RwLock<HashMap<Url, Result<String, String>>>,
    edges: &Mutex<HashSet<(Url, Url)>>,
    depth: usize,
) {
    if nodes.read().await.contains_key(&parent) {
        return;
    }
    let res = get_webpage(client, &parent)
        .await
        .map_err(|e| e.to_string());
    {
        let mut write = nodes.write().await;
        match write.contains_key(&parent) {
            true => return,
            false => {
                info!("Add nodes from {parent}");
                write.insert(parent.clone(), res.clone());
                drop(write);

                if let Ok(s) = res {
                    if let Some(children) = get_children(&parent, &s, depth) {
                        info!("Disovered {} children", children.len());
                        let mut write = edges.lock().await;
                        for child in &children {
                            let newly_added = write.insert((parent.clone(), child.clone()));
                            assert!(newly_added, "logic error - created same edge twice");
                        }
                        drop(write);
                        join_all(children.into_iter().map(|new_parent| {
                            edit_graph(
                                client,
                                new_parent,
                                get_children.clone(),
                                nodes,
                                edges,
                                depth + 1,
                            )
                        }))
                        .await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use httptest::{matchers::request, responders::status_code, Expectation, Server};
    use petgraph::graph::DiGraph;
    use soup::{NodeExt, QueryBuilderExt, Soup};
    use url::Url;

    use crate::build_graph;

    const LINK_TO_BAR: &'static str = r#"<a href="/bar">bar</a>"#;
    const LINK_TO_FOO: &'static str = r#"<a href="/foo">foo</a>"#;

    #[tokio::test]
    async fn cyclic() {
        let (graph, pages) = do_test(
            Server::run()
                .serve("/", LINK_TO_FOO)
                .serve("/foo", LINK_TO_BAR)
                .serve("/bar", LINK_TO_FOO),
        )
        .await;
        assert_eq!(graph.node_count(), 3);
        assert_eq!(pages.len(), 3);
    }

    #[tokio::test]
    async fn two_children() {
        let (graph, pages) = do_test(
            Server::run()
                .serve(
                    "/",
                    Box::leak(format!("{}{}", LINK_TO_FOO, LINK_TO_BAR).into_boxed_str()),
                )
                .no_serve("/foo")
                .no_serve("/bar"),
        )
        .await;
        assert_eq!(graph.node_count(), 3);
        assert_eq!(pages.len(), 3);
    }

    #[tokio::test]
    async fn single_grandchild() {
        let (graph, pages) = do_test(
            Server::run()
                .serve("/", LINK_TO_FOO)
                .serve("/foo", LINK_TO_BAR)
                .no_serve("/bar"),
        )
        .await;
        assert_eq!(graph.node_count(), 3);
        assert_eq!(pages.len(), 3);
    }

    #[tokio::test]
    async fn single_child() {
        let (graph, pages) = do_test(Server::run().serve("/", LINK_TO_FOO).no_serve("/foo")).await;
        assert_eq!(graph.node_count(), 2);
        assert_eq!(pages.len(), 2);
    }

    #[tokio::test]
    async fn terminal_node() {
        let (graph, pages) = do_test(Server::run().serve("/", "")).await;
        assert_eq!(graph.node_count(), 1);
        assert_eq!(pages.len(), 1);
    }

    #[tokio::test]
    async fn terminal_node_err() {
        let (graph, pages) = do_test(Server::run().no_serve("/")).await;
        assert_eq!(graph.node_count(), 1);
        assert_eq!(pages.len(), 1);
    }

    async fn do_test(server: Server) -> (DiGraph<Url, ()>, HashMap<Url, Result<String, String>>) {
        build_graph(
            &Default::default(),
            server
                .url("/")
                .to_string()
                .parse()
                .expect("URI isn't a URL"),
            get_all_children,
        )
        .await
    }

    fn get_all_children(url: &Url, body: &str, _depth: usize) -> Option<HashSet<Url>> {
        Some(
            Soup::new(body)
                .tag("a")
                .attr_name("href")
                .find_all()
                .map(|anchor| {
                    let href = anchor.get("href").expect("Already filtered by href");
                    match href.parse::<Url>() {
                        Ok(url) => Ok(url),
                        Err(url::ParseError::RelativeUrlWithoutBase) => url.join(&href),
                        Err(e) => Err(e),
                    }
                })
                .filter_map(Result::ok)
                .collect(),
        )
    }

    trait ServerExt {
        fn serve(self, path: &'static str, body: &'static str) -> Self;
        fn no_serve(self, path: &'static str) -> Self;
    }

    impl ServerExt for Server {
        fn serve(self, path: &'static str, body: &'static str) -> Self {
            self.expect(
                Expectation::matching(request::method_path("GET", path))
                    .respond_with(status_code(200).body(body)),
            );
            self
        }

        fn no_serve(self, path: &'static str) -> Self {
            self.expect(
                Expectation::matching(request::method_path("GET", path))
                    .respond_with(status_code(400)),
            );
            self
        }
    }
}
