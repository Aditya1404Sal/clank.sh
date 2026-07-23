# main-rc-1 — prerequisites (servers & registry)

Background services the demo/testing paths depend on. Start these first, in their own terminals, and
leave them running. Branch `main-rc-1`, worktree `~/Desktop/clank-spike`.

The native path ([NATIVE_RC_TESTING.md](NATIVE_RC_TESTING.md)) needs **only the grease registry**, and
only if you're demoing `grease install`. The agent path ([RC_TESTING.md](RC_TESTING.md)) needs the
golem cluster; add the registry for grease.

---

## Standing env (every terminal)

```bash
export GOLEM=~/Desktop/clank.sh/golem-stuff/golem/target/debug/golem
export GOLEM_BUILTIN_LOCAL_URL=http://localhost:9891
```

---

## Grease registry (signed + transparency-logged)

Generate the fixture registry, then serve it over HTTP.

**Generate** — writes the packages and prints the public key on the last line:

```bash
cd ~/Desktop/clank-spike
cargo run -q --example grease-fixture -- /tmp/clank-rc1-registry
```

The fixture is **deterministic** (fixed `[7u8; 32]` signing seed), so the public key is always:

```
6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=
```

Contents: `hello` [prompt] · `greeting` [prompt] · `hostinfo` [script] · `reviewing` [skill] ·
`greeter` [agent].

**Serve** (leave running):

```bash
cd /tmp/clank-rc1-registry && python3 -m http.server 8823
```

The registry is now at `http://localhost:8823`. In a clank shell (native or agent), point grease at it
and trust the key:

```sh
grease registry add http://localhost:8823 --key 6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=
grease install hello
```

---

## Golem dev cluster (agent path only)

```bash
cd ~/Desktop/clank-spike
$GOLEM server run --router-port 9891 --custom-request-port 9016 --mcp-port 9017 --data-dir /tmp/clank-rc1-devdata
```

> **Fresh data dir on purpose.** `/tmp/clank-rc1-devdata` is distinct from the released-demo's
> `/tmp/clank-demo-devdata`, so this cluster never inherits a stale component revision from the 1.5.x
> line. Then build & deploy per [RC_TESTING.md](RC_TESTING.md) Terminal B.
