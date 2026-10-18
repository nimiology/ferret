# Ferret

**fzf for your whole Mac, and your AI agent can use it too.**

Ferret indexes every file name on your disk once, keeps the index live with FSEvents, and answers queries in milliseconds. It ships as:

- `ff`: the CLI and an interactive picker (`ff -i`)
- `ferretd`: a small background daemon that keeps the index in memory and up to date
- `ff mcp`: an MCP server so Claude Code, Cursor and other agents can find files by name and metadata (never contents)
- `Ferret.app`: a menu bar app with a ⌥Space search panel (egui, no webview)

<!-- Demo GIFs: record with vhs (see docs/demo/*.tape), then uncomment. -->
<!-- ![ff in the terminal](docs/demo/ff.gif) -->
<!-- ![Claude Code using ff mcp](docs/demo/mcp.gif) -->

## Install

```sh
# CLI + daemon (ff, ferretd)
brew install nimiology/ferret/ferret

# From source (Rust stable)
git clone https://github.com/nimiology/ferret && cd ferret
cargo install --path crates/ferret        # ff + ferretd
cargo install --path crates/ferret-app    # ferret (menu bar app binary)
```

**Menu bar app:** download `Ferret-<version>-universal.dmg` from the [latest release](https://github.com/nimiology/ferret/releases/latest) (Apple Silicon and Intel). It isn't signed yet, so the first time, right-click Ferret.app → Open.

Release tarballs for Apple Silicon and Intel, with a shell installer, are attached to each [GitHub release](https://github.com/nimiology/ferret/releases).

## Quick start

```sh
ff index                      # first full scan (see benchmarks for timing)
ff daemon install             # launchd agent: keeps the index live
ff invoice 2025 *.pdf         # all words must appear in the name
ff ext:psd size:>100MB        # big Photoshop files
ff -i                         # interactive picker
eval "$(ff init zsh)"         # add to ~/.zshrc: Ctrl-T inserts a path, `fcd` jumps to a folder
```

Without the daemon, `ff` falls back to the last snapshot (`~/Library/Caches/ferret/index.bin`).

## Query syntax

| Token | Meaning | Example |
|---|---|---|
| `word` | name contains word (case-insensitive); all words must match | `ff tax 2025` |
| `"exact phrase"` | name contains the phrase | `ff "final draft"` |
| `!word` | exclude names containing word | `ff report !old` |
| `*.ext` / `ext:a;b` | extension filter | `ff ext:jpg;png` |
| `type:file` / `type:dir` | files or folders only | `ff node_modules type:dir` |
| `size:>N` / `size:<N` | size, units KB MB GB | `ff size:>1GB` |
| `mod:<7d` / `mod:>1y` | modified within / older than (s m h d w y) | `ff mod:<2d *.md` |
| `in:path` | limit to a folder | `ff in:~/Projects Cargo.toml` |

Flags: `--limit N` (default 50), `--sort score|size|mtime`.

## AI agents (MCP)

Claude Code:

```sh
claude mcp add --scope user ferret -- ff mcp
```

Cursor (`~/.cursor/mcp.json`):

```json
{
  "mcpServers": {
    "ferret": { "command": "ff", "args": ["mcp"] }
  }
}
```

Tools: `find_files` (query + sort by relevance, size or newest) and `file_info`.

Measured in Claude Code, "find the business plan PDF I saved recently", 5 runs, warm cache:

| | Shell tools (find / mdfind) | Ferret MCP |
|---|---|---|
| Cost per search | $0.096 | $0.016 |
| Time | 81.5 s | 5.3 s |
| Turns | 5.2 | 2 |

### Guardrails

- **Names and metadata only.** Ferret never reads file contents; it returns paths, sizes and dates.
- **At most 100 results** per call.
- **Allow / deny list** in `~/.config/ferret/config.toml`. Allow defaults to your home folder:

  ```toml
  [mcp]
  allow = ["~/Projects", "~/Documents"]
  deny  = ["~/Documents/Private"]
  ```

- **Default denies, always on** (cannot be allowed back): `~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.kube`, `~/.docker/config.json`, `~/.netrc`, `~/.config/gh`, `~/Library/Keychains`, `~/Library/Messages`, `~/Library/Mail`, `~/Library/Cookies`, `~/Library/Safari`, and Chrome, Firefox, Brave and Arc profile folders.
- **Every call is logged** to `~/Library/Logs/ferret/mcp.log`.

## Full Disk Access

macOS hides some folders (Mail, Messages, Safari, other apps' containers) from processes without Full Disk Access. Without it Ferret still works, but those folders are missing from the index. To grant it: System Settings → Privacy & Security → Full Disk Access → add `ferretd` (and your terminal if you run `ff index` directly). `ff doctor` reports what is missing.

## GUI

`ferret` opens a search window: type to search, ↑/↓ to move, Enter opens, Cmd-Enter reveals in Finder, Esc clears. It shares the same index as `ff` (daemon first, snapshot fallback).

## Benchmarks

Measured on the author's Mac (Apple Silicon, internal SSD):

| What | Result |
|---|---|
| Full scan of `/` (~4.1M entries, `getattrlistbulk`, 8 threads) | **87–96 s** (was 158–215 s with readdir + stat) |
| Peak memory during the scan | 85–89 MB (was ~400 MB) |
| Index in memory | ~298 MB |
| Snapshot on disk | 286 MB |
| Restart from snapshot | ~0.65 s until searchable |
| Substring search, 2M-entry synthetic index (criterion) | 17–18 ms |
| Daemon idle CPU | ~0.4% |

**Known gap:** the first full scan still takes ~90 s, short of the 30 s goal; almost all of it is kernel time listing ~880K folders. After that, FSEvents keeps the index current and restarts load the snapshot, so this is a one-time cost.

### vs `mdfind -name` and `find`

Same name query, daemon running, measured with `hyperfine` (5 runs; `find` timed once):

| Command | Time | Matches | Searches |
|---|---|---|---|
| `ff invoice` | 37 ms ± 10 ms | 145 | the whole disk, including hidden and system folders |
| `mdfind -name invoice` | 31 ms ± 2 ms | 70 | only what Spotlight indexes (skips hidden folders, `node_modules`, `.git`, many caches) |
| `find ~ -iname '*invoice*'` | 99 s | | your home folder only |

`ff` is as fast as Spotlight while seeing everything on disk, and about 2,700× faster than `find`.

## License

MIT, see [LICENSE](LICENSE).
