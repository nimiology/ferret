<div align="center">

<img src="docs/assets/icon.png" width="128" alt="Ferret icon: a ferret whose head is a magnifying lens">

# Ferret

**fzf for your whole Mac, and your AI agent can use it too.**

Ferret remembers the name of every file on your disk and finds any of them in milliseconds,
from your terminal, your menu bar, or your AI coding agent.

[![CI](https://github.com/nimiology/ferret/actions/workflows/ci.yml/badge.svg)](https://github.com/nimiology/ferret/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/nimiology/ferret?color=ff9259)](https://github.com/nimiology/ferret/releases/latest)
![macOS](https://img.shields.io/badge/macOS-Apple%20Silicon%20%7C%20Intel-1c1b19)
![Rust](https://img.shields.io/badge/made%20with-Rust-c4501b)
[![License: MIT](https://img.shields.io/badge/license-MIT-6e6b65)](LICENSE)

<img src="docs/demo/ff.gif" width="820" alt="ff finding invoices, the biggest files and a Cargo.toml in milliseconds, then the interactive picker">

</div>

---

## Why Ferret?

You know the file exists. You just don't remember where you put it. 🐾

Spotlight skips hidden folders and half your dev tree. `find` takes a minute and a half. Ferret walks your whole disk **once**, keeps the index fresh as files change, and then answers in the time it takes to lift your finger off the key:

- ⚡ **Instant**: a search over 4 million files comes back in 20–40 ms.
- 🔭 **Everything**: hidden folders, `node_modules`, `.git`, system folders, all of it.
- 🔄 **Always fresh**: a tiny daemon follows macOS file events, so new files show up within a second.
- 🤖 **Agent-ready**: Claude Code, Cursor and friends can search your disk through MCP, never seeing file contents.
- 🪶 **Light**: idle CPU around 0.4%. No Electron, no webview, no telemetry. Your index never leaves your Mac.

## What's in the box

| | What it is |
|---|---|
| **`ff`** | The command line: search, a live interactive picker (`ff -i`), and shell widgets (Ctrl-T, `fcd`) |
| **`ferretd`** | A small background daemon that keeps the index in memory and up to date |
| **`ff mcp`** | An MCP server so AI agents can find files by name and metadata |
| **Ferret.app** | A menu bar app: press ⌥Space anywhere, type, hit Enter |

## Install

**CLI + daemon** with Homebrew:

```sh
brew install nimiology/ferret/ferret
```

**Menu bar app:** grab `Ferret-<version>-universal.dmg` from the [latest release](https://github.com/nimiology/ferret/releases/latest) (Apple Silicon and Intel). It isn't notarized yet, so the first time, right-click Ferret.app → **Open**.

<details>
<summary>From source</summary>

```sh
git clone https://github.com/nimiology/ferret && cd ferret
cargo install --path crates/ferret        # ff + ferretd
cargo install --path crates/ferret-app    # the menu bar app binary
```

Release tarballs and a shell installer are also attached to every [GitHub release](https://github.com/nimiology/ferret/releases).
</details>

## Quick start

```sh
ff index                        # first full scan, about a minute and a half
ff daemon install               # keep the index live in the background
ff invoice 2025 ext:pdf         # every word must appear in the name
ff big in:~/Downloads           # biggest files first
ff -i                           # interactive picker
eval "$(ff init zsh)"           # in ~/.zshrc: Ctrl-T inserts a path, `fcd` jumps to a folder
```

Not sure everything is set up? `ff doctor` checks the index, the daemon and Full Disk Access, and tells you exactly what to fix.

## Searching

| Type | Finds | Example |
|---|---|---|
| `word` | names containing it, ignoring case; every word must match | `ff tax 2025` |
| `"exact phrase"` | names containing the phrase | `ff "final draft"` |
| `!word` | leaves out names containing it | `ff report !old` |
| `*.ext` / `ext:a;b` | those extensions | `ff ext:jpg;png` |
| `type:file` / `type:dir` | only files or only folders | `ff node_modules type:dir` |
| `size:>N` / `size:<N` | by size, units KB MB GB | `ff size:>1GB` |
| `mod:<7d` / `mod:>1y` | changed within / older than (s m h d w y) | `ff 'mod:<2d' ext:md` |
| `in:folder` | only under that folder | `ff in:~/Projects Cargo.toml` |

Flags: `--limit N`, `--sort score|size|mtime`, `--json` and `-0` for scripts. `ff big` and `ff recent` are shortcuts for the two questions everyone asks.

**In the picker** (`ff -i`): ↑/↓ move · Enter prints the path · Tab cycles All / Files / Folders / Recent · Ctrl-O open · Ctrl-R reveal in Finder · Ctrl-Y copy · Ctrl-L Quick Look · Esc quit.

## The menu bar app

Press **⌥Space** from anywhere and a search panel drops in. Type to search, switch between **All · Files · Folders · Recent · Large** with Tab, **Enter** opens, **⌘Enter** reveals in Finder, **⌘Y** opens Quick Look, **⌘C** copies the path. The panel follows your light or dark theme, and you can change the hotkey in Settings.

## Let your AI agent find files 🤖

```sh
claude mcp add --scope user ferret -- ff mcp
```

<details>
<summary>Cursor and other MCP clients</summary>

```json
{
  "mcpServers": {
    "ferret": { "command": "ff", "args": ["mcp"] }
  }
}
```
</details>

Now "find the contract PDF I downloaded last week" just works. Ferret gives agents one tool, `find_files` (sorted by relevance, size or newest), plus `file_info`. Measured in Claude Code, 5 runs each with a warm cache:

| "Find the business plan PDF I saved recently" | Shell tools (`find`, `mdfind`) | Ferret |
|---|---|---|
| Time | 81.5 s | **5.3 s** |
| Turns | 5.2 | **2** |
| Cost per search | $0.096 | **$0.016** |

### Your privacy comes first

- **Names and metadata only.** Ferret never reads or returns file contents.
- **You choose what agents can see.** By default, only your home folder. Narrow it in `~/.config/ferret/config.toml`:

  ```toml
  [mcp]
  allow = ["~/Projects", "~/Documents"]
  deny  = ["~/Documents/Private"]
  ```

- **Secrets are always off limits**, whatever the config says: `~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.kube`, `~/.docker/config.json`, `~/.netrc`, `~/.config/gh`, Keychains, Messages, Mail, Cookies, Safari, and Chrome, Firefox, Brave and Arc profiles.
- **At most 100 results per call**, and every call is logged to `~/Library/Logs/ferret/mcp.log`, so you can see exactly what your agent looked for.

## Full Disk Access

macOS keeps some folders (Mail, Messages, Safari, other apps' data) private until you allow access. Ferret works without it; those folders just won't be in your results. To include them: **System Settings → Privacy & Security → Full Disk Access → add `ferretd`**. `ff doctor` shows the exact path.

## How fast is it?

Measured on the author's Mac (Apple Silicon, internal SSD), about 4.1 million files and folders:

| | |
|---|---|
| Search over the whole disk | **~20–40 ms** end to end |
| New or changed files visible | **under 1 s** |
| Restart until searchable | **~0.65 s** (loads a snapshot) |
| First full scan of `/` | ~90 s, once |
| Daemon idle CPU | ~0.4% |
| Index in memory | ~300 MB |

Against the tools you already have, searching for `invoice`:

| Command | Time | Matches | Looks at |
|---|---|---|---|
| `ff invoice` | **37 ms** | **145** | the whole disk, hidden and system folders included |
| `mdfind -name invoice` | 31 ms | 70 | only what Spotlight indexes |
| `find ~ -iname '*invoice*'` | 99 s | | your home folder only |

Spotlight speed, seeing twice as much, and about 2,700× faster than `find`.

## How it works

One flat array of nodes and one buffer of names, NFC-normalized and searched on every core. A daemon owns the index and follows FSEvents; the CLI, the picker, the menu bar app and the MCP server are all thin clients of one small JSON socket protocol. The first scan lists each folder with `getattrlistbulk`, which gets names, sizes and dates in a single call. Curious? [CONTRIBUTING.md](CONTRIBUTING.md) maps out the crates.

## Contributing

Bug reports, ideas and pull requests are all very welcome 💛 Start with [CONTRIBUTING.md](CONTRIBUTING.md). For anything security-related, see [SECURITY.md](SECURITY.md).

## License

[MIT](LICENSE). Use it, fork it, ship it.

<div align="center">
<br>
<sub>Made with ❤️ and a lot of curiosity by <a href="https://github.com/nimiology">@nimiology</a>, for everyone who has ever lost a file.</sub>
</div>
