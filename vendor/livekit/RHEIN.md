# Vendored livekit (LiveKit rust-sdks, rev c20fa7c)

A copy of the `livekit` crate from https://github.com/livekit/rust-sdks at
`c20fa7c37e50cb97f1a588a6ad50534b53c2037e`. Rhein's root `Cargo.toml` patches the git
dependency with it.

Changes:
- `Room::set_token`, next to `Room::token`, so a caller can install a fresh
  signalling token on the live session (the engine re-reads it on every resume
  and full reconnect). Telium's voice recovery depends on it: Rhein mints a
  fresh grant at half the join token's lifetime and installs it here, so the
  first network blip is a resume, not a reconnect. It needs
  `livekit-signaling`'s `set_token` alongside (see its RHEIN.md).
- Workspace-inherited manifest fields are written out, the sibling crates come
  from the same git rev, and the dev-dependencies are dropped (a dependency's
  tests are never built).
