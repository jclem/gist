# gist

A fast GitHub Gist CLI with an interactive TUI, written in Rust.

## Setup

Set a GitHub personal access token with the `gist` scope:

```sh
export GH_GIST_TOKEN="ghp_..."
```

## Install

```sh
cargo install --path .
```

## Usage

### Create a gist

Pipe content to `gist` via stdin:

```sh
echo "hello world" | gist
cat script.py | gist -n script.py
```

Or open your `$EDITOR`:

```sh
gist -e -n notes.md
```

Use `-p` to make the gist public (gists are secret by default).

### Show a gist

```sh
gist <url-or-id>
```

Prints gist file contents to stdout.

### List your gists

```sh
gist list
```

### Delete a gist

```sh
gist delete <url-or-id>
```

### Interactive TUI

```sh
gist tui
```

Browse your gists in a split-pane terminal UI with syntax highlighting.

| Key           | Action               |
|---------------|----------------------|
| `j` / `k`    | Navigate up/down     |
| `h` / `l`    | Collapse/expand      |
| `Enter`       | Expand/select file   |
| `Esc`         | Back                 |
| `Tab`         | Switch pane          |
| `Ctrl-h/l`   | Jump to pane         |
| `n`           | Create new gist      |
| `e`           | Edit gist in $EDITOR |
| `d`           | Delete gist          |
| `r`           | Refresh              |
| `q`           | Quit                 |
