// SPDX-License-Identifier: LGPL-2.1-or-later

use anyhow::Context;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, Implementation, ListToolsResult,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::FileTypeExt;
use std::sync::Arc;
use tokio::sync::Mutex;
use zlink::varlink_service::Proxy;

/// Error reply from a dynamic varlink call.
#[derive(Debug, Deserialize)]
struct DynReplyError<'e> {
    error: &'e str,
    #[serde(default)]
    parameters: Option<HashMap<&'e str, Value>>,
}

/// Method call with dynamic method name and parameters.
#[derive(Debug, Serialize)]
struct DynMethod<'m> {
    method: &'m str,
    parameters: Option<Value>,
}

/// Discovered varlink tool: a single method on a single interface/socket.
struct VarlinkTool {
    /// MCP tool definition (name, description, input_schema)
    tool: Tool,
    /// Varlink socket name (e.g. "org.freedesktop.systemd1")
    socket: String,
    /// Full varlink method name (e.g. "org.freedesktop.systemd1.Manager.ListUnits")
    varlink_method: String,
}

struct SystemdMcp {
    socket_dir: OwnedFd,
    tools: Vec<VarlinkTool>,
    /// Cached varlink connections, keyed by socket name.
    conns: Arc<Mutex<HashMap<String, Arc<Mutex<zlink::unix::Connection>>>>>,
}

fn type_to_schema(
    ty: &zlink::idl::Type,
    custom_types: &HashMap<&str, &zlink::idl::CustomType>,
) -> Value {
    match ty {
        zlink::idl::Type::Bool => json!({"type": "boolean"}),
        zlink::idl::Type::Int => json!({"type": "integer"}),
        zlink::idl::Type::Float => json!({"type": "number"}),
        zlink::idl::Type::String => json!({"type": "string"}),
        zlink::idl::Type::ForeignObject | zlink::idl::Type::Any => json!({"type": "object"}),
        zlink::idl::Type::Custom(name) => {
            if let Some(ct) = custom_types.get(name) {
                if let Some(obj) = ct.as_object() {
                    let mut schema = fields_to_schema(obj.fields(), custom_types);
                    if let Value::Object(ref mut map) = schema {
                        map.insert("title".to_string(), json!(name.to_string()));
                    }
                    schema
                } else if let Some(en) = ct.as_enum() {
                    let names: Vec<&str> = en.variants().map(|v| v.name()).collect();
                    json!({"type": "string", "enum": names, "title": name.to_string()})
                } else {
                    json!({"type": "object", "title": name.to_string()})
                }
            } else {
                json!({"type": "object", "title": name.to_string()})
            }
        }
        zlink::idl::Type::Optional(inner) => type_to_schema(inner.inner(), custom_types),
        zlink::idl::Type::Array(inner) => {
            json!({"type": "array", "items": type_to_schema(inner.inner(), custom_types)})
        }
        zlink::idl::Type::Map(inner) => {
            json!({"type": "object", "additionalProperties": type_to_schema(inner.inner(), custom_types)})
        }
        zlink::idl::Type::Object(fields) => fields_to_schema(fields.iter(), custom_types),
        zlink::idl::Type::Enum(variants) => {
            let names: Vec<&str> = variants.iter().map(|v| v.name()).collect();
            json!({"type": "string", "enum": names})
        }
    }
}

fn fields_to_schema<'a>(
    fields: impl Iterator<Item = &'a zlink::idl::Field<'a>>,
    custom_types: &HashMap<&str, &zlink::idl::CustomType>,
) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();

    for field in fields {
        let mut schema = type_to_schema(field.ty(), custom_types);
        if let Some(desc) = comments_to_string(field.comments()) {
            if let Value::Object(ref mut map) = schema {
                map.insert("description".to_string(), json!(desc));
            }
        }
        properties.insert(field.name().to_string(), schema);
        if !matches!(field.ty(), zlink::idl::Type::Optional(_)) {
            required.push(json!(field.name()));
        }
    }

    let mut schema = serde_json::Map::new();
    schema.insert("type".to_string(), json!("object"));
    schema.insert("properties".to_string(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".to_string(), Value::Array(required));
    }
    Value::Object(schema)
}

