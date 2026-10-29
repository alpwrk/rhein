# Changelog

Every release of Rhein, newest first. The release notes on GitHub come from here.

## Unreleased

- Added: Rhein tells you when a new version is out, and installs it when you click Install. It looks once at startup, or whenever you ask under Settings → About, where you can also turn the check off. Your settings, accounts and messages stay as they are, and every update is checked against Rhein's release signature before anything is replaced. A copy installed for the whole system, under `/usr`, only shows the notice.
- Fixed: calls in a voice channel now stay up through server trouble. If the server a call runs on goes down or restarts for a few minutes, the call waits for it and rejoins on its own when it's back, instead of ending and taking everyone out of the voice channel.
- Fixed: long calls no longer reconnect out of nowhere. A call that has been going for a while used to be knocked into a full reconnect by the first small network hiccup, because its grant to the call had quietly expired. The call now renews its connection in the background, and the conversation just continues.
- Fixed: sound that breaks mid-call now repairs itself. If a glitch left the call unable to hear you, or you unable to hear it, it used to stay broken until everyone left the channel and joined again. The call now catches its encryption back up on its own — and, if the group is truly lost, asks for it to be rebuilt — usually within seconds.
- Fixed: being removed from a voice channel now ends the call clearly. If a moderator disconnects you, the app tells you what happened and leaves the call with you, instead of leaving you sitting in a call that has quietly gone silent.

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
