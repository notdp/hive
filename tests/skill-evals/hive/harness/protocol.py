"""Shared stub/grader parsing; only models the protocol forms used in evals."""
import json
import re
from pathlib import Path

# cli/mod.rs::KNOWN_COMMANDS. This is an executable CLI inventory, not skill prose.
KNOWN_COMMANDS = set('fork join create delete spawn config inject compact team layout mirror workflow pr view attach ls send doctor capture interrupt kill cvim vim vfork hfork notify plugin codex claude grok ccd resume-hint shell-init update uninstall worktree'.split())
VALUE_FLAGS = {'--artifact', '--task', '--cli', '--model', '-m', '--team', '-t',
               '--base', '--cwd', '--workspace', '-w', '--skill', '--desc', '-d'}


def parse_args(argv):
    """Strip interspersed value options before interpreting positional arguments."""
    positionals, options = [], {}
    i = 1
    while i < len(argv):
        arg = argv[i]
        if arg == '--':
            positionals.extend(argv[i + 1:])
            break
        key, sep, value = arg.partition('=')
        if key in VALUE_FLAGS:
            if not sep:
                i += 1
                value = argv[i] if i < len(argv) else None
            options[key] = value
        elif arg in ('-h', '--help', '--force', '--plain'):
            options[arg] = True
        else:
            positionals.append(arg)
        i += 1
    return positionals, options


def parse_send(argv):
    pos, options = parse_args(argv)
    return {'address': pos[0] if pos else '', 'body': pos[1] if len(pos) > 1 else '',
            'artifact': options.get('--artifact'), 'options': options}


def command_key(argv):
    """The scripted lifecycle commands compare positions, not option placement."""
    if argv[:1] and argv[0] in ('create', 'join', 'spawn', 'kill'):
        return [argv[0], *parse_args(argv)[0]]
    return argv


def modelled_calls(calls):
    return [call for call in calls if not call.get('unmodelled') and not call.get('help')]


def successful_calls(calls):
    return [call for call in modelled_calls(calls) if call.get('exit_code', 0) == 0]


def fixture_path(run):
    # Control files live beside the executor run.
    run = Path(run).resolve()
    return run.parent / ('.' + run.name + '.control') / 'fixture.json'


def load_fixture(run):
    path = fixture_path(run)
    return json.loads(path.read_text())


def canonical_address(address, fixture):
    if address.startswith('ccd.'):
        label = address[4:]
        matches = [s for s in fixture.get('ccd', {}).get('sessions', [])
                   if label in {str(s.get(k, '')) for k in ('name', 'title', 'pid')}]
        if len(matches) == 1:
            return 'ccd.' + matches[0]['name']
        # An ambiguous title must not silently select a session.
        return address if not matches else 'ambiguous:' + address
    own_team = fixture.get('team', {}).get('name', '')
    return own_team + '.' + address if own_team and '.' not in address else address


def equivalent(address, expected, fixture):
    expected = expected if isinstance(expected, list) else [expected]
    actual = canonical_address(address, fixture)
    if actual.startswith('ambiguous:'):
        return False
    return any(actual == canonical_address(value, fixture) for value in expected)


def body_warning(body, command='send'):
    text = body.strip()
    if not text:
        return ''
    lines = re.split(r'\r\n|\r|\n', text)
    fenced = '```' in text
    markdown = any(line.lstrip().startswith(('# ', '- ', '* ')) for line in lines)
    if len(text) <= 500 and len(lines) < 3 and not fenced and not markdown:
        return ''
    details = [f'{len(text)} chars', f'{len(lines)} lines']
    if fenced:
        details.append('fenced code')
    if markdown:
        details.append('markdown')
    # message.rs::format_body_warning, plus eprintln's final newline.
    return (f"warning: body looks long or structured ({', '.join(details)}); consider stdin artifact:\n"
            f'  hive {command} <agent> "<short summary>" --artifact - <<\'EOF\'\n  ...\n  EOF\n')


def logical_sends(calls, fixture):
    """A warned send immediately corrected for the same target is one delivery."""
    sends = [call for call in successful_calls(calls) if call['argv'][:1] == ['send']]
    result = []
    for call in sends:
        current = parse_send(call['argv'])
        if result:
            prior = result[-1]
            previous = parse_send(prior['argv'])
            same_target = canonical_address(previous['address'], fixture) == canonical_address(current['address'], fixture)
            artifact = call.get('artifacts', {}).get('--artifact', {})
            corrected = (current['artifact'] is not None and artifact.get('exists') is True
                         and bool(artifact.get('text')) and call.get('exit_code', 0) == 0
                         and not call.get('body_warning'))
            if same_target and prior.get('body_warning') and prior.get('exit_code', 0) == 0 and corrected:
                result[-1] = call
                continue
        result.append(call)
    return result


def run_file(run, name):
    run = Path(run)
    control = fixture_path(run).parent / name
    return control if control.exists() else run / name