fn comments_to_string<'a>(
    comments: impl Iterator<Item = &'a zlink::idl::Comment<'a>>,
) -> Option<String> {
    let parts: Vec<&str> = comments.map(|c| c.content()).collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

impl SystemdMcp {
    async fn new(socket_dir_path: &str) -> anyhow::Result<Self> {
        let dir_file = std::fs::File::open(socket_dir_path)
            .with_context(|| format!("failed to open {socket_dir_path}"))?;
        let dirfd = OwnedFd::from(dir_file);

        let mut handler = SystemdMcp {
            socket_dir: dirfd,
            tools: Vec::new(),
            conns: Arc::new(Mutex::new(HashMap::new())),
        };
        handler.discover_tools().await?;
        Ok(handler)
    }

    fn socket_path(&self, socket_name: &str) -> String {
        format!("/proc/self/fd/{}/{socket_name}", self.socket_dir.as_raw_fd())
    }

    async fn get_connection(
        &self,
        socket: &str,
    ) -> anyhow::Result<Arc<Mutex<zlink::unix::Connection>>> {
        let mut cache = self.conns.lock().await;
        if let Some(conn) = cache.get(socket) {
            return Ok(conn.clone());
        }
        let path = self.socket_path(socket);
        let connection = Arc::new(Mutex::new(zlink::unix::connect(&path).await?));
        cache.insert(socket.to_string(), connection.clone());
        Ok(connection)
    }

    async fn discover_tools(&mut self) -> anyhow::Result<()> {
        let dir_path = format!("/proc/self/fd/{}", self.socket_dir.as_raw_fd());
        let mut entries = tokio::fs::read_dir(&dir_path).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let Ok(metadata) = tokio::fs::metadata(&path).await else {
                continue;
            };
            if !metadata.file_type().is_socket() {
                continue;
            }
            let Some(socket_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };

            if let Err(e) = self.discover_socket_tools(socket_name).await {
                eprintln!("warning: skipping socket {socket_name}: {e}");
            }
        }

        eprintln!("Discovered {} tools from varlink sockets", self.tools.len());
        Ok(())
    }

    async fn discover_socket_tools(&mut self, socket_name: &str) -> anyhow::Result<()> {
        let conn_arc = self.get_connection(socket_name).await?;
        let mut conn = conn_arc.lock().await;

        let info = conn
            .get_info()
            .await?
            .map_err(|e| anyhow::anyhow!("GetInfo error: {e}"))?;

        // Collect interface names to avoid holding the borrow across the loop.
        let iface_names: Vec<String> = info.interfaces.iter().map(|s| s.to_string()).collect();

        for iface_name in &iface_names {
            if iface_name == "org.varlink.service" {
                continue;
            }
            // Only register interfaces whose name matches the socket name.
            // Many sockets expose extra interfaces (e.g. io.systemd.Manager
            // also serves io.systemd.UserDatabase), but the canonical socket
            // for an interface is the one named after it.  This avoids
            // duplicate tools and ensures the correct socket is used.
            if iface_name != socket_name {
                continue;
            }
            let description = conn
                .get_interface_description(iface_name)
                .await?
                .map_err(|e| anyhow::anyhow!("GetInterfaceDescription error: {e}"))?;

            let iface: zlink::idl::Interface = description
                .parse()
                .map_err(|e| anyhow::anyhow!("IDL parse error for {iface_name}: {e}"))?;

            let iface_desc = comments_to_string(iface.comments());

            let custom_types: HashMap<&str, &zlink::idl::CustomType> = iface
                .custom_types()
                .map(|ct| (ct.name(), ct))
                .collect();

            for method in iface.methods() {
                let full_method = format!("{}.{}", iface.name(), method.name());
                let input_schema = fields_to_schema(method.inputs(), &custom_types);

                let description = {
                    let method_desc = comments_to_string(method.comments());
                    let mut desc = format!("Varlink method on socket '{socket_name}'.");
                    if let Some(i) = &iface_desc {
                        desc.push_str(&format!("\n\nInterface: {i}"));
                    }
                    if let Some(m) = &method_desc {
                        desc.push_str(&format!("\n\n{m}"));
                    }
                    desc
                };

                let schema_obj: serde_json::Map<String, Value> = match input_schema {
                    Value::Object(map) => map,
                    _ => serde_json::Map::new(),
                };

                let tool = Tool::new(
                    full_method.clone(),
                    description,
                    Arc::new(schema_obj),
                );

                self.tools.push(VarlinkTool {
                    tool,
                    socket: socket_name.to_string(),
                    varlink_method: full_method,
                });
            }
        }

        Ok(())
    }
}

