# Stage 3 — Rich actions

> **Status: the definition of done passes.** A note declaring all five kinds produces:
>
> ```text
> Alt+1  OBS cursor follow   OpenFile   …/obs-follow.md:7
> Alt+5  smoothing.rs        OpenFile   …/proj/src/smoothing.rs:1
> Alt+6  does-not-exist.md   OpenFile   …/does-not-exist.md:3   (unavailable)
> Alt+7  ▶ 06:54             OpenVideo  https://www.youtube.com/watch?v=ABCDEF @06:54
> Alt+8  Ghostty             LaunchDesktopApp  ghostty_ghostty
> Alt+9  proj                OpenProject       …/proj
> ```
>
> The timestamp there was lifted out of `&t=6m54s` in the URL and the URL stored clean;
> the broken `@file` is disabled rather than missing; `@app ghostty` resolved through the
> `.desktop` index. `grep -rn 'RunShell|"sh", "-c"|Command::new("sh")'` finds only the two
> comments explaining why no such thing exists.
>
> **Found while verifying:** every `[Note]` button was rendering *disabled*. `yalive` stores
> `files.path` relative to the vault root, and everything downstream treats a `Hit`'s path as
> something to open — so `exists()` was resolving against the daemon's working directory and
> `Alt+1`, the keystroke this whole product is shaped around, would have opened nothing.
> Fixed at the one place that knows the vault, with a test that the path is absolute *and*
> exists.
>
> Two deviations from what is written below. The video handler is configured under
> `[openers] video` rather than a new `[actions.video]` table, since the fallback chain
> (configured → mpv → xdg-open) makes a separate `fallback` key redundant. And `CopyText` is
> not implemented — `Ctrl+C` already copies the answer, and no `@action` syntax produces a
> copyable snippet.

**Goal:** one answer can jump into a note, a video at a timestamp, a code file at a line,
an application, or a project — without typing another command.

This is the feature that makes Brain Dock different from a nice grep. Stage 1 already
stored `@action` metadata; this stage resolves and executes it.

**Prerequisite:** Stage 2 verification passes, and you have used it for a while.

---

## Deliverables

- Full `@video` / `@file` / `@app` / `@project` / `@url` resolution.
- `.desktop` application index.
- Action buttons in the dock with `Alt+1..9` and `Tab` cycling.
- Timestamped video opening.
- `CopyText` action.

---

## 3.1 Action resolution

The `ActionKind` enum from spec §11, unchanged. **No `RunShell` variant exists** — not
behind a flag, not commented out. Once the type cannot represent an arbitrary command, an
entire category of bug and vulnerability is structurally impossible.

Resolution happens at index time, in trusted parser code, from parsed metadata. The LLM
never contributes to an `Action` record. It may *mention* an action in prose; the buttons
come from the database (spec §3.3, §23).

Parsing rules, all with unit tests:

| Syntax | Produces | Notes |
|---|---|---|
| `@file PATH` | `OpenFile` | expand `~`, resolve relative to the note's directory |
| `@file PATH:LINE` | `OpenFile` + `line` | `:83` suffix; a path containing `:` needs the last-colon rule |
| `@video URL` | `OpenVideo` | validate with `url::Url::parse`, scheme in `{http,https,file}` |
| `@video URL HH:MM:SS` | `OpenVideo` + `timestamp_seconds` | also accept `MM:SS` and bare seconds |
| `@app DESKTOP_ID` | `LaunchDesktopApp` | resolved against the `.desktop` index |
| `@project PATH` | `OpenProject` | must be a directory |
| `@url URL` | `OpenUrl` | |

A YouTube URL already carrying `&t=414` should have its timestamp extracted into
`timestamp_seconds` and the canonical URL stored clean — then rebuilt at launch time by
trusted code (spec §31). Do not round-trip a URL the model or a note author hand-edited.

**Validation is at index time.** A `@file` pointing nowhere becomes an action marked
untrusted (`trusted = 0`) and is rendered disabled with a tooltip, rather than silently
dropped. A dead link you can see is more useful than one that vanished.

Every section also gets an implicit `OpenFile` action for its own document at
`start_line`. That is the `[Note]` button and it is the one you will press most.

## 3.2 Desktop application index

Spec §32. Scan `~/.local/share/applications` and each `$XDG_DATA_DIRS/applications`, parse
the ini-format `.desktop` entries, and store `id` (filename minus `.desktop`), `Name`,
`Exec`, and the file path.

- Skip `NoDisplay=true` and `Hidden=true` entries.
- Rescan on daemon start and on a `notify` watch of those directories.
- Match `@app blender` case-insensitively against id first, then `Name`, then a prefix of
  the id. `@app` values in notes are hand-written and will not always be exact.

**Launch with `gtk-launch <id>`** (present on this machine). It handles `Exec` field codes
(`%f`, `%U`, `%i`) and startup notification correctly. Hand-parsing `Exec` and spawning it
yourself gets field codes wrong in ways that break on some apps. Keep a configurable
override for the rare app that needs it.

## 3.3 Video handler

Config, per spec §31:

```toml
[actions.video]
handler = ["mpv", "--start={seconds}", "{url}"]   # or your own client
fallback = ["xdg-open", "{url}"]
```

Resolution order: configured handler if its binary exists → `mpv` if present →
`xdg-open` with a rebuilt timestamped URL. For YouTube that means appending `&t={n}s`
in trusted code.

If the handler binary is missing, fall back silently and log — spec §51 lists this
explicitly as a graceful-degradation case.

## 3.4 UI

Buttons render from `ActionView` records in the `Actions` event. Layout per spec §4:

```text
[Note] [Code] [▶ 06:54] [OBS] [Copy]
```

- Label a video button with its formatted timestamp, not the URL.
- `Alt+1..9` activates by position; `Tab`/`Shift+Tab` cycles; `Enter` on a focused button
  activates it.
- Cap at 9 visible actions; if there are more, the source list (`Ctrl+S`) is where they go.
- Deduplicate identical targets across sections — three sections referencing the same
  video should produce one button.
- **Hide the dock before spawning**, so focus lands in the opened application.
- `Ctrl+C` copies the answer text; a `CopyText` action copies a specific snippet (use
  `arboard` for the clipboard — X11 clipboard ownership requires a live process, and
  `arboard` handles the daemon-side detail).

Order buttons: Note first, then Code, Video, App, Project, Copy. Stable ordering matters
more than optimal ordering because `Alt+1` becomes muscle memory.

---

## Definition of done

```text
One answer can jump into note, video, code, or app without typing another command.
```

Using a real note containing all five metadata types:

```bash
brainctl sources          # actions listed with kinds and targets
```

- `Alt+1` → nvim at the exact line, in ghostty.
- `Alt+2` → the referenced source file at its line.
- `Alt+3` → the video opens *at the timestamp*, not at 0:00.
- `Alt+4` → the application launches.
- A note with a broken `@file` path renders a disabled button, and the daemon does not
  crash or drop the other actions.
- `grep -rn "RunShell\|sh\", \"-c\|Command::new(\"sh\")" crates/` returns nothing.
