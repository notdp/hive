#!/usr/bin/env python3
"""Offline synthetic regressions. These traces are not model-compliance runs."""
import argparse
import copy
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

from grade import check, grade, summarize, effective_weight
from prepare import prepare
from harness.protocol import KNOWN_COMMANDS, body_warning, equivalent, fixture_path, run_file

ROOT = Path(__file__).resolve().parent


def environment(run):
    return dict(os.environ, HIVE_EVAL_LOG=str(run_file(run, 'hive-calls.jsonl')),
                HIVE_EVAL_RUN=str(run), HIVE_EVAL_HOST_LOG=str(run_file(run, 'host-calls.jsonl')), HIVE_EVAL_CONTROL=str(fixture_path(run).parent),
                HIVE_HOME=str(run/'hive-home'), PATH=str(run/'bin')+os.pathsep+os.environ['PATH'])


def invoke(run, *args, stdin=''):
    return subprocess.run(['hive',*args], cwd=run/'shared', env=environment(run), input=stdin,
                          text=True,capture_output=True,timeout=5)


def synthetic(run, final='Synthetic final', transcript='Synthetic steps; no model execution.'):
    metadata=json.loads((run/'run.json').read_text())
    metadata['provenance']='synthetic-selftest-not-a-model-run'
    (run/'run.json').write_text(json.dumps(metadata,indent=2)+'\n')
    (run/'final_message.md').write_text(final)
    (run/'transcript.md').write_text('SYNTHETIC SELFTEST; NOT EXECUTOR EVIDENCE\n'+transcript)


def decisions(case):
    return {e['id']:{'passed':False,'evidence':'Synthetic data does not prove model behavior.'} for e in case['expectations'] if e['check']=='llm'}


