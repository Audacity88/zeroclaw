# Config pane

zerocode's **Config** pane is the way to configure a running ZeroClaw. Each
setting has a typed control, validation, and an inline explanation of what it
does, and most settings apply live without a daemon restart. Open it from any
zerocode session and edit settings there rather than hand editing the config
file.

Settings still persist to your config, and the docs
describe the relevant fields so you can see exactly what a given control writes. Read
those descriptions as the persisted result, not as an
instruction to open the file in an editor. Hand editing is a fallback for
headless hosts and scripted provisioning, where the docs call it out
explicitly.

## Save and cancel drafts

The editor shows your current Save and Cancel shortcuts and whether edits are pending. Save works for text, choices, arrays, personality files, and Skills. Enter also saves text and choices; in multiline editors it inserts a newline. Cancel discards the current edit without saving. In a filtered choice list, Cancel clears the filter first.

Switching to another pane parks your draft. Navigation that would replace a pending draft asks you to stay or discard it. Failed saves keep the draft for retry. Saving an inline personality file keeps its content open for further edits.

Before opening an external personality or Skills editor, Config explains that a successful editor exit imports and saves changed content. You can cancel before launch and continue in the inline editor.

## Keybinding modifiers

Keybindings use canonical modifier names: `control` is literal Control, `primary` is Command on macOS and Control elsewhere, and `super` is literal Super/Command. For example, `control+c`, `primary+r`, and `alt+shift+up` are portable persisted values. Older `ctrl+...` values are migrated once when the config loads and rewritten to the corresponding canonical spelling.

## Why the pane over the file

- **Validation.** Controls reject malformed values before they reach the
  daemon, so a typo cannot leave the config in a state that fails to load.
- **Discoverability.** Every setting carries an inline description, so you do
  not have to cross-reference the config reference to know what a field does.
- **Live apply.** Most settings take effect on the next frame, with no restart.
- **Registry-backed lists.** Provider, channel, model, and theme choices come
  from the backend registry, so the options you see are exactly the ones this
  build supports.

## Field labels and full details

Config field rows use readable labels and keep long values to a one-line preview. Open Help while a field is selected to read its exact configuration path, complete description, and current value. The help view scrolls, including long array values and paths. Filtering continues to match the configuration path.

An unset field is shown as `<unset>`, secret values remain hidden, and environment overrides are marked. Unit and default explanations come from the field description. A current value does not establish whether you explicitly configured it or inherited a default.

## Local UI settings (`zerocode-config.toml`)

Some settings describe how *zerocode itself* draws its panes rather than how the
daemon behaves. Those live in zerocode's own file,
`<config-dir>/zerocode-config.toml`, and are edited from zerocode's **Config**
pane.

The TodoWrite tracker is one of them. It is a display-only concern: the daemon
just emits plan updates, and over ACP the client controls formatting entirely,
so it is owned by zerocode:

```toml
[todotracker]
enabled = true           # master switch; when false the tracker never renders
enabled_at_start = false # visible at launch, before the first plan arrives
location = "right"       # "bottom", "left", or "right"
width = 32               # side-panel target column width (left/right)
max_height = 5           # bottom-strip maximum height in rows
```

The shell-level agent sidebar is stored in the same file. The default section
is serialized explicitly so environment overrides have a schema node to
target:

```toml
[sidebar]
visible = true # show the agent/session sidebar at launch
width = 24     # target width in terminal columns
```

Press `Ctrl+B` to show or hide the sidebar. Quickstart remains reachable from
the keyboard mode bar and also appears as a sidebar launcher, including at
narrow terminal widths. Selecting an existing agent from the sidebar starts a
new Chat or Code session without replacing the other sessions already tracked
by that pane.

Each Chat or Code pane tracks at most eight sessions by default, including the focused session, background sessions, and retained reconnect entries. Headless and scripted setups can change this client-side bound in `zerocode-config.toml`; the Config pane does not expose this field yet:

```toml
[sessions]
max_tracked_per_pane = 8 # valid range: 1 through 32
```

The daemon enforces its session limits independently. Increasing this value cannot exceed a lower server-side limit. ZeroCode resolves it at startup; reconnects within the same run keep the original value. Malformed or out-of-range values prevent startup instead of silently falling back.

The Config pane shows the selected field's description below the editor. Use the description's displayed scroll bindings or the mouse wheel over it to read longer text. The Todo tracker section also shows the effective shared dock side and width; dock geometry remains shell-owned rather than a numeric Todo editor.

TodoWrite values are re-read at every session boundary, so an edit made in the
Config pane applies to the next session you start, restart, or switch to, with
no zerocode restart needed.

### Environment overrides

Any field can be overridden for a single run with a `ZEROCODE_` variable. The
spelling is the prefix followed by the lowercase config path, with `.` written
as `__`:

```sh
ZEROCODE_todotracker__enabled=false zerocode
ZEROCODE_todotracker__location=bottom zerocode
ZEROCODE_sidebar__visible=false zerocode
ZEROCODE_sessions__max_tracked_per_pane=12 zerocode
```

These overrides are process-transient: they affect the running instance only and
are never written back to `zerocode-config.toml`. Saving an unrelated field in
the Config pane will not bake an env-injected value into the file.

### Upgrading from a daemon-owned `[todotracker]`

Before this setting moved, `[todotracker]` was a section of the *daemon's*
`config.toml`. If you set it there, copy the values across:

1. Open your daemon `config.toml` and note the `[todotracker]` values.
2. Put the same block into `<config-dir>/zerocode-config.toml` (shown above), or
   set them from **Config → Todo tracker**.
3. Delete the `[todotracker]` section from the daemon `config.toml`.

Existing `ZEROCLAW_todotracker__*` environment variables do **not** need to be
removed before upgrading: the five recognized fields (`enabled`,
`enabled_at_start`, `location`, `width`, `max_height`) are accepted and ignored
by the daemon so a previously working deployment still starts. They no longer
have any effect, so move them to the `ZEROCODE_` spelling above.
