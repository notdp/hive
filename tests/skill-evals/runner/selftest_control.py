#!/usr/bin/env python3
"""Synthetic control-layout and transcript-audit regression checks; no engines."""
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
from unittest.mock import patch

import claude_exec
import codex_exec
from control_access import scan_control_access, record_control_access
from exec_common import CONTROL_FILES, env_exports, probe_control_layout, run_file
from run_claude import executor_prompt, process


def main():
    with tempfile.TemporaryDirectory(prefix='runner-control-') as directory:
        run = Path(directory)/'run-1'
        control = run.with_name('.run-1.control')
        run.mkdir();control.mkdir()
        env = {'HIVE_EVAL_CONTROL': str(control), 'HIVE_EVAL_LOG': str(control/'hive-calls.jsonl'),
               'HIVE_EVAL_HOST_LOG': str(control/'host-calls.jsonl')}
        (run/'env.sh').write_text('\n'.join(f'export {k}={json.dumps(v)}' for k,v in env.items()))
        (run/'run.json').write_text('{}')
        for name in CONTROL_FILES:
            (control/name).write_text('control: '+name)
            assert run_file(run,name)==control/name
        assert env_exports(run/'env.sh')==env
        assert executor_prompt(run,env)==('control: executor-prompt.md',control/'executor-prompt.md')
        assert probe_control_layout(run,env)['ok']
        (run/'prompt.md').write_text('exposed')
        assert not probe_control_layout(run,env)['ok']
        (run/'prompt.md').unlink()
        assert claude_exec.mcp_config(run,env)['mcpServers']['host']['env']['HIVE_EVAL_HOST_LOG']==env['HIVE_EVAL_HOST_LOG']
        assert codex_exec.mcp_config(run,env)['host']['env']['HIVE_EVAL_HOST_LOG']==env['HIVE_EVAL_HOST_LOG']
        def transcript(tool,args):
            return '## [1] tool_use '+tool+' id=x\n\n'+claude_exec._fence(json.dumps(args),'json')+'\n'
        positives=[('Read',{'file_path':str(control/'hive-calls.jsonl')}),
                   ('Bash',{'command':'cat "$HIVE_EVAL_LOG"'}),
                   ('Bash',{'command':'ls -la "$HIVE_EVAL_CONTROL"'}),
                   ('Bash',{'command':f"python3 -c 'from pathlib import Path; Path({str(control)!r}).iterdir()'"}),
                   ('functions.exec_command',{'cmd':'cat host-calls.jsonl','workdir':str(control)}),
                   ('Bash',{'command':f'p={control}; cat "$p/host-calls.jsonl"'})]
        negatives=[('Write',{'file_path':str(control/'host-calls.jsonl'),'content':'x'}),
                   ('Bash',{'command':'hive send orch done'}),
                   ('Bash',{'command':'printf "%s" "cat $HIVE_EVAL_LOG"'}),
                   ('Read',{'file_path':str(run/'files/input.txt')})]
        for tool,args in positives:
            assert scan_control_access(run,transcript(tool,args)), (tool,args)
        for tool,args in negatives:
            assert not scan_control_access(run,transcript(tool,args)), (tool,args)
        # Quoted instructions and output headings cannot masquerade as tool calls.
        quoted='## [1] assistant\nDo not cat '+str(control)+'/prompt.md\n'
        quoted+='## [2] tool_result id=x\n'+claude_exec._fence(transcript('Read',{'file_path':str(control/'prompt.md')}))+'\n'
        assert not scan_control_access(run,quoted)
        shell='## [3] command_execution exit_code=0 status=completed\n\n'+claude_exec._fence('cat '+str(control/'prompt.md'),'bash')+'\n'
        assert scan_control_access(run,shell)
        (run/'transcript.md').write_text(shell)
        assert record_control_access(run)['peeked_control']
        assert json.loads((run/'run.json').read_text())['control_read_attempts'][0]['transcript_line']==1
        legacy=Path(directory)/'old-run';legacy.mkdir()
        for name in CONTROL_FILES:
            (legacy/name).write_text('legacy: '+name)
            assert run_file(legacy,name)==legacy/name
        assert executor_prompt(legacy,{})==('legacy: executor-prompt.md',legacy/'executor-prompt.md')
        assert probe_control_layout(legacy,{})['ok']
        resumed = Path(directory)/'iteration/eval-1/with_skill/run-1'
        resumed.mkdir(parents=True)
        hidden = resumed.with_name('.run-1.control');hidden.mkdir()
        (resumed/'env.sh').write_text(f'export HIVE_EVAL_CONTROL={hidden}\n')
        (resumed/'run.json').write_text(json.dumps({'executor':{'status':'ok'}}))
        (resumed/'raw.jsonl').write_text('')
        (resumed/'final_message.md').write_text('synthetic')
        (resumed/'grading.json').write_text(json.dumps({'status':'complete','summary':{}}))
        (resumed/'transcript.md').write_text(transcript('Read',{'file_path':str(hidden/'prompt.md')}))
        args=SimpleNamespace(iteration=Path(directory)/'iteration', configuration='with_skill', retry_failed=False,
                             grade_llm=True, eval_root=Path(directory), grader_model='synthetic', grader_md=Path(directory)/'grader.md',
                             grader_max_turns=1, grader_timeout_seconds=1)
        with patch('run_claude.log'), patch('run_claude.grade_llm.grade_run',return_value={'status':'complete','summary':{}}) as grader:
            result=process(({'id':1,'name':'synthetic'},1),args)
            assert grader.call_count==1 and result['peeked_control']
            process(({'id':1,'name':'synthetic'},1),args)
            assert grader.call_count==1  # same already-reviewed evidence does not regrade again
    print('PASS: control layout, legacy fallback, MCP env, explicit read audit and false-positive controls')


if __name__=='__main__':
    main()
