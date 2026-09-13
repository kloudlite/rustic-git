# kloudlite harness

A desktop client for working with AI agents, not for editing files yourself.

## The hierarchy the UI is built on

    team
     ├─ environment ×N       the services a team runs; a developer clones one to
     │                       get a copy they can break
     └─ bench                exactly one per developer per team, connected to one
         │                   environment at a time
         ├─ thread ×N        the first may change things, the rest are read-only
         └─ workspace ×N     a working copy the developer can enter
             └─ ephemeral ×N a copy cut for one agent, watched not driven

Switching team is what switches machine: there is nothing to choose within one.
Environments belong to the team, so connecting to one is a change of address,
not of ownership, and cloning is what gives a developer their own.

The three columns follow the machine: the left panel is what exists, the centre
is the conversation that drives it, the right is whatever is selected, in full.

## Running it

    npm install
    npm start          # builds main + renderer, then launches

    npm run build      # tsc for main/preload, vite for the renderer
    npm run typecheck  # both tsconfigs, no emit

## Running the bench

The harness no longer runs pi. It is a view of a bench: `harness-bench`, one per person per team, serving the sessions in a bench folder on port 7789.

    node bench/src/main.ts --dir /path/to/bench [--host 127.0.0.1] [--port 7789] [--read-only] [--wait] [--idle-secs N]
    node bench/src/main.ts --ping [--port 7789]
    HARNESS_BENCH=http://127.0.0.1:7789 npm start

On the platform the folder is `/bench` and the address is the local end of `kl-connect bench`. There the listener binds `0.0.0.0` behind a gateway-only NetworkPolicy, a held folder lock exits 75 (the agent shows `FolderLocked` and the pod restarts), and `--ping` is the readiness probe. With no client connected and nothing running for the region's `benchIdleSecs`, the bench exits 0 and has no pod; the next connection through `kl-connect bench` starts it, and the first request waits out a cold start of seconds. On a laptop pass `--host 127.0.0.1` and `--wait` to wait for a held lock; leave `--idle-secs` unset (0), and it never sleeps. `--read-only` is a bench whose owner has left the team: it serves history with no pi at all. `npm run bench:test` runs the bench's tests; they need `flock(1)` (`brew install flock` on a Mac).

Env: `KL_MODEL` (default model, overrides `--model`'s default), `KL_BENCH_IDLE_SECS` (default for `--idle-secs`), `KL_TEAM` (passed through to pi's extensions unchanged), `TERMINATION_LOG` (where a lock or idle exit writes its reason instead of `/dev/termination-log`), `NODE_NAME` (the lock holder's name; falls back to the hostname). Exit codes: `0` a clean stop or an idle exit, `75` the folder is locked by someone else, `2` a bad `--idle-secs`.

To bring this laptop's old sessions onto the bench, run "Import this laptop's sessions into the bench" from the palette. Running it twice changes nothing.

Two env vars help when looking at the UI from a terminal:

    HARNESS_SHOT=out.png    capture the window once after first paint, then exit
    HARNESS_THEME=light     open in a given theme (also `?theme=` on the URL)
    HARNESS_SIZE=1920x1080  window size for the capture (default 1360x860)
    HARNESS_HASH=eph-a1/changes   open with a node selected and a view showing

## Keys

Defined once in `src/renderer/keys.ts`, which the composer's hint row is
generated from, so the two cannot drift. Only bindings that do something are
listed: a shortcut for a thing that is not built teaches a lie.

    ⌘↩      send                 ⌘B      workspaces
    ⌘L      focus the prompt     ⌘⌥B     inspector
    ⌘J      shell                ⌘1…⌘9   thread by position
    ⌘E      environment          ⌘[ ⌘]   previous / next thread
    ⌘W      close what is open   esc     back, one layer at a time

## Design system

Colour lives in `src/renderer/styles/app.css` in two layers and nowhere else.

1. **Theme** — one block of raw values per theme, keyed by the role names Zed's
   One Dark and One Light use, so any value can be checked against the source.
   These are the only declarations that change when the theme changes.
2. **Tokens** — `@theme inline` maps those values into Tailwind's namespaces, so
   `bg-panel`, `text-muted`, `border-line` exist as utilities that resolve
   *through* the vars. Because of that, no component carries a `dark:` variant
   and no component names a hex value: swapping `data-theme` restyles the app.

Three themes, not two: "system" sets no attribute and lets the media query
decide; "light" and "dark" stamp `data-theme` and win over it. The choice is
remembered in `localStorage` and handed to the main process, because the preview
window's title bar is painted by the OS rather than by this stylesheet.

Icons are Lucide through `lucide-solid`, which is the set Zed ships (its
`assets/icons` are Lucide under the ISC licence). `ui/Icon.tsx` names them by
role rather than by glyph and tunes the stroke to Zed's weight.

Type, space and radius are tokens too. The spacing base is 4px, so `h-6.5` is
the 26px row every list in the app shares. Fonts are the ones Zed ships — IBM
Plex Sans for the UI, Lilex for anything monospaced — vendored under
`src/renderer/fonts` with their OFL licences.

Two text rules hold everywhere: a row clips to one line with an ellipsis, and a
paragraph wraps at word boundaries and never inside a word.

## Layout

    src/
      main.ts            window, preview windows, native theme
      preload.ts         the only bridge: openPreview, setTheme
      renderer/
        model.ts         the data shapes, with placeholder content
        theme.ts         theme mode, persisted
        styles/app.css   theme + tokens + the few things utilities cannot say
        ui/              primitives with no domain knowledge
        components/      the session's own views
