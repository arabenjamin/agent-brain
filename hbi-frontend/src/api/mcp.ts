/**
 * MCP client singleton.
 *
 * Uses StreamableHTTPClientTransport (Streamable HTTP — the transport the
 * brain actually implements) rather than the legacy SSEClientTransport.
 */
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { API_KEY, MCP_URL } from "./config";

let _client: Client | null = null;
let _connecting: Promise<Client> | null = null;

export async function getMcpClient(): Promise<Client> {
  if (_client) return _client;
  if (_connecting) return _connecting;

  _connecting = (async () => {
    // Resolve relative paths (e.g. "/mcp") against the current origin.
    const transport = new StreamableHTTPClientTransport(
      new URL(MCP_URL, window.location.href),
      {
        requestInit: {
          headers: { Authorization: `Bearer ${API_KEY}` },
        },
      }
    );

    const client = new Client(
      { name: "hbi-frontend", version: "1.0.0" },
      { capabilities: {} }
    );

    await client.connect(transport);
    _client = client;
    _connecting = null;
    return client;
  })();

  return _connecting;
}

/** Call an MCP tool, returning the parsed text content. */
export async function callTool(
  name: string,
  args: Record<string, unknown> = {}
): Promise<string> {
  const client = await getMcpClient();
  const result = await client.callTool({ name, arguments: args });

  const content = result.content as Array<{ type: string; text?: string }>;
  return content
    .filter((c) => c.type === "text")
    .map((c) => c.text ?? "")
    .join("\n");
}

/** Reset the singleton (called on disconnect). */
export function resetMcpClient() {
  _client = null;
  _connecting = null;
}
