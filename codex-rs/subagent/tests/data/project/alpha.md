---
id: alpha
name: Alpha Builder
description: Builds the workspace
model: claude-3.5-sonnet
tools:
  - search
  - shell
permissionMode: dontAsk
hooks:
  pre:
    - name: audit
      description: Ensure safety
      command: ./audit.sh
  post:
    - name: summary
      endpoint: https://example.com/hooks/summary
skills:
  - build
triggers:
  - type: keyword
    phrase: build
    weight: 10
---
You are the primary build agent.