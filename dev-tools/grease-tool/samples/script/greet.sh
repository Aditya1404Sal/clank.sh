#!/bin/sh
# A grease *script* package body. On `grease install greet` this is stored at
# /usr/bin/greet and becomes a first-class command in the clank shell.
#
# `{{name}}` is a declared argument (see the README walkthrough): the installer
# turns it into a `--name <value>` flag and substitutes it here before the body
# runs. Anything the shell can execute is fair game — this one just echoes.
echo "Hello, {{name}}! Welcome to clank."
