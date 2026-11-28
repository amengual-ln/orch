"""Synthetic providers for the PTY tests. Keep the filesystem state in each session cwd."""
import json
import os
from pathlib import Path
import sys
import time

args = sys.argv[1:]
agy = "--output-format" in args
model = next((arg.split("=", 1)[1] for arg in args if arg.startswith("--model=")), None)
prompt = args[-1].removeprefix("--print=") if agy else args[-1]
conversation = "tui-agy" if agy else "tui-codex"


def event(**payload):
    print(json.dumps(payload), flush=True)


with Path("requests").open("a") as record:
    record.write(json.dumps({"args": args, "prompt": prompt, "cwd": os.getcwd(), "pid": os.getpid()}) + "\n")

if agy:
    event(event="init", conversation_id=conversation,
          init={"model": model or "fixture-default", "cwd": os.getcwd(), "permission_mode": "strict"})
else:
    event(type="thread.started", thread_id=conversation)
    event(type="turn.started")

if prompt == "wait":
    deadline = time.monotonic() + 25
    while not Path("continue").exists() and time.monotonic() < deadline:
        time.sleep(.02)
elif prompt == "fail":
    if agy:
        event(event="result", result={"status": "ERROR", "error": "error de prueba"})
    else:
        event(type="error", message="error de prueba")
    sys.exit(1 if agy else 0)

text = "\n".join(f"LINE-{i:02}" for i in range(50)) if prompt == "long" else "RESPUESTA"
if agy:
    for delta in ("RESP", "UESTA"):
        event(event="step_update", step_update={
            "step_type": "agent_response", "step_index": 0, "text_delta": delta,
            "conversation_id": conversation,
        })
    event(event="result", result={"status": "SUCCESS", "response": text, "conversation_id": conversation})
else:
    event(type="item.completed", item={"type": "agent_message", "id": "m", "text": text})
    event(type="turn.completed")