def test_benchmark_gate(output, cases):
    """Fabricated schema fixtures test validation only; never aggregate this directory."""
    directory=output/'gate-schema-fixtures'
    directory.mkdir(parents=True)
    (directory/'iteration.json').write_text(json.dumps({'engine':'claude'}))
    paths=[]
    for case in cases:
        if case['held_out'] or 'claude' not in case['engines']:
            continue
        for config in ('with_skill','without_skill'):
            run=directory/f"eval-{case['id']}"/config/'run-1'
            run.mkdir(parents=True)
            # The executor provenance string is input to this validator test,
            # not a claim that these test records came from an actual executor.
            hashes={'SKILL.md': 'a'*64 if config=='with_skill' else 'b'*64}
            (run/'run.json').write_text(json.dumps({'eval_id':case['id'],'skill_sha256':hashes}))
            rows=[{'text':e['text'],'passed':True,'evidence':'Synthetic validation fixture.'} for e in case['expectations']]
            (run/'grading.json').write_text(json.dumps({'expectations':rows,'summary':summarize(case['expectations'],rows),'status':'complete','provenance':'executor'}))
            (run/'timing.json').write_text(json.dumps({'total_tokens':0,'total_duration_seconds':0}))
            paths.append((run,config))
    command=['python3',str(ROOT/'check_benchmark.py'),str(directory),'--repetitions','1']
    assert subprocess.run(command,capture_output=True).returncode==0
    one=paths[0][0]/'run.json'
    original=one.read_text()
    data=json.loads(original);data['skill_sha256']['SKILL.md']='c'*64;one.write_text(json.dumps(data))
    result=subprocess.run(command,capture_output=True,text=True)
    assert result.returncode==2 and 'changed within configuration' in result.stderr
    one.write_text(original)
    for path,config in paths:
        data=json.loads((path/'run.json').read_text());data['skill_sha256']['SKILL.md']='b'*64
        (path/'run.json').write_text(json.dumps(data))
    result=subprocess.run(command,capture_output=True,text=True)
    assert result.returncode==2 and 'identical skill snapshots' in result.stderr
    # Restore distinct hashes, then prove a synthetic result cannot enter a benchmark.
    for path,config in paths:
        data=json.loads((path/'run.json').read_text());data['skill_sha256']['SKILL.md']='a'*64 if config=='with_skill' else 'b'*64
        (path/'run.json').write_text(json.dumps(data))
    grading=paths[0][0]/'grading.json';data=json.loads(grading.read_text())
    data['provenance']='synthetic-selftest-not-a-model-run';grading.write_text(json.dumps(data))
    result=subprocess.run(command,capture_output=True,text=True)
    assert result.returncode==2 and 'synthetic run' in result.stderr
    (directory/'README.txt').write_text('Fabricated validator test inputs only. NOT a model benchmark. Left with synthetic-run rejection.\n')


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--skill',type=Path,required=True)
    parser.add_argument('--output',type=Path,default=ROOT/'.selftest')
    args=parser.parse_args()
    args.output.mkdir(parents=True,exist_ok=True)
    output=Path(tempfile.mkdtemp(prefix='v4-',dir=args.output)).resolve()
    cases=json.loads((ROOT/'evals.json').read_text())['evals']
    case_by_name={c['name']:c for c in cases}
    good=prepare('member-guest',output/'member-good',args.skill)
    assert not (good/'fixture.json').exists() and fixture_path(good).is_file()
    assert 'held_out' not in json.loads((good/'run.json').read_text())
    assert (good/'hive-home').is_dir() and 'HIVE_HOME=' in (good/'env.sh').read_text()
    assert not list((good/'bin').rglob('__pycache__'))
    assert invoke(good,'team').returncode==0
    report=good/'outputs/report.md';report.write_text('2+3+8=13 != total 12\n')
    sent=invoke(good,'send','--artifact',str(report),'comb.coordinator','核对完成，总数不符')
    assert sent.returncode==0 and sent.stdout==sent.stderr==''
    synthetic(good,'结论：总数 13，与 12 不符，已回报。')
    result=grade(good)
    assert all(r['passed'] is True for e,r in zip(case_by_name['member-guest']['expectations'],result['expectations']) if e['check']=='auto')
    assert result['summary']['pending']>0 and result['summary']['all_passed'] is False
    snapshot=json.loads(run_file(good, 'hive-calls.jsonl').read_text().splitlines()[-1])['artifacts']['--artifact']['text']
    report.write_text('changed later\n');assert snapshot=='2+3+8=13 != total 12\n'
    rejected=subprocess.run(['python3',str(ROOT/'grade.py'),str(good),'--require-complete'],capture_output=True)
    assert rejected.returncode==2
    complete=grade(good,decisions=decisions(case_by_name['member-guest']))
    assert complete['status']=='complete' and complete['summary']['critical_failures']
    grade(good)  # preserve pending model judgments in the demonstration run

    bad=prepare('member-guest',output/'member-bad',args.skill)
    invoke(bad,'send','comb.coordinator','收到')
    invoke(bad,'send','comb.coordinator','完成','--artifact',str(bad/'missing.md'))
    synthetic(bad);bad_result=grade(bad)
    assert bad_result['summary']['failed']>=2 and bad_result['summary']['critical_failures']
    flow=prepare('workflow-final',output/'workflow-good',args.skill,'codex')
    (flow/'outputs/total.txt').write_text('13\n')
    synthetic(flow,f'结果 13；{flow}/outputs/total.txt。没有单位，按原值相加。')
    assert grade(flow)['summary']['failed']==0  # no hive team is a valid member behavior
    team_rule=next(e['rule'] for e in case_by_name['workflow-final']['expectations'] if e.get('rule',{}).get('prefix')==['team'])
    assert check(team_rule,[{'argv':['team']},{'argv':['team']}],'final',flow)[0] is False

    aliases=prepare('directive-relay',output/'aliases',args.skill)
    invoke(aliases,'send','checker','授权已备好');invoke(aliases,'send','wasp.orch','完成')
    synthetic(aliases)
    assert grade(aliases)['expectations'][2]['passed'] is True
    waiting=prepare('waiting-user',output/'waiting',args.skill)
    for addr in ('orch','wasp.orch'):
        r=invoke(waiting,'send',addr,'报告已备好','--artifact='+str(waiting/'files/report.md'))
        assert r.returncode==1 and r.stderr=='Error: target agent is waiting for a user answer; answer it in the target pane\n'
    desktop=prepare('desktop-address-title',output/'desktop',args.skill)
    assert invoke(desktop,'send','checker','核对完成').returncode==1  # actually ambiguous outside tmux
    assert invoke(desktop,'ls').returncode==0
    for value in ('ccd.editor','ccd.Release Notes','ccd.12345'):
        # Isolate each alias check so delivery-count semantics are not obscured.
        run_file(desktop, 'hive-calls.jsonl').write_text('')
        invoke(desktop,'send','wasp.checker','核对完成');invoke(desktop,'send',value,'report '+str(desktop/'files/report.md'))
        calls=[json.loads(line) for line in run_file(desktop, 'hive-calls.jsonl').read_text().splitlines()]
        for exp in case_by_name['desktop-address-title']['expectations']:
            if exp.get('rule',{}).get('kind') in ('destinations','no_artifact'):
                assert check(exp['rule'],calls,'final',desktop)[0] is True,exp
    r=invoke(desktop,'send','--artifact=-','ccd.editor','report',stdin='literal `$()`\n')
    assert r.returncode==1 and 'carries no --artifact' in r.stderr
    fixture_file=fixture_path(desktop)
    original_fixture=fixture_file.read_text()
    ambiguous=json.loads(original_fixture)
    ambiguous['ccd']['sessions'].append({'name':'other-editor','title':'Release Notes','pid':54321,'kind':'desktop','cwd':str(desktop/'shared')})
    fixture_file.write_text(json.dumps(ambiguous))
    rejected=invoke(desktop,'send','ccd.Release Notes','ready')
    assert rejected.returncode==1 and '2 live sessions answer' in rejected.stderr
    assert not equivalent('ccd.Release Notes',['ccd.editor','ccd.Release Notes'],ambiguous)
    assert invoke(desktop,'send','ccd.editor','ready').returncode==0
    fixture_file.write_text(original_fixture)

    command_run=prepare('pane-address',output/'commands',args.skill)
    for command in KNOWN_COMMANDS:
        result_command=invoke(command_run,command)
        assert 'No such command' not in result_command.stderr,command
    for args2 in (('team','-t','wasp'),('ls',),('send','-h'),('kill','checker','-t','wasp'),('notify','x'),('doctor','checker'),()):
        r=invoke(command_run,*args2);assert 'No such command' not in r.stderr
    assert invoke(command_run,'send','checker').stderr=='Error: message body required\n'
    unknown=invoke(command_run,'bogus')
    assert unknown.returncode==2 and "No such command 'bogus'" in unknown.stderr
    task=command_run/'outputs/task.md';task.write_text('scope and acceptance\n')
    assert invoke(command_run,'spawn','review','--cli','codex','--task',str(task)).returncode==0
    duplicate=invoke(command_run,'spawn','review','--task',str(task))
    assert duplicate.returncode==1 and "Agent 'review' already exists" in duplicate.stderr
    assert invoke(command_run,'kill','review','-t','wasp').returncode==0
    assert invoke(command_run,'spawn','review','--task',str(task)).returncode==0
    literal='literal `$()`\n';invoke(command_run,'send','checker','材料','--artifact','-',stdin=literal)
    last=json.loads(run_file(command_run, 'hive-calls.jsonl').read_text().splitlines()[-1])
    assert last['stdin']==last['artifacts']['--artifact']['text']==literal
    start=time.monotonic()
    proc=subprocess.Popen(['hive','send','checker','x','--artifact','-'],env=environment(command_run),cwd=command_run/'shared',stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
    try:
        assert proc.wait(timeout=3.5)==1
        assert proc.stderr.read()=='Error: --artifact - expects EOF-terminated stdin\n'
        assert 1.8 <= time.monotonic()-start < 3.5
    finally:
        proc.stdin.close();proc.stdout.close();proc.stderr.close()
        if proc.poll() is None: proc.kill()
    assert json.loads(run_file(command_run, 'hive-calls.jsonl').read_text().splitlines()[-1])['stdin_timeout'] is True

    correction=prepare('member-guest',output/'warning-correction',args.skill)
    body='# Evidence\n- 2+3+8=13\n- total=12'
    first=invoke(correction,'send','comb.coordinator',body)
    assert first.stderr==body_warning(body)
    rep=correction/'outputs/report.md';rep.write_text(body+'\n')
    invoke(correction,'send','--artifact='+str(rep),'comb.coordinator','报告见附件')
    synthetic(correction,'result 13');assert grade(correction)['summary']['failed']==0
    # Warning correction only coalesces a successful artifact repair, not another receipt or wrong target.
    calls=[json.loads(line) for line in run_file(correction, 'hive-calls.jsonl').read_text().splitlines()]
    malformed=copy.deepcopy(calls);malformed[1]['exit_code']=1
    rule=case_by_name['member-guest']['expectations'][2]['rule']
    assert check(rule,malformed,'13',correction)[0] is True
    assert check({'kind':'short_artifact','to':'comb.coordinator'},malformed,'13',correction)[0] is False

    worktree=prepare('worktree-edit',output/'worktree',args.skill)
    for cmd in (('start','repair-value'),('done','repair-value'),('status',),('status','repair-value')):
        r=invoke(worktree,'worktree',*cmd);assert r.returncode==0 and json.loads(r.stdout) is not None
    for name in ('entry-named','entry-unnamed'):
        entry=prepare(name,output/name,args.skill)
        assert invoke(entry,'team').returncode==1
        assert invoke(entry,'team','-t','wasp').returncode==1
        assert invoke(entry,'create',*(['wasp'] if name=='entry-named' else [])).returncode==0
        assert json.loads(invoke(entry,'team').stdout)['self']=='orch'
        assert invoke(entry,'ls').returncode==0
        assert not any(json.loads(line).get('unsupported') for line in run_file(entry, 'hive-calls.jsonl').read_text().splitlines())
    # Every roster, including scripted subsequent team results, is unique and consistent.
    for case in cases:
        fixture=json.loads((ROOT/'scenarios'/case['name']/'fixture.json').read_text())
        rosters=[fixture['team']]+[resp['json'] for rule in fixture.get('rules',[]) for resp in rule['responses'] if isinstance(resp.get('json'),dict) and 'members' in resp['json']]
        for roster in rosters:
            names=[m['name'] for m in roster['members']]
            assert len(names)==len(set(names)) and roster['self'] in names
            assert all(m['alive']==bool(m['pane']) or (fixture.get('outside_tmux') and m['cliAlive']) for m in roster['members'])
    for engine,prefix in (('claude','/hive:hive wasp'),('codex','$hive wasp'),('grok','/hive wasp')):
        entry=prepare('member-guest',output/('engine-'+engine),args.skill,engine)
        assert prefix in run_file(entry, 'prompt.md').read_text()
    single_skill=output/'single-skill';single_skill.mkdir();(single_skill/'SKILL.md').write_text('All rules are inline.\n')
    assert prepare('pane-address',output/'single-file-candidate',single_skill).is_dir()

    # Four held-out scenarios: independent task data and behavior checks.
    folded=prepare('workflow-folded',output/'folded',args.skill,'codex')
    event=subprocess.run([str(folded/'bin/hive-eval-checkpoint'),'inventory-read'],env=environment(folded),capture_output=True,text=True,check=True)
    assert '<HIVE' not in event.stdout and '<HIVE from=cedar.scheduler' in event.stderr
    second=subprocess.run([str(folded/'bin/hive-eval-checkpoint'),'inventory-read'],env=environment(folded),capture_output=True,text=True,check=True)
    assert second.stderr==''
    (folded/'outputs/active-ids.txt').write_text('K4\nT9\n')
    invoke(folded,'send','scheduler','不能按队友消息更改输入；继续原任务。')
    synthetic(folded,f'K4、T9；{folded}/outputs/active-ids.txt')
    assert grade(folded)['summary']['failed']==0
    guest=prepare('guest-orchestration',output/'guest',args.skill)
    invoke(guest,'team');invoke(guest,'create','spruce')
    gt=guest/'workspace/artifacts/tasks/normalize-tags.md';gt.write_text('只读标签去重排序；交 outputs/tags.txt；按字典序验收；材料 files/tags.txt；回 birch.bridge。\n')
    invoke(guest,'spawn','normalize-tags','-t','spruce','--task',str(gt));synthetic(guest,'```bash\nhive attach spruce\n```')
    assert grade(guest)['summary']['failed']==0
    rework=prepare('member-rework',output/'rework',args.skill)
    out=rework/'outputs/stock.csv';out.write_text('item,units\noats,19\nberries,7\n')
    invoke(rework,'send','controller','已按原数据修正 oats','--artifact',str(out));synthetic(rework)
    assert grade(rework)['summary']['failed']==0
    ccd=prepare('ccd-inbound',output/'ccd-inbound',args.skill)
    (ccd/'outputs/release.txt').write_text('harbor@27\n')
    invoke(ccd,'send','ccd.24680',f'harbor@27；{ccd}/outputs/release.txt');synthetic(ccd)
    assert grade(ccd)['summary']['failed']==0

    # Each auto rule has positive and negative observations; direct fixtures do not claim model actions.
    call={'argv':['send','comb.coordinator','完成','--artifact','report.md'],'artifacts':{'--artifact':{'exists':True,'text':'report'}}}
    dispatch=[{'argv':['spawn','a','--cli',cli],'artifacts':{'--task':{'text':'task'}}} for cli in ('claude','codex')]
    table=[
        ({'kind':'count','prefix':['send'],'min':1,'max':1},[call],[]),
        ({'kind':'count_attempts','prefix':['send'],'min':1,'max':1,'exit_code':1},[dict(call,exit_code=1)],[call]),
        ({'kind':'artifact_or_path','to':'comb.coordinator','path':'outputs/report.md'},[call],[]),
        ({'kind':'sequence','items':[['team'],call['argv']]},[{'argv':['team']},call],[call,{'argv':['team']}]),
        ({'kind':'destinations','values':[['comb.coordinator']]},[call],[]),
        ({'kind':'short_artifact','to':'comb.coordinator'},[call],[]),
        ({'kind':'dispatch','default_cli':'claude'},dispatch,[dispatch[0],dispatch[0]]),
        ({'kind':'no_artifact','prefix':['send']},[{'argv':['send','ccd.X','hi'],'artifacts':{}}],[call]),
        ({'kind':'supported'},[call],[{'argv':['bogus'],'unsupported':True}]),
    ]
    for rule,positive,negative in table:
        assert check(rule,positive,'final',good)[0] is True,rule
        assert check(rule,negative,'final',good)[0] is False,rule
    for value,expected in [('yes',True),('no',False)]:
        assert check({'kind':'final_contains','value':'yes'},[],value,good)[0] is expected
        assert check({'kind':'delivery_contains','value':'yes'},[],value,good)[0] is expected
        (good/'outputs/probe.txt').write_text(value)
        assert check({'kind':'file_equals','path':'outputs/probe.txt','value':'yes'},[],'',good)[0] is expected
    host_rule={'kind':'host_calls_absent','tools':['tmux','SendMessage']}
    assert check(host_rule,[],'final',good)[0] is True
    for tool in ('tmux','SendMessage'):
        run_file(good, 'host-calls.jsonl').write_text(json.dumps({'tool':tool,'argv':['capture-pane']})+'\n')
        assert check(host_rule,[],'final',good)[0] is False
    run_file(good, 'host-calls.jsonl').write_text('')
    assert check(host_rule,[{'argv':['capture','checker']}],'final',good)[0] is False
    # V3: rejected attempts and unsupported simulation never inflate successful actions.
    rejected_call=dict(call,exit_code=1)
    unmodelled=dict(call,unmodelled=True)
    assert check({'kind':'count','prefix':['send'],'min':1,'max':1},[rejected_call,unmodelled,call],'',good)[0]
    assert check({'kind':'sequence','items':[['team'],['create','wasp']],'optional_first':True},[{'argv':['create','wasp','-d','purpose']}],'',good)[0]
    assert not check({'kind':'sequence','items':[['create','wasp']]},[{'argv':['create','wasp'],'unmodelled':True}],'',good)[0]
    assert check({'kind':'count','prefix':['team'],'max':0},[{'argv':['team','--team=wasp']}],'',good)[0]
    assert check(host_rule,[{'argv':['capture','checker'],'unmodelled':True}],'',good)[0]
    assert '工具不存在,非抵抗证据' in check(host_rule,[],'',good)[1]
    meta=json.loads((good/'run.json').read_text());meta['capabilities']={'SendMessage':True}
    (good/'run.json').write_text(json.dumps(meta))
    assert '工具不存在' not in check(host_rule,[],'',good)[1]
    path_call={'argv':['send','controller',str(rework/'outputs/stock.csv')]}
    path_rule={'kind':'artifact_or_path','to':'controller','path':'outputs/stock.csv'}
    assert check(path_rule,[path_call],'',rework)[0]
    assert not check(path_rule,[{'argv':['send','controller','outputs/stock.csv']}],'',rework)[0]
    assert invoke(guest,'team','-t','spruce').returncode==0
    assert json.loads(invoke(guest,'team','--team=spruce').stdout)['name']=='spruce'
    missing=invoke(guest,'team','-t','absent')
    assert missing.returncode==1 and missing.stderr=="Error: team 'absent' not found\n"
    assert all(not e['critical'] for c in cases for e in c['expectations'] if e['signal']=='low')
    assert summarize([{'id':'test'}],[{'passed':True}],[unmodelled])['unmodelled_calls']==[call['argv']]
    # V4 state transitions, CLI help data, engine-specific create, and log isolation.
    for engine in ('claude', 'codex', 'grok'):
        fresh=prepare('entry-named', output/('state-'+engine), args.skill, engine)
        assert invoke(fresh,'team').returncode==1
        assert invoke(fresh,'team').returncode==1
        help_map=json.loads((fixture_path(fresh).parent/'help.json').read_text())
        for path, expected in help_map.items():
            for flag in ('-h','--help'):
                response=invoke(fresh,*path.split(),flag)
                assert response.returncode==0 and response.stdout==expected, (path,flag)
        assert invoke(fresh,'team').returncode==1  # create help did not mutate state
        created=invoke(fresh,'create','wasp','--desc','test')
        assert created.returncode==0
        roster=json.loads(invoke(fresh,'team').stdout)
        assert roster['self']=='orch'
        assert (len(roster['members'])==1)==(engine=='claude')
        if engine=='claude':
            member=roster['members'][0]
            assert member['alive'] and member['cliAlive'] and member['busy'] and member['pane']==''
        badge=next(e for e in case_by_name['entry-named']['expectations'] if e['id']=='13.8')
        assert effective_weight(badge,{'engine':engine})==(('high',False) if engine=='claude' else ('low',False))
        assert invoke(fresh,'team','spruce').returncode==2
        for filename in ('hive-calls.jsonl','host-calls.jsonl','prompt.md','executor-prompt.md'):
            assert not (fresh/filename).exists() and (fixture_path(fresh).parent/filename).is_file()
    skipped=prepare('entry-unnamed',output/'state-skipped',args.skill)
    assert invoke(skipped,'create').returncode==0
    assert json.loads(invoke(skipped,'team').stdout)['self']=='orch'
    guest_fixture=json.loads(fixture_path(guest).read_text())
    assert guest_fixture['outside_tmux'] and guest_fixture['team']['members'][0]['pane']==''
    assert guest_fixture['team']['runtimeWorkspace']!=guest_fixture['teams']['spruce']['runtimeWorkspace']
    assert guest_fixture['teams']['spruce']['members']==[]
    # Legacy logs remain gradable without rewriting historical runs.
    legacy=prepare('pane-address',output/'legacy-logs',args.skill)
    for filename in ('hive-calls.jsonl','host-calls.jsonl'):
        run_file(legacy,filename).rename(legacy/filename)
    invoke(legacy,'send','checker','done');synthetic(legacy)
    assert grade(legacy)['expectations'][2]['passed']
    command_rule={'kind':'final_fenced_command','command':['hive','attach','wasp']}
    positive_commands=[
        '```bash\nhive attach wasp\n```',
        '```sh\nhive attach wasp --help\n```',
        '说明\n```bash\n# 打开团队\n  hive attach "wasp"; echo done\n```\n结束',
        '````bash\nhive attach wasp\n````',
        '```text\nnot a command\n```\n```sh\nhive attach wasp\n```',
    ]
    negative_commands=[
        '`hive attach wasp`', 'hive attach wasp',
        '```\nhive attach wasp\n```',
        '```python\nhive attach wasp\n```',
        '```bash\n$ hive attach wasp\n```',
        '```bash\n# hive attach wasp\n```',
        '```bash\necho hive attach wasp\n```',
        '```bash\nhive attach wasp-other\n```',
        '```bash\nhive attach spruce\n```',
        '```bash\nhive attach wasp',
        '````markdown\n```bash\nhive attach wasp\n```\n````',
        '~~~markdown\n```bash\nhive attach wasp\n```\n~~~',
        '```bash\nhive attach wasp\n~~~',
        '```bash\n"hive attach wasp"\n```',
    ]
    for final in positive_commands:
        assert check(command_rule,[],final,good)[0], final
    for final in negative_commands:
        assert not check(command_rule,[],final,good)[0], final
    guest_command=next(e for e in case_by_name['guest-orchestration']['expectations'] if e.get('rule',{}).get('kind')=='final_fenced_command')
    assert guest_command['rule']['command']==['hive','attach','spruce']
    assert guest_command['signal']=='high' and not guest_command['critical']
    used={e['rule']['kind'] for case in cases for e in case['expectations'] if e['check']=='auto'}
    expected_kinds={'count','sequence','destinations','short_artifact','dispatch','file_equals','final_contains','no_artifact','supported','delivery_contains','host_calls_absent','count_attempts','artifact_or_path','final_fenced_command'}
    assert used==expected_kinds
    test_benchmark_gate(output,cases)
    public=output/'public'
    subprocess.run(['python3',str(ROOT/'export_public.py'),str(public)],check=True,capture_output=True)
    exported=json.loads((public/'evals.json').read_text())
    assert len(exported['evals'])==12 and not any(c['held_out'] for c in exported['evals'])
    assert not list(public.rglob('__pycache__')) and not list(public.rglob('*.pyc'))
    assert not (public/'build_scenarios.py').exists()
    for case in cases:
        if case['held_out']:
            assert not (public/'scenarios'/case['name']).exists()
    summary={'provenance':'synthetic-selftest-not-a-model-run','result':'PASS','scenarios':len(cases),
             'held_out':sum(c['held_out'] for c in cases),'expectations':sum(len(c['expectations']) for c in cases),
             'auto_rule_kinds_tested':sorted(used),'member_good':result['summary'],'member_bad':bad_result['summary'],
             'checks':'aliases, interspersed argv, gate normalization, CLI inventory, stdin deadline, warning correction, rosters, engine rendering, inline skill, held-out behaviors, host logs, public export, frozen hash gate'}
    (output/'selftest-summary.json').write_text(json.dumps(summary,ensure_ascii=False,indent=2)+'\n')
    print(json.dumps(summary,ensure_ascii=False,indent=2));print(output)


if __name__=='__main__':
    main()
