/**
 * MCP client singleton.
 *
 * Uses StreamableHTTPClientTransport (Streamable HTTP — the transport the
 * brain actually implements) rather than the legacy SSEClientTransport.
 */
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { getMcpUrl, getApiKey } from "./config";

let _client: Client | null = null;
let _connecting: Promise<Client> | null = null;

export async function getMcpClient(): Promise<Client> {
  if (_client) return _client;
  if (_connecting) return _connecting;

  _connecting = (async () => {
    // Resolve relative paths (e.g. "/mcp") against the current origin.
    // Read URL and key fresh each connection so Settings changes take effect.
    const transport = new StreamableHTTPClientTransport(
      new URL(getMcpUrl(), window.location.href),
      {
        requestInit: {
          headers: { Authorization: `Bearer ${getApiKey()}` },
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
  try {
    const client = await getMcpClient();
    const result = await client.callTool({ name, arguments: args });

    const content = result.content as Array<{ type: string; text?: string }>;
    return content
      .filter((c) => c.type === "text")
      .map((c) => c.text ?? "")
      .join("\n");
  } catch (e) {
    console.warn(`MCP tool call failed (${name}), attempting reconnect:`, e);
    // Attempt reconnect once on transport error
    resetMcpClient();
    const client = await getMcpClient();
    const result = await client.callTool({ name, arguments: args });

    const content = result.content as Array<{ type: string; text?: string }>;
    return content
      .filter((c) => c.type === "text")
      .map((c) => c.text ?? "")
      .join("\n");
  }
}

/** Reset the singleton (called on disconnect or after settings change). */
export function resetMcpClient() {
  _client = null;
  _connecting = null;
}
