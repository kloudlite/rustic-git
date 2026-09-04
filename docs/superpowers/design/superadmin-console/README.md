# Superadmin console — approved mockups (2026-09-04)

Seven static screens the owner approved, exported from the design canvas
(https://claude.ai/code/artifact/39fa8b5e-0733-40d1-a603-98b9ee613fe1). Each `*.dc.html` is one
screen at 1440×900 with the app's tokens inlined (Mona Sans / Hubot Sans, zinc greys, brand blue,
sharp corners, 56 px header, 32 px controls, 13 px table text, 12 px muted captions).

| file | screen |
|---|---|
| Main.dc.html | Overview |
| Requests.dc.html | Requests queue with the decision panel |
| Owners.dc.html | Owners list with the Defaults card |
| Owner.dc.html | Owner detail |
| Clusters.dc.html | Clusters list |
| Cluster.dc.html | Cluster detail: nodes with drain, per-region workloads |
| Monitoring.dc.html | Central workloads and the signals table |

The web tasks of `docs/superpowers/plans/2026-09-04-superadmin-console.md` build these with the
app's real components (`components/ui/*`, tokens from `globals.css`), never with the inline styles
here: the mockups fix layout, hierarchy, density, copy and states; the components fix the pixels.
