# Changelog

Every release of Rhein, newest first. The release notes on GitHub come from here.

## Unreleased

Catches up with Telium server 0.5 and desktop 0.4, which changed what an account needs before people on the web client can message it.

- **Encrypted DMs**: accounts made in Rhein now get their identity key at the first sign-in, as the web client makes it. Since Telium desktop 0.4.0 the web client leaves out of group chats anyone without one, so until now an account that had only ever used Rhein could not be messaged from a browser. An account whose key the web client already made keeps that one.
- **Recovery key**: signing up makes a recovery key, and Rhein asks you to save it (Settings → Encryption). If you forget your password, the Telium web client's password reset takes it and keeps your encrypted history. Accounts made with an earlier Rhein can make one there with their password.
- **Group chats**: group names set in the web client show in Rhein, and right-clicking a group renames it, for whoever the group's rules allow.
- **Sign-up**: servers that register by invitation take the registration code you were given.
- **Voice and video**: when the voice relay pauses your camera or screen for going over its bandwidth limit, Rhein sends it again at a lower rate. When it pauses someone else's, Rhein says why the picture stopped.
- **Sign-in**: an account whose password uses a newer key-stretching setting than this Rhein knows gets a message to update Rhein, instead of "wrong password". A vault in a custody mode Telium retired shows as locked instead of stopping encrypted messages from starting.
- **Fixes**: a device let back into a group after a start-over no longer rejoins from information about the old group, and its rejoin request now says which key it holds and how far it has read, so whoever answers can repair it.

## 0.1.1 – 2026-10-29

Rhein now keeps itself up to date, and calls in voice channels hold up through server and network trouble instead of ending.

- **Voice and video**: a call in a voice channel waits through a server going down or restarting for a few minutes and rejoins on its own when it's back, instead of ending for everyone. Long calls renew their connection in the background, so the first small network hiccup no longer forces a full reconnect. Sound that breaks mid-call repairs itself, usually within seconds, by catching the call's encryption back up or asking for its group to be rebuilt. When a moderator disconnects you, Rhein says so and leaves the call, instead of leaving you in one that has gone silent.
- **Install**: Rhein tells you at startup when a new version is out and installs it when you click Install. You can also look for one, or turn the check off, under Settings → About. Your settings, accounts and messages stay as they are. Every update is checked against Rhein's release signature before anything is replaced, and a copy installed under `/usr` only shows the notice. `./install` and `install.sh` now install for you into `~/.local` (use `--system` for `/usr`) and point out an older copy left in `/usr`. A build from source no longer prints a wall of warnings from bundled code.

Updating from 0.1.0: that version has no updater yet, so install 0.1.1 once by hand. Later versions come through the app.

## 0.1.0 – 2026-10-28

The first public release of Rhein, a native Telium client for Linux, and the start of its open beta. It works next to Telium's web client on the same account.

- **Chat**: guild channels with history, editing, deleting, replies, reactions, pins, threads and search. Markdown, images and link previews render as in the web client. Uploads by paste or drag and drop, and H.264 videos (MP4, MOV) play inline.
- **Federation**: one account on several servers, invites across servers, and one gateway per server.
- **Encrypted DMs**: direct messages and group chats use MLS on openmls with Telium's profile and interoperate with the web client. Recovery vault, cross-signing, identity continuity, message requests, and the encrypted history mirror, so a new device sees earlier messages.
- **Voice and video**: guild voice and DM calls over mesh or an SFU, encrypted end to end with keys derived from MLS. Speaking indicators, mute and deafen, push-to-talk, per-user volume, echo cancellation, region probing, and screen sharing through the desktop portal.
- **Profiles and presence**: banners, accents and card themes. Activities from a Discord-compatible RPC server, game detection and MPRIS.
- **Guilds and bots**: AutoMod rules, forums, channel settings, timeouts and nicknames. Bot buttons, selects, cards, modals and ephemeral messages.
- **Keyboard**: a Ctrl+K switcher, keyboard navigation, an emoji picker and autocomplete.
- **Install**: a prebuilt build for x86_64 Linux that runs on glibc 2.35 or newer, on Wayland and X11.

Not done yet: read receipts, camera capture, the recovery key, a gapless switch from mesh to SFU, and translations.
