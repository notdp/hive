export const meta = {
  name: 'demo-auth-review',
  description: 'spike demo: scout, fan-out review, follow-up ask — hive dialect on the JS engine',
  phases: [{ title: 'Explore' }, { title: 'Review' }],
}

phase('Explore')
const scout = await agent('探索认证模块,列出改动面;产出写 <workspace>/artifacts/scout.md', { name: 'scout' })

phase('Review')
const reviewers = await parallel([
  () => agent(`基于线索复查安全面:\n${scout.summary}`, { name: 'sec' }),
  () => agent(`基于线索复查性能面:\n${scout.summary}`, { name: 'perf' }),
  () => agent(`基于线索复查测试面:\n${scout.summary}`, { name: 'tests', cli: 'codex' }),
])

const ok = reviewers.filter(Boolean)
const [sec] = ok
await sec.ask('第一条发现给出修复建议')
await scout.kill()
log(`review done: ${ok.length}/3`)

return {
  scout: scout.summary,
  reviews: ok.map((r) => ({ name: r.name, summary: r.summary })),
  secFollowup: sec.summary,
}
