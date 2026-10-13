use veil::prelude::*;

#[veil::route(path = "/hello/{name}", method = "GET")]
async fn hello(name: String) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "hello": name }))
}

#[veil::main]
async fn main() -> Result<(), veil::api::Error> {
    veil::serve(Config::load()?).await // plaintext HTTP/1.1 + h2c on 127.0.0.1:8080, OpenAPI at /docs
}
