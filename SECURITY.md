# Security

## Reporting a vulnerability

Please report security issues privately through [GitHub's private vulnerability reporting](https://github.com/nimiology/ferret/security/advisories/new), not in a public issue. I'll reply within a few days.

## What counts

Ferret indexes file names and metadata on your Mac. The security boundaries are:

- **The MCP server (`ff mcp`)**: AI agents must never see paths outside the allowed roots or inside denied folders (`~/.ssh`, `~/Library/Mail`, browser profiles, …), and must never receive file contents. Any way to get around the allow/deny list (symlinks, `..`, letter case, Unicode forms, query syntax) is a vulnerability.
- **The daemon socket**: `~/Library/Application Support/ferret/ferret.sock` must only be usable by your own user.
- **Local data**: the index snapshot (`~/Library/Caches/ferret/index.bin`) never leaves your Mac.
