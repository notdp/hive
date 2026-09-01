export const meta = {
  name: 'torture',
  description: 'dialect surface torture test: everything the 125-script corpus actually uses',
  phases: [{ title: 'T' }],
}

const checks = {}

// -- determinism poisons --------------------------------------------------
try { Date.now(); checks.dateNow = 'FAIL' } catch (e) { checks.dateNow = 'poisoned' }
try { Math.random(); checks.mathRandom = 'FAIL' } catch (e) { checks.mathRandom = 'poisoned' }
try { new Date(); checks.newDateArgless = 'FAIL' } catch (e) { checks.newDateArgless = 'poisoned' }
checks.newDateWithArg = new Date(0).getTime() === 0 ? 'ok' : 'FAIL'

// -- template composition, spread, ?., ?? ---------------------------------
const COMMON = `common 纪律
多行前缀`
const dims = [{ key: 'a' }, { key: 'b' }]
const prompts = dims.map((d) => `${COMMON}\n维度:${d.key}`)
const merged = { ...dims[0], extra: dims[1]?.missing ?? 'dflt' }
checks.templates =
  prompts[1].includes('维度:b') && prompts[1].includes('多行前缀') && merged.extra === 'dflt'
    ? 'ok'
    : 'FAIL'

// -- hand-rolled retry over a flaky spawn (tryAgent pattern) --------------
async function tryAgent(prompt, opts) {
  for (let i = 1; i <= 3; i++) {
    try { return await agent(prompt, opts) } catch (e) { log(`${opts.name} attempt ${i} failed: ${e.message}`) }
  }
  return null
}
const flaky = await tryAgent('flaky task', { name: 'flaky-one' })
checks.retryLoop = flaky && flaky.summary.length > 0 ? 'ok' : 'FAIL'

// -- real Promise .then chains --------------------------------------------
checks.thenChain = await agent('chain start', { name: 'chain' })
  .then((m) => m.ask('chain follow-up'))
  .then((m) => (m.summary.length > 0 ? 'ok' : 'FAIL'))

// -- parallel: failed branches resolve to null, never reject --------------
const pr = await parallel([
  () => agent('good branch', { name: 'p-ok' }),
  () => { throw new Error('sync boom') },
  async () => { throw new Error('async boom') },
])
checks.parallelNulls =
  pr.length === 3 && pr[0] !== null && pr[1] === null && pr[2] === null ? 'ok' : 'FAIL'

// -- pipeline: throwing stage drops item, later stages see (prev,item,i) --
const pl = await pipeline(
  [1, 2, 3],
  async (n) => { if (n === 2) throw new Error('drop me'); return n * 10 },
  async (prev, item, i) => `${item}:${prev}:${i}`,
)
checks.pipelineDrop =
  JSON.stringify(pl) === JSON.stringify(['1:10:0', null, '3:30:2']) ? 'ok' : 'FAIL'

// -- while / loop-until-dry shape -----------------------------------------
let rounds = 0
let dry = 0
while (dry < 2) {
  rounds++
  if (rounds > 1) dry++
  if (rounds > 10) break
}
checks.whileLoop = rounds === 3 ? 'ok' : 'FAIL'

// -- dead-member guard -----------------------------------------------------
const victim = await agent('to be killed', { name: 'victim' })
await victim.kill()
try { await victim.ask('speak'); checks.deadGuard = 'FAIL' } catch (e) { checks.deadGuard = 'ok' }

// -- builtins the corpus leans on -----------------------------------------
checks.builtins =
  [...new Set([1, 1, 2])].flatMap((x) => [x]).reduce((a, b) => a + b, 0) === 3 &&
  JSON.parse(JSON.stringify({ deep: { v: 1 } })).deep.v === 1
    ? 'ok'
    : 'FAIL'

log('torture done')
return checks
