// Runs the Telium web client's own DM code -- web/src/lib/{opaque-auth,vault,mls,verify,api}.js,
// unmodified, from the clone in .dev/telium/web -- outside a browser, so Rhein's tests can hold
// encrypted conversations with the real thing. IndexedDB and localStorage are provided in memory.
//
//   cd .dev/telium/web && bun install        (once)
//   bun scripts/web-interop/web-client.mjs   (driven by crates/rhein-store/tests/web_interop_live.rs)
//
// One JSON request per line on stdin, one JSON answer per line on stdout.

import { createRequire } from 'node:module';
import { createInterface } from 'node:readline';
import { join, resolve } from 'node:path';

const WEB = resolve(process.env.TELIUM_WEB ?? join(import.meta.dir, '../../.dev/telium/web'));
const need = createRequire(join(WEB, 'package.json'));

// Browser globals the modules expect.
await import(need.resolve('fake-indexeddb/auto'));
const memory = new Map();
globalThis.localStorage = {
  getItem: (k) => (memory.has(k) ? memory.get(k) : null),
  setItem: (k, v) => memory.set(k, String(v)),
  removeItem: (k) => memory.delete(k),
};
globalThis.navigator ??= { platform: 'rhein-interop' };

const lib = (name) => import(join(WEB, 'src/lib', name));
const { Api } = await lib('api.js');
const { loginOpaque } = await lib('opaque-auth.js');
const vault = await lib('vault.js');
const { Mls } = await lib('mls.js');
const { Trust } = await lib('verify.js');

let me = null;

const ops = {
  /// Signs in exactly as the web client's session store does, then brings up the vault, the
  /// MLS engine and cross-signing the way store/lifecycle.js and store/trust.js do.
  async signin({ base, user, password }) {
    const secure = await loginOpaque(base, user, password, 'web (rhein interop)');
    const session = secure.session;
    const home = new Api({ base, token: session.access_token, domain: session.server_name });
    const ring = await vault.load(home, session.user_id, secure.recoveryWrapper, null);
    const mls = new Mls(home, { userId: session.user_id, deviceId: session.device_id });
    await mls.adoptRecoveryKey(ring);
    await mls.start();
    const trust = new Trust(home, { userId: session.user_id, deviceId: session.device_id, recovery: ring });
    await trust.start(mls.cs, ring.key, await mls.leafKey(), ring.mode);
    mls.verifyClaimedPackage = (u, d, k) => trust.verifyClaimedPackage(u, d, k);
    me = { home, mls, trust, userId: session.user_id, deviceId: session.device_id };
    return { user_id: me.userId, device_id: me.deviceId, identity_key: trust.identityKey ? Buffer.from(trust.identityKey).toString('base64') : null };
  },

  /// `mintConversation`: the server row, the group, the Welcome.
  async create_conversation({ kind = 'dm', members }) {
    const convo = await me.home.createConversation({ kind, members });
    const built = await me.mls.establish(convo.id, [...members, me.userId]);
    if (built.welcome) {
      await me.home.sendFrame(convo.id, { kind: 'welcome', ciphertext: built.welcome, epoch: built.epoch || 0 });
    }
    return { id: convo.id, missing: built.missing, rejected: built.rejected };
  },

  async accept({ id }) {
    await me.home.acceptMessageRequest(id).catch(() => {});
    return {};
  },

  /// Reads everything and syncs it through the engine, self-joining if needed, as openConversation does.
  async sync({ id }) {
    const frames = [];
    let after = 0;
    for (;;) {
      const page = (await me.home.dmHistory(id, { after, limit: 500 }))?.messages || [];
      frames.push(...page);
      if (page.length < 500) break;
      after = page.reduce((m, f) => Math.max(m, f.seq || 0), after);
    }
    await me.mls.sync(id, frames);
    if (!me.mls.groups.has(id) && !(await me.mls.loadGroup(id))) {
      await me.mls.selfJoin(id, frames);
    }
    const texts = frames
      .filter((f) => f.kind === 'application' && me.mls.plaintext.has(f.id))
      .sort((a, b) => a.seq - b.seq)
      .map((f) => ({ id: f.id, sender: f.sender, text: me.mls.currentText?.(id, f.id) ?? me.mls.plaintext.get(f.id).text, attachments: me.mls.plaintext.get(f.id).attachments }));
    const reactions = {};
    for (const t of texts) {
      const r = me.mls.reactionsTo?.(id, t.id);
      if (r && Object.keys(r).length) reactions[t.id] = r;
    }
    return { texts, reactions, in_group: me.mls.groups.has(id), error: me.mls.failed.get(id)?.message || null };
  },

  async send({ id, text }) {
    const frame = await me.mls.send(id, text, []);
    return { id: frame?.id || null };
  },

  async key_packages() {
    return { status: await me.home.keyPackages(), replenished: await me.mls.replenish() };
  },

  async react({ id, target, emoji }) {
    await me.mls.react(id, target, emoji);
    return {};
  },
};

const rl = createInterface({ input: process.stdin });
for await (const line of rl) {
  if (!line.trim()) continue;
  let answer;
  try {
    const request = JSON.parse(line);
    const op = ops[request.op];
    if (!op) throw new Error(`unknown op ${request.op}`);
    answer = { ok: true, ...(await op(request)) };
  } catch (e) {
    answer = { ok: false, error: String(e?.stack || e) };
  }
  process.stdout.write(`${JSON.stringify(answer)}\n`);
}
