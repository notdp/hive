// Hive hooks module: reports this Claude session's turn boundaries to the
// hived of the team whose roster names its session id.
//
// Discovery is read-only and local: the roster under $HIVE_HOME/teams names
// the workspace, and <workspace>/run/hooks-endpoint.json names the hived's
// loopback port and bearer token. Nothing here decides membership; a session
// no roster names posts nothing. Every hook passes the event on unchanged.
//
// Every report carries this module instance's epoch and a sequence number
// taken in event order, so the hived orders reports by the engine's order,
// not by arrival: a report that ran past its budget and lands after a later
// one is stale there, and a new engine process (a wake, a claimed spare) is
// a new epoch that opens with session.start.
import type { EngineInterface, Register } from 'claude-code'

type Endpoint = { port: number; token: string; teamCreatedAt: string }

const POST_BUDGET_MS = 1500
// session.end runs under the engine's own end step, 1.5 s by default.
const END_BUDGET_MS = 1000

let endpoint: Endpoint | null = null
let sessionId: string | null = null
const epoch = `${Date.now().toString(16)}-${Math.random().toString(16).slice(2, 10)}`
let seq = 0

const hiveHome = async ($: EngineInterface) =>
  (await $.env.get('HIVE_HOME')) ?? `${await $.env.get('HOME')}/.hive`

const readJson = async ($: EngineInterface, path: string): Promise<any | null> => {
  try { return JSON.parse(await $.fs.read(path)) } catch { return null }
}

// Whether a roster row names this session: a joined session's row carries
// the session id itself; a bg member's row carries its jobId, the first
// hex group of the id the engine minted. Discovery only — the hived checks
// the row against the engine registry before it admits a report.
const namesSession = (row: any, sid: string) =>
  row?.cli === 'claude' && typeof row.sessionId === 'string' && row.sessionId !== '' &&
  (row.sessionId === sid || sid.startsWith(`${row.sessionId}-`))

// The workspace whose team.json roster names this session, else null.
const locate = async ($: EngineInterface, sid: string): Promise<Endpoint | null> => {
  const teams = `${await hiveHome($)}/teams`
  let entries: readonly { name: string; kind: string }[] = []
  try { entries = await $.fs.list(teams) } catch { return null }
  for (const entry of entries) {
    if (entry.kind !== 'dir') continue
    const team = await readJson($, `${teams}/${entry.name}/team.json`)
    const members: any[] = Array.isArray(team?.members) ? team.members : []
    if (!members.some((m) => namesSession(m, sid))) continue
    const workspace: string = typeof team.workspace === 'string' && team.workspace ? team.workspace : `${teams}/${entry.name}`
    const ep = await readJson($, `${workspace}/run/hooks-endpoint.json`)
    if (!ep || typeof ep.port !== 'number' || typeof ep.token !== 'string') return null
    return { port: ep.port, token: ep.token, teamCreatedAt: String(ep.teamCreatedAt ?? '') }
  }
  return null
}

type Sent = { status: number; unregistered: boolean }

const send = async ($: EngineInterface, ep: Endpoint, body: Record<string, unknown>): Promise<Sent> => {
  const res = await $.http.fetch(`http://127.0.0.1:${ep.port}/v1/hooks`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', authorization: `Bearer ${ep.token}` },
    body: JSON.stringify({ ...body, sessionId, teamCreatedAt: ep.teamCreatedAt }),
  })
  let unregistered = false
  try { unregistered = res.status === 200 && JSON.parse(res.text)?.unregistered === true } catch { unregistered = false }
  return { status: res.status, unregistered }
}

const envelope = async ($: EngineInterface, event: string, fields: Record<string, unknown>, mine: number) => ({
  event,
  eventId: `${sessionId}:${event}:${mine}:${Math.random().toString(16).slice(2, 10)}`,
  epoch,
  seq: mine,
  at: await $.clock.now(),
  ...fields,
})

