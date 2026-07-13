# To Run all tests with llm capabilities (ask, mcp) :

```sh
scripts/golem-e2e.sh --takeover --with-llm --with-grease --with-mcp
```

> Note : You will require ANTHROPIC_API_KEY set (exported) in your terminal

You won't require the key to run the script with `--with-grease` and `--with-mcp` (excluding llm tool use)

# For Experiencing clank.sh as a terminal but still running it as a golem agent :

```sh
scripts/clank-repl.sh --deploy
```

run it with `--takeover` in case you've got other golem servers running about :)
 > enter `exit` to .... well, exit?