---
name: summarize
description: Summarize a file with configurable output length
model: anthropic/claude-sonnet-5
arguments:
  - name: file
    description: Path to the file to summarize
    required: true
  - name: length
    description: "short | medium | long"
    required: false
    default: medium
---
Please summarize {{file}}.

Target length: {{length}}. Lead with the single most important point, then
supporting detail. Preserve any file paths, identifiers, or numbers verbatim.