// One event to the hived. The sequence number is taken before the first
// await, in the engine's event order. An unbound session looks its team up
// at every event (the roster row may land after the engine's own start);
// a bound one keeps its endpoint until a post fails, then looks up once
// more. A hived that does not know this epoch (its session.start was lost,
// or the hived is a new generation) answers `unregistered`: the module
// registers with a session.start at seq 0 (no watermark moves) and sends
// the event again exactly as it was, same seq and eventId, so a resend
// never overtakes an event that landed meanwhile. Never throws.
const report = async ($: EngineInterface, event: string, fields: Record<string, unknown>) => {
  const mine = ++seq
  try {
    if (!sessionId) sessionId = await $.session.id()
    if (!endpoint) endpoint = await locate($, sessionId)
    if (!endpoint) return
    const body = await envelope($, event, fields, mine)
    let sent: Sent = { status: 0, unregistered: false }
    try { sent = await send($, endpoint, body) } catch { sent = { status: 0, unregistered: false } }
    if (sent.status !== 200) {
      endpoint = await locate($, sessionId)
      if (!endpoint) return
      try { sent = await send($, endpoint, body) } catch { endpoint = null; return }
      if (sent.status !== 200) { endpoint = null; return }
    }
    if (!sent.unregistered) return
    const register = await envelope($, 'session.start', {}, 0)
    try { if ((await send($, endpoint, register)).status !== 200) return } catch { return }
    try { await send($, endpoint, body) } catch { endpoint = null }
  } catch {
    // fail-open by design: the hived's other observations stand
  }
}

// A hive frame as the inbox lane delivers it to a session: the receiver's
// own peer-card tag around one <HIVE> envelope (claude_sessions::
// peer_card_envelope), or the bare envelope. Anything else is not hive's.
const hiveEnvelopeOf = (text: string): string | null => {
  const m = /^<cross-session-message\b[^>]*>\n([\s\S]*)\n<\/cross-session-message>$/.exec(text.trim())
  const inner = (m ? m[1] : text).trim()
  return inner.startsWith('<HIVE') && inner.endsWith('</HIVE>') ? inner : null
}

// Bounded: a slow hived must not hold the turn.
const bounded = ($: EngineInterface, work: Promise<void>, ms = POST_BUDGET_MS) =>
  Promise.race([work, $.clock.sleep(ms)])

export const register: Register = (on) => {
  on('session.start', async ($, e, next) => {
    await bounded($, report($, 'session.start', { cwd: e.cwd, surface: e.surface, isInteractive: e.isInteractive }))
    return next(e)
  })
  on('turn.start', async ($, e, next) => {
    await bounded($, report($, 'turn.start', { turnId: e.turnId }))
    return next(e)
  })
  on('turn.complete', async ($, e, next) => {
    await bounded($, report($, 'turn.complete', {
      turnId: e.turnId, reason: e.reason, isAborted: e.isAborted, durationMs: e.durationMs,
      agentId: e.agentId ?? null, usage: e.usage ?? null,
    }))
    return next(e)
  })
  // The engine leaving (exit, /clear, resume, logout, a signal; a kill -9
  // raises nothing): the hived closes any open turn at once instead of
  // waiting the report out.
  on('session.end', async ($, e, next) => {
    await bounded($, report($, 'session.end', { reason: e.reason, resumeId: e.resume?.id ?? null }), END_BUDGET_MS)
    return next(e)
  })
  // The relay lane: a hive frame arriving on this session's inbox is taken
  // here and submitted again as the plugin's own prompt, so the model reads
  // it under the plugin's short wrapper instead of the peer banner and its
  // safety paragraph (about 180 characters of wrapper instead of 580,
  // measured on 2.1.278). Only for a session some roster names: an outside
  // session keeps the peer frame, whose `from` is what its own reply goes
  // to. The frame is consumed only once the submission is in — a refused
  // or dropped submission, or a throw, passes the frame on unchanged, so a
  // message is never lost to the relay. A plugin's prompt runs once the
  // session is idle, as its own turn; nothing folds into a running turn.
  on('session.receive', async ($, e, next) => {
    if (e.origin?.kind !== 'peer') return next(e)
    const inner = hiveEnvelopeOf(e.text)
    if (!inner) return next(e)
    try {
      if (!sessionId) sessionId = await $.session.id()
      if (!endpoint) endpoint = await locate($, sessionId)
      if (!endpoint) return next(e)
      const r = await $.prompt.submit({ text: inner })
      if ('drop' in r && r.drop) return next(e)
      return { consumed: 'hive: relayed as the plugin\'s own prompt' }
    } catch {
      return next(e)
    }
  })
}