impl ServerHandler for SystemdMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("systemd-mcp", env!("CARGO_PKG_VERSION")),
            )
            .with_instructions(
                "This server exposes systemd varlink interfaces as MCP tools. \
                 Each tool corresponds to a varlink method. Call them with the \
                 appropriate JSON parameters.",
            )
    }

    fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<rmcp::service::RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        let tools: Vec<Tool> = self.tools.iter().map(|t| t.tool.clone()).collect();
        std::future::ready(Ok(ListToolsResult {
            tools,
            next_cursor: None,
            meta: None,
        }))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<rmcp::service::RoleServer>,
    ) -> impl Future<Output = Result<CallToolResult, McpError>> + Send + '_ {
        async move {
            let tool_name = request.name.as_ref();
            let vt = self.tools.iter().find(|t| t.varlink_method == tool_name);
            let vt = match vt {
                Some(t) => t,
                None => {
                    return Err(McpError::invalid_params(
                        format!("unknown tool: {tool_name}"),
                        None,
                    ))
                }
            };

            let conn_arc = self.get_connection(&vt.socket).await.map_err(|e| {
                McpError::internal_error(format!("connection error: {e}"), None)
            })?;
            let mut conn = conn_arc.lock().await;

            let args = Value::Object(request.arguments.unwrap_or_default());
            let method_call = DynMethod {
                method: &vt.varlink_method,
                parameters: Some(args),
            };

            // Always use `more: true` so streaming methods return all
            // results.  Non-streaming methods simply return one reply
            // with `continues: false`.
            conn.send_call(&zlink::Call::new(&method_call).set_more(true), vec![])
                .await
                .map_err(|e| McpError::internal_error(format!("varlink send error: {e}"), None))?;

            let mut results: Vec<Value> = Vec::new();
            loop {
                let (reply, _fds) = conn
                    .receive_reply::<Value, DynReplyError>()
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("varlink error: {e}"), None)
                    })?;

                match reply {
                    Ok(r) => {
                        let continues = r.continues().unwrap_or(false);
                        if let Some(params) = r.into_parameters() {
                            results.push(params);
                        }
                        if !continues {
                            break;
                        }
                    }
                    Err(e) => {
                        let msg = match e.parameters {
                            Some(params) => format!("{}: {params:?}", e.error),
                            None => e.error.to_string(),
                        };
                        return Ok(CallToolResult::error(vec![Content::text(msg)]));
                    }
                }
            }

            // Return a single result for one reply, an array for multiple.
            let output = if results.len() == 1 {
                serde_json::to_string_pretty(&results.into_iter().next().unwrap())
            } else {
                serde_json::to_string_pretty(&results)
            }
            .unwrap_or_default();
            Ok(CallToolResult::success(vec![Content::text(output)]))
        }
    }
}

use std::future::Future;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing_subscriber::filter::LevelFilter::INFO.into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let socket_dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/run/varlink/registry".to_string());

    let handler = SystemdMcp::new(&socket_dir).await?;

    let service = handler
        .serve(rmcp::transport::io::stdio())
        .await
        .map_err(|e| anyhow::anyhow!("MCP server error: {e}"))?;

    service.waiting().await?;
    Ok(())
}
