use openrpsee::openrpc::{Generator, Info, OpenRpc};

#[allow(unused)]
pub mod output;

pub mod methods;

fn main() {
    let mut generator = Generator::new();
    let mut methods = Vec::new();

    for (tag, item) in output::API_METHODS.iter() {
        for (name, method) in item.entries() {
            methods.push(method.generate_with_tag(&mut generator, name, Some(tag)));
        }
    }

    let doc = OpenRpc {
        openrpc: "1.3.2",
        info: Info {
            title: "Fiber Network RPC",
            description: "RPC interface for Fiber Network",
            version: "0.8.0",
        },
        methods,
        components: generator.into_components(),
    };

    let json = serde_json::to_string_pretty(&doc).expect("failed to serialize OpenRPC document");
    println!("{}", json);
}
