#!/usr/bin/env python3
"""Deterministic child process. No provider/API/network; only temporary test files."""
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time

case = os.environ.get('ORCH_FAKE_CASE', 'normal')

def event(kind, **fields):
    print(json.dumps({'type': kind, **fields}), flush=True)

record = os.environ.get('ORCH_FAKE_RECORD')
if record:
    with open(record, 'a') as file:
        file.write(json.dumps({'cwd': os.getcwd(), 'argv': sys.argv[1:]}) + '\n')

if case in ('timeout', 'cancel', 'closed-pipes'):
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    child = subprocess.Popen([
        sys.executable, '-c',
        'import signal,time; signal.signal(signal.SIGINT,signal.SIG_IGN); time.sleep(60)',
    ], stdout=subprocess.DEVNULL if case == 'closed-pipes' else None,
       stderr=subprocess.DEVNULL if case == 'closed-pipes' else None)
    if os.environ.get('ORCH_FAKE_PIDS'):
        Path(os.environ['ORCH_FAKE_PIDS']).write_text(json.dumps({'leader': os.getpid(), 'descendant': child.pid}))
    event('inspection', leader=os.getpid(), descendant=child.pid)
    if case != 'closed-pipes':
        time.sleep(60)

if case == 'flood':
    # More than a pipe buffer on both streams, with no newline in stderr.
    for _ in range(512):
        os.write(2, b'e' * 8192)
        event('unknown.future', payload='x' * 1024)
if case == 'oversized':
    os.write(1, b'x' * (2 * 1024 * 1024) + b'\n')
if case == 'invalid-json':
    os.write(1, b'not-json\n\xff\n')
if case == 'auth-failure':
    print('authentication required: run codex login', file=sys.stderr)
    sys.exit(1)

model = next((arg.removeprefix('--model=') for arg in sys.argv if arg.startswith('--model=')), None)
if model is not None:
    # Synthetic agy process, using sanitized real captures when supplied.
    if fixture := os.environ.get('ORCH_FAKE_AGY_FIXTURE'):
        for line in Path(fixture).read_text().splitlines():
            value = json.loads(line)
            if value['event'] == 'init' and value['init']['cwd'] == '/tmp/orch-agy-fixture/cwd':
                value['init']['cwd'] = os.getcwd()
            print(json.dumps(value), flush=True)
        sys.exit(int(os.environ.get('ORCH_FAKE_EXIT', '0')))

    def agy_event(kind, **fields):
        print(json.dumps({'event': kind, **fields}), flush=True)

    resuming = any(arg.startswith('--conversation=') for arg in sys.argv)
    conversation_id = 'fixture-agy'
    agy_event('init', conversation_id=conversation_id, init={
        'model': model, 'cwd': os.getcwd(), 'permission_mode': 'strict',
    })
    prompt = sys.argv[-1]
    marker = re.search(r'ORCH_SMOKE_\d+_\d+', prompt)
    state = Path('.fake-agy-context')
    if marker:
        state.write_text(marker.group())
    text = state.read_text() if resuming else 'OK'
    agy_event('step_update', step_update={
        'conversation_id': conversation_id, 'step_index': 1, 'step_type': 'agent_response',
        'state': 'DONE', 'text_delta': text,
    })
    agy_event('result', result={'conversation_id': conversation_id, 'status': 'SUCCESS', 'response': text})
    sys.exit(0)

resuming = 'resume' in sys.argv
thread = 'different-thread' if case == 'wrong-thread' and resuming else 'fixture-thread'
event('thread.started', thread_id=thread)
event('turn.started')
if case == 'provider-error':
    # Synthetic; the precise error schema was not captured from the real provider in 1A.
    event('turn.failed', error={'message': 'synthetic provider failure'})
    sys.exit(1)

prompt = sys.argv[-1]
marker = re.search(r'ORCH_SMOKE_\d+_\d+', prompt)
state = Path('.fake-codex-context')
if marker:
    state.write_text(marker.group())
text = state.read_text() if resuming and state.exists() else 'OK'
event('item.completed', item={'id': 'item_0', 'type': 'agent_message', 'text': text})
if case != 'incomplete':
    event('turn.completed', usage={'input_tokens': 1, 'output_tokens': 1})
if case == 'nonzero':
    print('synthetic exit failure', file=sys.stderr)
    sys.exit(7)
