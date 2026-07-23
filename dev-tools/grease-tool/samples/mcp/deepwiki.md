# Sample mcp package — `deepwiki`

An **mcp** package records a remote MCP server. On `grease install deepwiki` the
agent connects, enumerates the server's tools/prompts/resources, and exposes
them: tools as `deepwiki <tool>` commands, prompts on `$PATH`, and resources
under the `/mnt/mcp` virtual filesystem. Like agent, the tool reads **no file** —
you answer a couple of prompts.

Choose `mcp` at the menu, then enter:

| Prompt | Value |
|---|---|
| `name` | `deepwiki` |
| `description` | `Ask questions about public GitHub repositories` |
| `MCP server URL (https://…)` | `https://mcp.deepwiki.com/mcp` |
| `auth env var (blank = none)` | *(leave blank — deepwiki is public)* |

For a server that needs a token, name the environment variable that holds it
(e.g. `GITHUB_TOKEN`) at the last prompt; the agent reads it at connect time
rather than storing the secret in the registry.

The tool writes a minimal `packages/deepwiki.json` (kind `mcp`) — the
tools/prompts/resources cache fields are left empty on purpose and enriched by
the agent at install time.
