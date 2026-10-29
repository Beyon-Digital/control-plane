//! MCP stdio bridge. All desktop actions use the same host leases as human control.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let base = std::env::var("CONTROL_PLANE_URL").context("CONTROL_PLANE_URL required")?;
    let url = reqwest::Url::parse(&base)?;
    if url.scheme() != "https"
        && !(url.scheme() == "http"
            && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")))
    {
        bail!("HTTPS required outside loopback");
    }
    let token = std::env::var("CONTROL_PLANE_DEVICE_TOKEN")
        .context("CONTROL_PLANE_DEVICE_TOKEN required")?;
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("x-device-token", token.parse()?);
    let mut builder = reqwest::Client::builder()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(20));
    if let Ok(cert) = std::env::var("CONTROL_PLANE_CLIENT_CERT_PEM") {
        let key =
            std::env::var("CONTROL_PLANE_CLIENT_KEY_PEM").context("client private key required")?;
        if cert.len() > 65536 || key.len() > 16384 {
            bail!("oversized client identity");
        }
        builder = builder.identity(reqwest::Identity::from_pem(
            format!("{cert}\n{key}").as_bytes(),
        )?);
    }
    if let Ok(ca) = std::env::var("CONTROL_PLANE_SERVER_CA_PEM") {
        if ca.len() > 65536 {
            bail!("oversized server CA");
        }
        builder = builder.add_root_certificate(reqwest::Certificate::from_pem(ca.as_bytes())?);
    }
    let client = builder.build()?;
    let mut input = BufReader::new(tokio::io::stdin());
    let mut output = tokio::io::stdout();
    loop {
        let mut buf = Vec::new();
        let n = input.read_until(b'\n', &mut buf).await?;
        if n == 0 {
            break;
        }
        if n > 1024 * 1024 {
            bail!("MCP input exceeds 1 MiB");
        }
        let request: Value = match serde_json::from_slice(&buf) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(id) = request.get("id") else {
            continue;
        };
        let response = match request["method"].as_str().unwrap_or("") {
            "initialize" => Ok(
                json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"control-plane-computer","version":env!("CARGO_PKG_VERSION")}}),
            ),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools":[
                {"name":"computer_status","description":"Check availability and OS permissions for the remote desktop.","inputSchema":{"type":"object","properties":{}}},
                {"name":"computer_accessibility","description":"Inspect the foreground application's accessibility tree where the host supports it.","inputSchema":{"type":"object","properties":{}}},
                {"name":"computer_screenshot","description":"Capture the desktop and receive a geometry version. Required before acting. Human control takes priority.","inputSchema":{"type":"object","properties":{}}},
                {"name":"computer_action","description":"Act on a recent desktop screenshot. Types: click {x,y}, text {text}, key {key}, scroll {delta_x,delta_y}. Coordinate version must match screenshot.","inputSchema":{"type":"object","properties":{"geometry_version":{"type":"integer"},"action":{"type":"object"}},"required":["geometry_version","action"]}}
            ]})),
            "tools/call" => match call(&client, &base, &request["params"]).await {
                Ok(v) => Ok(v),
                Err(e) => {
                    Ok(json!({"isError":true,"content":[{"type":"text","text":e.to_string()}]}))
                }
            },
            _ => Err(json!({"code":-32601,"message":"Method not found"})),
        };
        let response = match response {
            Ok(v) => json!({"jsonrpc":"2.0","id":id,"result":v}),
            Err(e) => json!({"jsonrpc":"2.0","id":id,"error":e}),
        };
        output
            .write_all(format!("{}\n", response).as_bytes())
            .await?;
        output.flush().await?;
    }
    Ok(())
}
async fn request(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    body: Option<Value>,
) -> Result<Value> {
    let url = format!("{}{path}", base.trim_end_matches('/'));
    let response = if let Some(body) = body {
        client.post(url).json(&body).send().await?
    } else {
        client.get(url).send().await?
    };
    let status = response.status();
    let value: Value = response.json().await?;
    if !status.is_success() {
        bail!(
            "{}",
            value["error"].as_str().unwrap_or("host request failed")
        );
    }
    Ok(value)
}
async fn call(client: &reqwest::Client, base: &str, params: &Value) -> Result<Value> {
    let value = match params["name"].as_str().unwrap_or("") {
        "computer_status" => request(client, base, "/v1/desktop/status", None).await?,
        "computer_accessibility" => {
            request(client, base, "/v1/desktop/accessibility", None).await?
        }
        "computer_screenshot" => {
            let frame = request(client, base, "/v1/desktop/frame", None).await?;
            return Ok(
                json!({"content":[{"type":"image","data":frame["image_base64"],"mimeType":frame["mime_type"]},{"type":"text","text":json!({"geometry_version":frame["geometry_version"],"width":frame["width"],"height":frame["height"]}).to_string()}]}),
            );
        }
        "computer_action" => {
            let lease = request(
                client,
                base,
                "/v1/desktop/lease",
                Some(json!({"controller_id":"mcp"})),
            )
            .await?;
            request(client,base,"/v1/desktop/action",Some(json!({"controller_id":"mcp","lease_id":lease["lease_id"],"geometry_version":params["arguments"]["geometry_version"],"action":params["arguments"]["action"]}))).await?
        }
        _ => bail!("Unknown computer tool"),
    };
    Ok(json!({"content":[{"type":"text","text":value.to_string()}]}))
}
