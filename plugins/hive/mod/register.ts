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

const send = async ($: EngineInterface, ep: Endpoint, body: Record<string, unknown>): Promise<number> => {
  const res = await $.http.fetch(`http://127.0.0.1:${ep.port}/v1/hooks`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', authorization: `Bearer ${ep.token}` },
    body: JSON.stringify({ ...body, sessionId, teamCreatedAt: ep.teamCreatedAt }),
  })
  return res.status
}

// One event to the hived. The sequence number is taken before the first
// await, in the engine's event order. An unbound session looks its team up
// at every event (the roster row may land after the engine's own start);
// a bound one keeps its endpoint until a post fails, then looks up once
// more. Never throws.
const report = async ($: EngineInterface, event: string, fields: Record<string, unknown>) => {
  const mine = ++seq
  try {
    if (!sessionId) sessionId = await $.session.id()
    if (!endpoint) endpoint = await locate($, sessionId)
    if (!endpoint) return
    const eventId = `${sessionId}:${event}:${await $.clock.now()}:${Math.random().toString(16).slice(2, 10)}`
    const body = { event, eventId, epoch, seq: mine, at: await $.clock.now(), ...fields }
    let status = 0
    try { status = await send($, endpoint, body) } catch { status = 0 }
    if (status === 200) return
    endpoint = await locate($, sessionId)
    if (!endpoint) return
    try { if (await send($, endpoint, body) !== 200) endpoint = null } catch { endpoint = null }
  } catch {
    // fail-open by design: the hived's other observations stand
  }
}

// Bounded: a slow hived must not hold the turn.
const bounded = ($: EngineInterface, work: Promise<void>) =>
  Promise.race([work, $.clock.sleep(POST_BUDGET_MS)])

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
}
