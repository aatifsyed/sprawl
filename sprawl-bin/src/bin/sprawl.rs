use anyhow::Context;
use clap::Parser;
use regex::Regex;
use reqwest::Client;
use soup::{NodeExt, QueryBuilderExt, Soup};
use tracing::info;
use url::Url;

#[derive(Parser)]
#[clap(name = "sprawl")]
struct Args {
    #[clap(short, long)]
    url: Url,
    #[clap(short, long, default_value = "10")]
    depth: usize,
    #[clap(short, long)]
    regex: Option<Regex>,
    #[clap(short, long)]
    limit_children: Option<usize>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    let client = Client::builder()
        // example.com requires this header
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION"),
        ))
        .build()
        .context("Couldn't construct client")?;
    let (graph, _) = sprawl::build_graph(&client, args.url, move |url, body, depth| {
        if depth >= args.depth {
            return None;
        }
        let soup = Soup::new(body);
        let children = soup
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
            .filter(|url| {
                matches!(
                    args.regex.as_ref().map(|re| re.is_match(url.as_str())),
                    Some(true)
                )
            })
            .map(|mut url| {
                url.set_fragment(None);
                url
            });
        match args.limit_children {
            Some(limit) => Some(children.take(limit).collect()),
            None => Some(children.collect()),
        }
    })
    .await;
    let graph = graph.map(|_, n| n.to_string(), |_, _| ());
    println!("{:?}", petgraph::dot::Dot::new(&graph));
    info!("Graph has {} nodes", graph.raw_nodes().len());
    Ok(())
}
