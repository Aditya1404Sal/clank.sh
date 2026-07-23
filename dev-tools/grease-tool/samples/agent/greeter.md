# Sample agent package — `greeter`

An **agent** package points the registry at a deployed Golem agent type so
`grease install` can wire up wRPC calls to it. Unlike prompt/script/skill, the
tool reads **no file** for an agent — you answer a short series of prompts. This
sample mirrors the repo's own `greeter-agent` fixture (`GreeterAgent`).

Choose `agent` at the menu, then enter:

| Prompt | Value |
|---|---|
| `name` | `greeter` |
| `description` | `Greets a person by name` |
| `agent type` | `GreeterAgent` |
| `constructor params (comma-separated names)` | `name` |
| `add a method?` → `method name` | `greet` |
| &nbsp;&nbsp;`method description` | `Greet someone` |
| &nbsp;&nbsp;`method params (comma-separated names)` | `who` |
| `add a method?` (again) | `no` |
| `ephemeral (stateless) agent?` | `no` |

The tool writes `packages/greeter.json` (kind `agent`) plus a signed,
content-hashed, transparency-logged entry in `index.json`. After
`grease install greeter`, the agent's methods are reachable from the clank
shell — see the grease agent surface (`--trigger` / `--schedule` / oplog /
`status` / `kill`).
