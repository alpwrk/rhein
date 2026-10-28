# Vendored livekit-signaling (LiveKit rust-sdks, rev c20fa7c)

A copy of the `livekit-signaling` crate from https://github.com/livekit/rust-sdks at
`c20fa7c37e50cb97f1a588a6ad50534b53c2037e`. Rhein's root `Cargo.toml` patches the git
dependency with it.

Changes:
- `SignalClient::set_token`. The token that every resume and full reconnect
  re-reads could only be changed by the server's own RefreshToken messages, so a
  Telium join token renewed mid-call had no way onto the live session. Telium's
  voice recovery depends on it: Rhein mints a fresh grant at half the token's
  lifetime and installs it here, so the first network blip is a resume, not a
  reconnect.
- Workspace-inherited manifest fields are written out, and the sibling crates
  come from the same git rev.
