# Revs

Line history for [Zed](https://zed.dev), like GitLens's "Open Changes with Previous Revision".
From the line you're on, open the commit that last changed it, or the next one, in Zed's
side-by-side diff, scrolled to that line. It works from both sides of the diff, so you can keep
walking one line's history backwards and forwards. Optional git blame for the cursor line.

A 0.77 MB native language server that uses about 1.6 MB of memory. Free and open source.

| Command | What it does |
| --- | --- |
| **Revs: Previous Revision** | The commit that last changed the line, against its parent. If the open diff already shows that line's change, it goes one change further back. On a removed line (left side), it opens the commit that wrote it. With several lines selected, the newest change among them. |
| **Revs: Toggle Line Blame** | Shows or hides blame for the cursor line (`You, last month • Fix typo (#12)`). It is off by default, and the choice is remembered. |
| **Revs: Next Revision** | The next commit that changed or removed the line. At the end it shows your uncommitted changes. |
| **Revs: Open Working File** | From a diff, opens the working file (editable) at the same line, followed through later commits. |
| **Revs: Open Commit on GitHub** | Opens the commit on GitHub (or GitLab or Bitbucket, named after your remote). |

Each step opens a new diff tab named after the two commits, e.g. `1f3c2e9 ↔ 8a7b6c5 parser.rs`.
A card in the corner names the commit shown, its author and date, and the file and line the
step started from, with a link to the commit. It closes by itself.

Requires a Mac with Apple silicon, macOS 27 or later, and Zed 1.23 or later.

## Install

In Zed, open the extensions page (Cmd-Shift-X), search for **Revs** and install it. The first
time it starts, the extension downloads the Revs server (`revs-aarch64-apple-darwin.zip` from
this repository's release of the same version, about 0.4 MB) into its own folder, and checks
every file of it against the checksums the extension carries: a server that doesn't match is
never run. Revs runs on Macs with Apple silicon only; elsewhere the extension says so instead
of starting.

Not in the list yet? Clone this repository and run `zed: install dev extension` on its folder
(needs Rust from [rustup](https://rustup.rs) with `rustup target add wasm32-wasip2`).

Then add these to Zed's `settings.json` (Cmd-, ):

```jsonc
// Cmd-Shift-P → type "revs" → Enter opens Revs's commands.
"command_aliases": { "revs": "lsp_command_selector::Toggle" },
// Keep diffs side by side in narrow panes too: removed lines only exist in the split view.
"minimum_split_diff_width": 0
```

If you list `language_servers` for a language yourself without `"..."` (all the others), add
`"revs"` to that list, e.g.

```jsonc
"languages": { "Python": { "language_servers": ["basedpyright", "ruff", "revs"] } }
```

## Using it

Click a line, press Cmd-Shift-P, type `revs` and press Enter. The commands are listed in the
order above, so Enter again runs Previous Revision; `n` then Enter runs Next Revision. Zed puts
the last palette command first, so afterwards Cmd-Shift-P then Enter brings the list back.

Zed extensions can't add rows to the command palette. Zed does list language-server commands
in its *LSP command selector*, so Revs's commands live there, and the alias opens it when you
type `revs`. Without the alias, type `lsp command` instead. The selector shows "Executing
command…" until the diff is open.

Commands act on the line where the cursor last moved. Zed tells language servers the cursor
position about 250 ms after it settles, and doesn't report switching tabs, so click the line
first. The card says which file and line a step used. Zed connects no language server to a
file with a line longer than 20,000 characters (`global_lsp_settings.max_buffer_line_length`),
so in such a file the commands act on the last line clicked elsewhere.

In a diff, only the right side reaches language servers. A cursor on the left is reported as
the right-side line beside it, which is enough for every line except removed ones. A click on
a removed line arrives as the start of the first line after the removal. So the very start of
that line counts as "the removed lines": click anywhere else in it to mean the line itself.
(Rows removed at the end of a file without a final newline arrive as the end of its last line
instead.) When the removed lines came from different commits, or when two different lines sit
side by side, Revs asks which one. Right after a step, Revs knows exactly which line the new
diff is about, and the opposite step (Next after Previous) goes straight back.

Each step opens a new diff tab next to the current one; Zed can't replace a diff tab from
outside. Cmd-W closes it and returns to the previous tab. In a project that is a single file
rather than a folder, Zed brings that file's tab forward before each diff, so diffs open after
it and Cmd-W returns to the file; open its folder to keep diffs in step order. While a diff is
open, your language servers also see the old version, so Go to Definition may list it too.

Line blame is an inlay hint, so it needs Zed's inlay hints turned on. To show only Revs's:

```jsonc
"inlay_hints": {
  "enabled": true,
  "show_type_hints": false,
  "show_parameter_hints": false,
  "show_other_hints": true,
  "edit_debounce_ms": 0      // follows the cursor without delay
}
```

## Settings

All optional, under `lsp.revs.initialization_options` (or `lsp.revs.settings`):

| Key | Default | |
| --- | --- | --- |
| `stepCard` | `true` | the card naming the commit after each step |
| `annotation` | `false` | line blame when it hasn't been toggled yet |
| `fastCursor` | `false` | also follow the cursor through linked-editing requests (about 50 ms instead of about 250 ms). Zed sends those only to the first server that offers linked editing, so this can take tag renaming away from HTML or Vue servers. |
| `summaryLength` | `50` | characters of the commit summary in the line blame |

To run a server you built yourself, set `lsp.revs.binary.path` to it, with
`"arguments": ["lsp"]`.

## Privacy

Everything stays on your Mac. Revs reads your repository with git, writes diff snapshots to
`~/Library/Caches/Revs` and the line blame setting to `~/Library/Application Support/Revs`
(folders only you can open), registers its `revs://` links with macOS, and talks only to Zed.
The extension downloads the server from this repository's GitHub releases; apart from that,
the only network access is git's own: in a partial clone, git fetches the file versions it
doesn't have yet from your remote, as it does for any git command there. No telemetry, no
accounts.

In a project you share with collaborators, those who can edit can run Revs's commands too,
like any language server's. A step that ends at the working file shows its unsaved changes,
including ones made in another Zed window.

## How it fits into Zed

Zed extensions can't add buttons or commands, so Revs is a small native language server that
the extension starts for every file:

| Feature | Zed mechanism |
| --- | --- |
| Commands | `workspace/executeCommand`, listed by Zed's LSP command selector |
| Cursor line | the `textDocument/codeAction` request Zed sends after the cursor settles |
| Side-by-side diffs | `zed --diff old new:line:column` |
| Commit card | `window/showMessage` |
| Open Working File / Open Commit | `window/showDocument` |
| Line blame (optional) | an inlay hint at the end of the cursor line, with a hover card |

Revs lines rows up with imara-diff's histogram diff, the same diff Zed draws, and blames with
git's histogram algorithm, so the lines it reasons about are the lines Zed shows as changed.
Moved or reflowed lines are followed across nearby changes.

Diff sides are read-only snapshots in `~/Library/Caches/Revs/repos/<repository>/<folder>/`,
named after their commit. Commit snapshots are shared between diffs, and a snapshot is never
rewritten while it exists. Snapshots are removed once no diff can still show them: after Zed
restarts, or a week after they were last used, unless a diff in another Zed window still shows
them. The line blame hover card's links carry only a random id for a record in Revs's cache, so
they work on this Mac, for up to a day.

## How it stays small

Git runs in short-lived processes, using the real git binary rather than the `/usr/bin/git`
shim. On a 13,000-line file with a few hundred commits of history, a step usually takes 0.05–0.3 s before
Zed starts drawing the diff (up to about 0.5 s when `git blame` has to go far back). The server
is a 0.77 MB binary, and its memory footprint (Activity Monitor's "Memory") stays around 1.6 MB,
measured with `footprint` during a soak test of thousands of cursor moves, edits and steps.

## Build from source

Needs Rust from [rustup](https://rustup.rs) (the build uses its toolchain even when another Rust
comes first on `PATH`):

```sh
rustup target add wasm32-wasip2
make install    # the server as revs-dev in the extension's folder, and the extension
make test       # unit tests + an end-to-end LSP session against a throwaway repository
```

After an update, Zed keeps running the old server: run `editor: restart language server` in a
file, or restart Zed. `make uninstall` removes everything (your Zed settings stay as they are).

`revs previous|next|open-file <file> <row>` does the same as the commands from scripts. `<file>`
is a working file or a Revs snapshot. Protocol trace: `touch ~/Library/Caches/Revs/trace` and
watch `~/Library/Caches/Revs/trace.log` (delete `trace` to stop).

## License

MIT
